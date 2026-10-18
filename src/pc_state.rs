//! PC 电源状态回读 —— 对应 C 版 `main/pc_state.c`
//!
//! 电路（见 `main/pc_state.h` 的原始说明）：
//!   SATA 5V → 1k → 817D 光耦 pin1；pin2 → SATA GND；
//!   pin4 → GPIO6（带 10k 上拉）；pin3 → ESP32 GND。
//!
//! 逻辑：
//!   PC 关机 → 无 5V → LED 不亮 → 光敏管截止 → GPIO6 被上拉到高 → false
//!   PC 开机 → 有 5V → LED 亮   → 光敏管导通 → GPIO6 被拉低     → true
//!
//! 这个模块纯只读，不参与任何安全线 —— 它读错了顶多上报错状态，
//! 不会像 pulse 那样碰电源键。

use esp_idf_svc::hal::gpio::{Gpio6, Input, PinDriver, Pull};
use esp_idf_svc::sys::EspError;
use log::info;

use crate::consts::PIN_PC_STATE;

/// PC 状态输入句柄。
///
/// 持有 `PinDriver` 而不是每次读都重新配置：配置只在 init 做一次，
/// 之后 `is_on()` 是纯粹的寄存器读，不碰任何可能失败的东西 ——
/// 状态上报跑在 MQTT 回调/定时线程里，那里不适合做可能返回 Err 的操作。
pub struct PcState<'d> {
    pin: PinDriver<'d, Input>,
}

impl PcState<'_> {
    /// 低电平 = 光耦导通 = SATA 有 5V = PC 开着
    #[inline]
    pub fn is_on(&self) -> bool {
        self.pin.is_low()
    }
}

/// 配置 GPIO6 为输入并使能上拉。
///
/// 上拉是必须的：光耦截止时 GPIO6 悬空，没有上拉会读到随机值，
/// 表现为 PC 明明关着却报「开机」。
pub fn init(pin: Gpio6<'static>) -> Result<PcState<'static>, EspError> {
    let pin = PinDriver::input(pin, Pull::Up)?;
    info!("初始化完成，GPIO{PIN_PC_STATE} 作为状态回读输入");
    Ok(PcState { pin })
}
