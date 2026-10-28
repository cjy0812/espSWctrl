//! 电源键脉冲生成 —— 对应 C 版 `main/pulse.c`
//!
//! 安全线 2：脉冲在【独立线程】里生成，绝不在 MQTT 回调里直接拉 GPIO。
//! 回调一旦被阻塞，脉冲宽度就失控 —— 超过 4 秒会触发主板强制断电。
//! 本模块用队列接收请求，线程串行执行，天然限流。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::hal::gpio::{Gpio5, Output, PinDriver};
use esp_idf_svc::sys::EspError;
use log::{info, warn};

use crate::consts::{PIN_PWR_SW, PULSE_GAP_MIN_MS, PULSE_MS_DEFAULT, PULSE_MS_MAX, PULSE_MS_MIN};

/// 队列深度，对应 C 版 `xQueueCreate(4, ...)`
const QUEUE_DEPTH: usize = 4;
/// Rust 的线程比 C 那个裸 task 吃栈，留宽一点
const TASK_STACK: usize = 8192;

#[derive(Debug, PartialEq, Eq)]
pub enum PulseError {
    /// 队列满 —— 调用方应回复 busy
    QueueFull,
    /// 脉冲线程已消失（正常运行时不该发生）
    TaskGone,
}

/// 可克隆的脉冲请求句柄。持有一个 SyncSender，克隆不会多开线程。
#[derive(Clone)]
pub struct PulseHandle {
    tx: SyncSender<u16>,
    busy: Arc<AtomicBool>,
}

impl PulseHandle {
    /// 投递一个「按一下电源键」请求。**非阻塞**，立即返回。
    ///
    /// `width_ms == 0` 表示「用当前配置的默认宽度」。
    /// Phase 2 接上 cfg 模块后，这里改成读 NVS；现在先用编译期默认值。
    pub fn request(&self, width_ms: u16) -> Result<(), PulseError> {
        let width_ms = if width_ms == 0 {
            PULSE_MS_DEFAULT
        } else {
            width_ms
        };

        self.tx.try_send(width_ms).map_err(|e| match e {
            TrySendError::Full(_) => {
                warn!("脉冲队列已满，拒绝请求");
                PulseError::QueueFull
            }
            TrySendError::Disconnected(_) => PulseError::TaskGone,
        })
    }

    /// 查询当前是否正在执行脉冲（用于状态上报/拒绝并发请求）
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }
}

/// 初始化：配置 GPIO5 为输出并**立即置低**（上电默认安全态，越快越好）。
///
/// 这个函数必须在 main 的最前面调用 —— 在 Wi-Fi/MQTT 之前。
pub fn init(pin: Gpio5<'static>) -> Result<PulseHandle, EspError> {
    let mut pin = PinDriver::output(pin)?;

    // ---- 安全线 1：先钉死低电平，再谈别的 ----
    // 复位后 GPIO 是高阻（Hi-Z），光耦不导通，PC 本来就不受影响；
    // 这里显式置低是把它钉死，防止周围噪声感应出电压。
    pin.set_low()?;

    let (tx, rx) = mpsc::sync_channel::<u16>(QUEUE_DEPTH);
    let busy = Arc::new(AtomicBool::new(false));
    let busy_task = Arc::clone(&busy);

    std::thread::Builder::new()
        .name("pulse".into())
        .stack_size(TASK_STACK)
        .spawn(move || pulse_task(pin, rx, busy_task))
        // 线程建不起来 = 安全线 2 无法保证。这里必须硬失败（panic=abort），
        // 绝不降级成「在回调里直接拉 GPIO」。此时 GPIO5 已置低，停机是安全态。
        .expect("无法创建脉冲线程：没有它就无法保证脉冲独立于 MQTT 回调");

    info!("初始化完成，GPIO{PIN_PWR_SW} 已置低（安全态）");
    Ok(PulseHandle { tx, busy })
}

/// 脉冲线程主体：串行执行，独立于 MQTT 回调。
///
/// 关键：这里只做「拉高 → 精确延时 → 拉低」，中间不等待网络、不取锁。
/// 看门狗监控本线程 —— 一旦死锁，GPIO 卡高超过 4 秒会强制关机，
/// 所以 WDT 在本项目是安全相关项，不是可选项。
fn pulse_task(mut pin: PinDriver<'static, Output>, rx: mpsc::Receiver<u16>, busy: Arc<AtomicBool>) {
    while let Ok(mut width_ms) = rx.recv() {
        // 双保险：夹到安全区间。就算上游传了越界值也出不去。
        width_ms = width_ms.clamp(PULSE_MS_MIN, PULSE_MS_MAX);

        busy.store(true, Ordering::Relaxed);
        info!("按下电源键：脉冲 {width_ms} ms");

        // ---- 唯一操作 GPIO 的地方 ----
        if pin.set_high().is_ok() {
            std::thread::sleep(Duration::from_millis(width_ms as u64));
            let _ = pin.set_low();
        } else {
            // 拉高都失败，别让 pin 停在不确定状态
            let _ = pin.set_low();
        }
        // -----------------------------

        busy.store(false, Ordering::Relaxed);

        // 两次脉冲之间留足间隔，防被主板识别为长按/组合键
        std::thread::sleep(Duration::from_millis(PULSE_GAP_MIN_MS as u64));
    }
}
