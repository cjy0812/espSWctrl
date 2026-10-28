//! powerswitch —— ESP32-C3 远程电源键控制器
//!
//! Rust 重写版。Phase 2：在 Phase 1 的硬件骨架上接上
//! 串口控制台（`console.rs`）与 Wi-Fi/时间同步（`wifi.rs`）。
//!
//! 三条安全线（重写过程中一条都不能丢）：
//!
//! 1. 上电安全态 = 第一件事把 GPIO5 拉低；
//! 2. 脉冲在独立线程生成，绝不在 MQTT 回调里拉 GPIO；
//! 3. 固件侧对 topic 做防御性字节比对。
//!
//! 本阶段覆盖 1 和 2；3 在 Phase 4。
//!
//! # 启动顺序为什么是这个顺序
//!
//! ```text
//!   link_patches / logger
//!   Peripherals::take
//!   pulse::init(gpio5)      <- 安全线 1：越早钉死低电平越好
//!   pc_state::init(gpio6)
//!   sysloop / nvs
//!   Cfg::new                <- 控制台命令要读写 NVS，必须先就绪
//!   WifiManager::new        <- 只建驱动，不启动
//!   console::init           <- 注册命令，REPL 起来
//!   wifi.start（若 NVS 里有 SSID）
//! ```
//!
//! `pulse::init` 必须排在 `Cfg` / `Wifi` **之前** —— 安全态优先于一切。

// ---- lint 策略 ----
//
// 这里只放「现在就能满足、且真的能防住事故」的项。不放 `#![deny(warnings)]`：
// MQTT 接线（Phase 5）前有 36 处 MQTT 预留代码是传递性死代码，已逐点标注
// `allow(dead_code)`；全局 deny warnings 等 Phase 5 收尾后再开。
#![deny(unsafe_op_in_unsafe_fn)]

// 不启用 `unreachable_pub`：本 crate 是纯二进制，模块内的 `pub` 全部对外不可达，
// 开了会立刻产出 164 条无意义告警，把真正的问题淹掉。它只对 lib crate 有意义。
//
// 也不启用 `#![deny(warnings)]`：等 Phase 5 MQTT 接线收尾、预留代码的
// `allow(dead_code)` 全部撤掉之后，再在 CI 里开 `clippy -- -Dwarnings`。

mod cfg;
mod cmd_verify;
mod console;
mod consts;
mod ddns;
mod mqtt;
mod pc_state;
mod pulse;
mod wifi;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use crate::cfg::{Cfg, PulseMs};
use crate::console::{CmdError, ConsoleBackend};
use crate::consts::{PULSE_MS_DEFAULT, WIFI_TX_POWER_DBM};
use crate::pulse::{PulseError, PulseHandle};
use crate::wifi::{TimeSync, WifiManager};

// ---------------------------------------------------------------------------
//  控制台后端
// ---------------------------------------------------------------------------

/// `console.rs` 要求的宿主能力，全部转发给已有模块。
///
/// 这个结构体**本身不碰 GPIO** —— 所有「按电源键」都经 [`Backend::pulse_request`]
/// 走 `PulseHandle` 的队列，也就是和 MQTT 完全相同的那条路径。
/// 这样「控制台绝不做 GPIO 操作」这条安全线是结构性保证，而不是靠自觉。
struct Backend {
    cfg: Arc<Cfg>,
    pulse: PulseHandle,
    /// Wi-Fi 管理器的 `start` / `scan` 需要 `&mut`，所以加锁。
    /// 竞争很轻：REPL 命令偶发调用，主循环只读下面的 `net_up`。
    wifi: Mutex<WifiManager<'static>>,
    /// 免锁的「已拿到 IP」观察器，主循环 / DDNS 调度专用。
    ///
    /// 不能用 `wifi.try_lock()` 来问连接状态：SNTP 对时会阻塞最长 60 秒。
    net_up: crate::wifi::NetUpWatch,
    /// SNTP 对时器，**独立于** `wifi` 那把锁。
    ///
    /// 以前对时入口挂在 `WifiManager` 上（`WifiManager::sync`），调用方必须
    /// 先拿 Wi-Fi 互斥锁，再在锁内阻塞最长 60 秒（两台 NTP 各 15s 超时 +
    /// 中间 30s 换台）。那 60 秒里控制台的 `wifi_reconnect` / `txpower` /
    /// `scan` 全部卡在 `lock()` 上 —— 硬件上表现为「对时期间串口没反应」。
    ///
    /// SNTP 只需要「已经拿到 IP」这个事实，不需要 `&mut WifiManager`，
    /// 所以拆成两把锁：对时线程只在开头短暂拿 `wifi` 查一次连接状态，
    /// 之后在 `time_sync` 锁内做网络 I/O，两把锁互不相干。
    time_sync: Mutex<TimeSync>,
    /// MQTT 句柄。`None` 表示没配主机名或 client 创建失败。
    ///
    /// 与 C 版 `main.c:606` 一致：MQTT 起不来只记日志，不影响脉冲、
    /// 控制台和 DDNS —— 这台设备的核心职责是「按电源键」，网络是附加能力。
    mqtt: Option<crate::mqtt::MqttHandle>,
}

/// 把任意 `Display` 错误折成控制台能打印的 `CmdError::Other`。
fn other<E: std::fmt::Display>(e: E) -> CmdError {
    CmdError::Other(e.to_string())
}

/// 读 PC 开关机状态。`PcState` 持有 GPIO 驱动（非 `Sync`），所以要走锁。
///
/// 锁只覆盖一次 `is_low()` 读寄存器，极短；中毒时按「关机」处理并记日志 ——
/// 心跳只是观测用途，不值得因为一次读失败让主循环 panic。
fn pc_is_on(state: &Mutex<pc_state::PcState<'static>>) -> bool {
    match state.lock() {
        Ok(g) => g.is_on(),
        Err(e) => {
            log::error!("PC 状态互斥锁中毒: {e}");
            false
        }
    }
}

/// 「下一轮主循环立刻跑一次 DDNS」的请求标志。
///
/// 控制台回调只置这个位，真正的 HTTPS 永远发生在主循环里 ——
/// 在 REPL 回调里做网络 I/O 会卡住串口，还可能和主循环抢同一个连接。
static DDNS_NOW: AtomicBool = AtomicBool::new(false);

/// 「DDNS 没配记录名」这条提示是否已经打过。
///
/// 主循环 5 秒一轮，不加这个 once 会把日志刷爆。
static HOST_LOGGED: AtomicBool = AtomicBool::new(false);

/// 时间同步失败后隔多久再试。
///
/// 不能跟着 5 秒主循环周期重试：`WifiManager::sync()` 一次最长阻塞
/// 60 秒（两台 NTP 各 15s 超时 + 中间 30s 换台），失败即重试等于把
/// 主循环拖成几乎不转。5 分钟是「不空转、也不至于永远补不上」的折中。
const TIME_SYNC_RETRY_S: u64 = 300;

/// 当前 Unix 时间（秒）。时钟没设好时返回 0，不 panic。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 时间同步线程的栈。`sync()` 会走 lwIP 的 SNTP 客户端，
/// 给到和脉冲线程同级的 8KB 足够。
const TIME_SYNC_STACK: usize = 8192;

/// DDNS 遇到 `NextDelay::Stop`（配置错误，重试无用）后的冷却时间。
///
/// 不是「永不重试」：`Stop` 的典型原因是 token 打错 / 记录被删，
/// 用户在控制台改完配置后应该能在合理时间内自愈，而不必重启。
const DDNS_STOP_RETRY_S: u64 = 3600;

/// 等 Wi-Fi 连上后做一次 SNTP 对时；失败按 [`TIME_SYNC_RETRY_S`] 重试。
///
/// 独立线程的理由：`TimeSync::sync()` 最长阻塞约 60 秒（两台 NTP
/// 各 15s 超时 + 中间 30s 换台），超过 30s 任务看门狗的余量。放在
/// app_main 里等于拿看门狗赌运气。
///
/// **锁纪律**：Wi-Fi 锁只在开头拿一下查 `is_connected()`，随即 `drop`。
/// 真正的 SNTP 网络 I/O 全程只持有 `backend.time_sync` 那把锁。
/// 以前 `sync()` 挂在 `WifiManager` 上，那 60 秒里控制台的
/// `wifi_reconnect` / `txpower` / `scan` 全部卡在 `lock()` 上，
/// 硬件上表现为「对时期间串口没反应」。拆锁后这两个操作互不相干。
///
/// 失败**只 warn 不 panic**：对时失败的直接后果是 DDNS 的 TLS 校验证书
/// 有效期会失败，但心跳、脉冲、控制台都还能用，不该整机重启。
fn time_sync_loop(backend: &'static Backend, time_synced: Arc<AtomicBool>) {
    loop {
        // ---- 短暂持 Wi-Fi 锁：只问「连上了吗」 ----
        let connected = match backend.wifi.lock() {
            Ok(wifi) => wifi.is_connected().unwrap_or(false),
            Err(e) => {
                log::error!("Wi-Fi 互斥锁中毒: {e}");
                return;
            }
        };

        if !connected {
            // 还没连上就短周期轮询 —— 用 5 分钟的重试间隔等首次连接太浪费。
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        // ---- 锁外对时：只持 `time_sync`，不碰 Wi-Fi 锁 ----
        let mut time_sync = match backend.time_sync.lock() {
            Ok(g) => g,
            Err(e) => {
                log::error!("SNTP 互斥锁中毒: {e}");
                return;
            }
        };

        match time_sync.sync() {
            Ok(()) => {
                // 必须放在 `return` 之前（线程到此结束，之后再无机会写）；
                // 也必须放在 `log::info!` 之前（日志可能被级别过滤掉）。
                // 只置 true，永不置回 false —— 否则设备会突然开始拒命令。
                time_synced.store(true, Ordering::SeqCst);
                log::info!("SNTP 对时完成，当前时间戳 {}", unix_now());
                return;
            }
            Err(e) => {
                // 先显式 drop 再睡：睡 5 分钟期间不该占着 SNTP 单例槽位。
                drop(time_sync);
                log::warn!("SNTP 对时失败: {e}，{}s 后重试", TIME_SYNC_RETRY_S);
            }
        }

        std::thread::sleep(Duration::from_secs(TIME_SYNC_RETRY_S));
    }
}

/// 主循环里的一次 DDNS 检查。
///
/// 绝大多数轮次只读几个 NVS 值就返回；只有「该发了」的那一轮才真正
/// 走 HTTPS。HTTPS 的超时（`DDNS_HTTP_TIMEOUT_S`）远小于 30s 看门狗，
/// 所以这一次阻塞是安全的。
///
/// 锁纪律：host / token 全部从 `cfg` 读，**不碰 Wi-Fi 互斥锁** ——
/// 网络 I/O 期间持有 Wi-Fi 锁会让心跳和 `wifi scan` 一起卡死。
fn ddns_tick(cfg: &Cfg, next_due: &mut Instant) {
    // 无论后面走不走，先把这个「立刻更新」请求消费掉：
    // 否则关掉 DDNS 期间敲的 `ddns now` 会在重新打开时突然补发。
    let forced = DDNS_NOW.swap(false, Ordering::SeqCst);

    if !cfg.ddns_enabled().unwrap_or(false) {
        if forced {
            log::warn!("DDNS 未启用，忽略 `ddns now`");
        }
        return;
    }

    // token 只在这个局部变量里存在，不进任何日志。
    let token = cfg.ddns_token().unwrap_or_default();
    if token.is_empty() {
        if forced {
            log::warn!("DDNS 缺少 token，忽略 `ddns now`");
        }
        return;
    }

    // host 只是给人看的标签：请求里从不发送它，token 已经绑定了记录。
    // 所以没配记录名不该挡住更新，只提示一次（主循环 5 秒一轮，不加 once 会刷爆日志）。
    let host = cfg.ddns_host().unwrap_or_default();
    if host.is_empty() && !HOST_LOGGED.swap(true, Ordering::Relaxed) {
        log::info!("DDNS 未配置记录名，仍按 token 绑定的记录推送（host 仅用于日志）");
    }
    // 只影响日志文案：hostname 仅用于 `DDNS 已更新：{}`，请求里不会发送。
    let host_label = if host.is_empty() {
        "(未命名)".to_string()
    } else {
        host
    };

    // 时钟没对上时 TLS 必然因证书有效期失败，没必要发一次注定失败的请求。
    if unix_now() == 0 {
        return;
    }

    let now = Instant::now();
    if !forced && now < *next_due {
        return;
    }

    let interval_s = cfg
        .ddns_interval_s()
        .unwrap_or(consts::DDNS_INTERVAL_DEFAULT_S) as u64;

    // 每轮重建 client：provider 持有 token 的副本，短生命周期能少一份常驻内存。
    let client = ddns::DdnsClient::new(ddns::DnsheProvider::new(token, host_label), interval_s);

    // 只做 blind v4：服务端按请求来源 IP 绑定记录，本机不查外网 IP。
    // **不做 AAAA** —— IPv6 在这块板子上没验证过。
    let outcome = client.run_once(&ddns::DdnsRecord::blind_v4(), None);

    match &outcome {
        Ok(o) => {
            // Updated 和 NoChange 都算成功：NoChange 说明记录本来就是对的。
            let ts = unix_now();
            if let Err(e) = cfg.set_ddns_last_ok(ts) {
                log::warn!("DDNS 成功但写 last_ok 失败: {e}");
            }
            if o.is_change() {
                log::info!("DDNS 已更新：{}", client.provider().hostname());
            } else {
                // 每轮都打会淹掉日志，NoChange 只走 debug。
                log::debug!("DDNS 无变化（记录已是当前 IP）");
            }
        }
        // `DdnsError` 的 Display 已经做过脱敏（token 不在错误文本里）。
        Err(e) => log::warn!("DDNS 更新失败: {e}"),
    }

    let delay = client.next_delay(&outcome);
    *next_due = match delay {
        ddns::NextDelay::Scheduled(s) | ddns::NextDelay::Retry(s) => {
            Instant::now() + Duration::from_secs(s)
        }
        ddns::NextDelay::Stop => {
            log::error!(
                "DDNS 已停止重试（配置错误，需人工介入），{}s 后再试一次",
                DDNS_STOP_RETRY_S
            );
            Instant::now() + Duration::from_secs(DDNS_STOP_RETRY_S)
        }
    };
}

impl ConsoleBackend for Backend {
    // ---- 只读查询 ----

    fn cfg_dump(&self) {
        if let Err(e) = self.cfg.dump() {
            log::error!("读取配置失败: {e}");
        }
    }

    fn tx_power_dbm(&self) -> i32 {
        self.cfg.tx_power_dbm().unwrap_or(WIFI_TX_POWER_DBM) as i32
    }

    fn pulse_ms(&self) -> u16 {
        self.cfg
            .pulse_ms()
            .map(PulseMs::get)
            .unwrap_or(PULSE_MS_DEFAULT)
    }

    fn cmd_ttl(&self) -> u16 {
        self.cfg.cmd_ttl().unwrap_or(consts::CMD_TTL_DEFAULT as u16)
    }

    fn status_period_s(&self) -> u16 {
        self.cfg
            .status_period_s()
            .unwrap_or(consts::STATUS_PUBLISH_S as u16)
    }

    fn keepalive_s(&self) -> u16 {
        self.cfg
            .keepalive_s()
            .unwrap_or(consts::MQTT_KEEPALIVE_S as u16)
    }

    // ---- MQTT 只读查询 ----
    //
    // 同 DDNS：NVS 读失败一律退回「未配置」，让 `mq show` 仍能打出其余字段。

    fn mqtt_host(&self) -> String {
        self.cfg.mqtt_host().unwrap_or_default()
    }

    fn mqtt_user(&self) -> String {
        self.cfg.mqtt_user().unwrap_or_default()
    }

    fn mqtt_pass_masked(&self) -> String {
        // **只打掩码，绝不进日志**：明文在这里被 mask_token 截断，
        // 本函数是控制台能拿到密码的唯一通路，返回值已不含完整凭据。
        let pass = self.cfg.mqtt_pass().unwrap_or_default();
        if pass.is_empty() {
            "(未设置)".to_string()
        } else {
            crate::ddns::mask_token(&pass)
        }
    }

    fn mqtt_enabled(&self) -> bool {
        self.cfg.mqtt_enabled().unwrap_or(false)
    }

    fn mqtt_online(&self) -> Option<bool> {
        // 句柄不存在 = 客户端根本没起（未启用 / 建连失败），这与
        // 「起了但当前断开」是两种不同的排障方向，必须区分开。
        self.mqtt.as_ref().map(|h| h.is_online())
    }

    fn mqtt_refresh_state(&self) -> bool {
        self.mqtt.as_ref().is_some_and(|h| h.request_state())
    }

    fn dev_id(&self) -> String {
        // 读失败退回编译期默认值，而不是空串 —— 空串会拼出 `psw//cmd` 这种坏 topic。
        self.cfg
            .dev_id()
            .unwrap_or_else(|_| consts::DEFAULT_DEV_ID.to_string())
    }

    // ---- DDNS 只读查询 ----
    //
    // 读取失败一律退回「未配置」而不是报错：控制台 `ddns show` 的价值是
    // 看现状，NVS 读失败时打一行空值比整条命令失败更好排查。

    fn ddns_host(&self) -> String {
        self.cfg.ddns_host().unwrap_or_default()
    }

    fn ddns_token_masked(&self) -> String {
        // **只打掩码，绝不进日志**：明文 token 在这里就被 mask_token 截断，
        // 本函数是控制台能拿到 token 的唯一通路，返回值已经不含完整凭据。
        let token = self.cfg.ddns_token().unwrap_or_default();
        if token.is_empty() {
            "(未设置)".to_string()
        } else {
            crate::ddns::mask_token(&token)
        }
    }

    fn ddns_enabled(&self) -> bool {
        self.cfg.ddns_enabled().unwrap_or(false)
    }

    fn ddns_interval_s(&self) -> u32 {
        self.cfg
            .ddns_interval_s()
            .unwrap_or(consts::DDNS_INTERVAL_DEFAULT_S)
    }

    fn ddns_last_ok(&self) -> u64 {
        self.cfg.ddns_last_ok().unwrap_or(0)
    }

    fn ddns_trigger_now(&self) {
        // 只置标志位：真正的 HTTPS 在主循环里发。REPL 线程阻塞十几秒
        // 会把串口命令通道整个卡死。
        DDNS_NOW.store(true, Ordering::SeqCst);
    }

    // ---- 写入 ----

    fn set_wifi(&self, ssid: &str, pass: &str) -> Result<(), CmdError> {
        self.cfg.set_wifi(ssid, pass).map_err(other)
    }

    fn set_tx_power_dbm(&self, dbm: i8) -> Result<(), CmdError> {
        self.cfg.set_tx_power_dbm(dbm).map_err(other)
    }

    fn set_pulse_ms(&self, ms: u16) -> Result<(), CmdError> {
        // 先过 PulseMs 校验（它会夹到安全区间并拒绝越界），再落 NVS。
        let ms = PulseMs::new(ms).map_err(other)?;
        self.cfg.set_pulse_ms(ms).map_err(other)
    }

    fn set_cmd_ttl(&self, s: u16) -> Result<(), CmdError> {
        self.cfg.set_cmd_ttl(s).map_err(other)
    }

    fn set_status_period_s(&self, s: u16) -> Result<(), CmdError> {
        self.cfg.set_status_period_s(s).map_err(other)
    }

    fn set_keepalive_s(&self, s: u16) -> Result<(), CmdError> {
        self.cfg.set_keepalive_s(s).map_err(other)
    }

    // ---- MQTT 写入 ----
    //
    // 只落 NVS。MQTT 客户端尚不存在（Phase 5），所以没有任何「生效」信号：
    // 建连时才会重新读这些键，改完必须 reboot。

    fn set_mqtt_host(&self, host: &str) -> Result<(), CmdError> {
        self.cfg.set_mqtt_host(host).map_err(other)
    }

    fn set_mqtt_user(&self, user: &str) -> Result<(), CmdError> {
        self.cfg.set_mqtt_user(user).map_err(other)
    }

    fn set_mqtt_pass(&self, pass: &str) -> Result<(), CmdError> {
        // **不打印任何东西**：掩码由 console 侧统一打，这里多打一次就多一个泄漏点。
        self.cfg.set_mqtt_pass(pass).map_err(other)
    }

    fn set_mqtt_enabled(&self, en: bool) -> Result<(), CmdError> {
        self.cfg.set_mqtt_enabled(en).map_err(other)
    }

    fn set_dev_id(&self, id: &str) -> Result<(), CmdError> {
        self.cfg.set_dev_id(id).map_err(other)
    }

    // ---- DDNS 写入 ----
    //
    // 这里只负责落 NVS。真正的重排期由主循环在下一轮 tick 里从 cfg 重读，
    // 所以改完配置后不需要给任何「生效」信号 —— 下一轮自然按新值走。

    fn set_ddns_host(&self, host: &str) -> Result<(), CmdError> {
        self.cfg.set_ddns_host(host).map_err(other)
    }

    fn set_ddns_token(&self, token: &str) -> Result<(), CmdError> {
        // **只打掩码，绝不进日志**：本函数不打印任何东西，
        // 也不把入参回显给调用方；调用方（console）同样只打 mask_token。
        self.cfg.set_ddns_token(token).map_err(other)
    }

    fn set_ddns_enabled(&self, en: bool) -> Result<(), CmdError> {
        self.cfg.set_ddns_enabled(en).map_err(other)
    }

    fn set_ddns_interval_s(&self, s: u32) -> Result<(), CmdError> {
        self.cfg.set_ddns_interval_s(s).map_err(other)
    }

    // ---- 立即生效的副作用 ----

    fn wifi_reconnect(&self) {
        let ssid = self.cfg.wifi_ssid().unwrap_or_default();
        let pass = self.cfg.wifi_pass().unwrap_or_default();

        if ssid.is_empty() {
            log::error!("Wi-Fi 未配置（NVS 里没有 ssid），拒绝重连（fail-closed）");
            return;
        }

        let mut wifi = match self.wifi.lock() {
            Ok(g) => g,
            Err(e) => {
                log::error!("Wi-Fi 互斥锁中毒: {e}");
                return;
            }
        };
        if let Err(e) = wifi.start(&ssid, &pass) {
            log::error!("Wi-Fi 重连失败: {e}");
        }
    }

    fn wifi_set_tx_power_now(&self, dbm: i8) {
        let wifi = match self.wifi.lock() {
            Ok(g) => g,
            Err(e) => {
                log::error!("Wi-Fi 互斥锁中毒: {e}");
                return;
            }
        };
        if let Err(e) = wifi.set_tx_power(dbm) {
            log::error!("设置发射功率失败: {e}");
        }
    }

    fn wifi_scan_now(&self) {
        let mut wifi = match self.wifi.lock() {
            Ok(g) => g,
            Err(e) => {
                log::error!("Wi-Fi 互斥锁中毒: {e}");
                return;
            }
        };
        match wifi.scan() {
            Ok(aps) => {
                log::info!("扫描到 {} 个 AP:", aps.len());
                for ap in &aps {
                    log::info!(
                        "  SSID=\"{}\" ch={} rssi={} auth={:?}",
                        ap.ssid,
                        ap.channel,
                        ap.rssi,
                        ap.auth
                    );
                }
            }
            Err(e) => log::error!("扫描失败: {e}"),
        }
    }

    fn cfg_reset(&self) -> Result<(), CmdError> {
        self.cfg.reset().map_err(other)
    }

    // ---- 安全线 ----

    fn pulse_request(&self, width_ms: u16) -> Result<(), CmdError> {
        self.pulse.request(width_ms).map_err(|e| match e {
            // 队列满在 C 版里就是「忙：上一次脉冲还没结束」，不是设备错误。
            PulseError::QueueFull => CmdError::Busy,
            PulseError::TaskGone => CmdError::Other("脉冲线程已消失".into()),
        })
    }
}

// ---------------------------------------------------------------------------
//  入口
// ---------------------------------------------------------------------------

fn main() {
    // 这两行是 esp-idf-svc 应用的固定入口仪式。
    // link_patches 把 Rust 的 panic/alloc 钩子接到 IDF 上；
    // EspLogger 把 `log` crate 的输出导到 ESP_LOGI 那一套。
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().expect("Peripherals::take 只能调用一次");

    // ---- 安全线 1：第一件事就是钉死 GPIO5 ----
    // 注意顺序：pulse::init 内部立刻 set_low()，而它被放在
    // pc_state::init 以及所有网络初始化之前 —— 「安全态优先」这个
    // 顺序本身是设计的一部分，别调换。
    let pins = peripherals.pins;
    let modem = peripherals.modem;

    let pulse = pulse::init(pins.gpio5).expect("pulse 初始化失败");
    // 包成 `Arc<Mutex<>>`：MQTT 线程与主循环都要读它，而 `PcState`
    // 内部持有 GPIO 驱动（非 Sync）。锁内只做一次 `is_low()`，极短。
    let pc_state = Arc::new(Mutex::new(
        pc_state::init(pins.gpio6).expect("pc_state 初始化失败"),
    ));

    // ---- 事件循环 / NVS ----
    let sysloop = EspSystemEventLoop::take().expect("事件循环初始化失败");
    let nvs = EspDefaultNvsPartition::take().expect("NVS 分区初始化失败");

    // ---- 配置模块 ----
    // 必须早于 console::init：命令处理器会立刻读写 NVS。
    let cfg = Arc::new(Cfg::new(nvs.clone()).expect("配置模块初始化失败"));

    // ---- Wi-Fi 驱动 ----
    // 只建驱动、装事件回调，**不**启动、**不**连接。
    // 真正的连接动作在下面（若 NVS 里已有 SSID）或控制台 `wifi` 命令里发起。
    let wifi = WifiManager::new(modem, sysloop, nvs).expect("Wi-Fi 驱动初始化失败");

    // ---- 控制台 ----
    // 观察器必须在 `wifi` 被移进 `Mutex` **之前**取，之后就拿不到
    // `&WifiManager` 了（那样又得去抢锁，等于没解决）。
    let net_up = wifi.net_up_watch();

    // SNTP 成功 -> true，只写一次；MQTT 线程将来轮询读（docs/mqtt-port.md §6.2）。
    // 刻意**不**放进 `Backend`：`time_sync_loop` 拿的是 `&'static Backend`，
    // 从那里读标志等于让 SNTP 线程依赖自己的输出。
    let time_synced: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // ---- MQTT ----
    // 必须在 `Box::leak` 之前起：`Backend` 是 `&'static`，泄漏后就不能再赋值。
    // 失败不致命（C 版 `main.c:606` 语义）：这台设备的核心职责是「按电源键」，
    // 网络是附加能力，MQTT 起不来不影响脉冲、控制台和 DDNS。
    let mqtt = if cfg.mqtt_enabled().unwrap_or(false) {
        match mqtt::spawn(
            Arc::clone(&cfg),
            pulse.clone(),
            Arc::clone(&pc_state),
            Arc::clone(&time_synced),
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("MQTT 启动失败: {e}（其余功能不受影响）");
                None
            }
        }
    } else {
        log::info!("MQTT 未启用 —— 敲 `mq enable on` 后重启生效");
        None
    };

    // 后端必须活到进程结束，所以 Box::leak 成 'static。
    let backend: &'static Backend = Box::leak(Box::new(Backend {
        cfg: Arc::clone(&cfg),
        pulse,
        wifi: Mutex::new(wifi),
        net_up,
        // SNTP 实例单独一把锁：对时期间只锁这里，Wi-Fi 锁保持可抢。
        time_sync: Mutex::new(TimeSync::new()),
        mqtt,
    }));

    // 控制台起不来不致命（C 版语义）：只打日志，其余功能照跑。
    if let Err(e) = console::init(backend) {
        log::error!("串口控制台不可用: {e}（其余功能不受影响）");
    }

    // ---- 若已配置 Wi-Fi，则连上 ----
    let ssid = cfg.wifi_ssid().unwrap_or_default();
    if ssid.is_empty() {
        log::warn!("Wi-Fi 尚未配置 —— 在 COM11 上敲 `wifi <ssid> <pass>` 配一次");
    } else {
        backend.wifi_reconnect();
    }

    // ---- SNTP 对时线程 ----
    // `sync()` 最长阻塞 60s，不能放在 app_main 里赌 30s 看门狗，
    // 也不能放进 5s 主循环 —— 所以给它一条独立线程，只跑一次。
    std::thread::Builder::new()
        .name("sntp".into())
        .stack_size(TIME_SYNC_STACK)
        .spawn(move || time_sync_loop(backend, time_synced))
        .expect("SNTP 线程创建失败");

    log::info!(
        "Phase 2 就绪：安全态已建立，PC 当前状态 = {}",
        if pc_is_on(&pc_state) {
            "开机"
        } else {
            "关机"
        }
    );

    // ---- 主循环 ----
    //
    // 刻意**不**在这里阻塞等 IP（`wait_netif_up`）：本项目开了 30s 任务看门狗
    // （trigger_panic），在 app_main 里长时间阻塞会把芯片喂狗这件事变成
    // 一个必须时刻记着的隐性约束。改成周期性非阻塞轮询：
    //   * 连接是否建立由 `wifi.rs` 的事件回调负责（退避重连都在那里）；
    //   * 主循环只读状态，用于串口心跳观测。
    //
    // 心跳周期 5s：Phase 2 仍然没有网络业务，接串口看到心跳行
    // = 固件在跑；一片空白 = 芯片卡在下载模式。
    // DDNS 下次到期时刻。初值 = 现在，即上电后第一轮就连一次 ——
    // 路由器重启后 IP 可能已经变了，尽早纠正比等一个周期好。
    let mut next_due = Instant::now();

    loop {
        std::thread::sleep(Duration::from_secs(5));

        // 免锁读取，**直查 netif**：
        //   * 不能用 `wifi.try_lock()`：抢不到锁时会读成「未连接」；
        //   * 不能只信 `IpEvent` 订阅：esp-idf-svc 0.53 + IDF 6.1 下
        //     `IpEvent::DhcpIpAssigned` 实测不触发（`unknown event ID 9`），
        //     只靠事件的话这里恒 false，`ddns_tick` 一次都跑不到（硬件实测过）。
        // 所以 `is_up_live()` 先看事件标志，未置位就向 netif 查一次真实 IP。
        let connected = backend.net_up.is_up_live();

        // 只在 Wi-Fi 已连接时才碰 DDNS：没网时发请求只会拿一个
        // 连接超时，白等 `DDNS_HTTP_TIMEOUT_S`，还把下次重试推后。
        // 注意这里不持 Wi-Fi 锁 —— `ddns_tick` 只用 `cfg`。
        if connected {
            ddns_tick(&cfg, &mut next_due);
        }

        log::info!(
            "心跳：PC = {}，pulse busy = {}，Wi-Fi = {}",
            if pc_is_on(&pc_state) {
                "开机"
            } else {
                "关机"
            },
            backend.pulse.is_busy(),
            if connected { "已连接" } else { "未连接" }
        );
    }
}
