//! Wi-Fi STA 管理 —— 从 C 版 `main/main.c` 移植的三处修复。
//!
//! 这个模块只负责「连上、别把自己饿死、按正确的安全参数去连」以及
//! 「拿到 IP 之后把时间同步起来」。业务逻辑（MQTT、命令、脉冲）不在这里。
//!
//! # 从 C 版移植的三处修复
//!
//! 1. **重连退避**（main.c 64–78 声明 / 171–199 应用 / 204–211 清零）
//!    断线后无条件立刻 `esp_wifi_connect()` 会让扫描饿死：驱动里
//!    「连接」优先级高于「扫描」，自动重连一开，`scan` 命令永远抢不到空口
//!    （官方 Station Scenarios 原话：*avoid the scan starve to death*）。
//!    对策：前 `RECONNECT_IMMEDIATE` 次给 `RECONNECT_FAST_MS`，
//!    之后给 `RECONNECT_SLOW_MS`。计数在**拿到 IP** 时清零 ——
//!    用 GOT_IP 而不是 STA_CONNECTED：关联上但 DHCP 拿不到 IP 不算连上。
//!    计数不清零的后果：失败 6 次后计数器永远停在 6，退避变成永久惩罚。
//!
//! 2. **扫描饥饿**（main.c 62 标志 / 166–169 第一道闸 / 189–198 第二道闸 /
//!    221–300 扫描主体 / 336–352 强制扫描）
//!    两道闸都必须有：第一道在**入睡前**，第二道在**醒来后**。
//!    如果这段退避睡眠里用户敲了 `scan`，只查一次是挡不住的 ——
//!    醒来时会正好在扫描进行中调用 `esp_wifi_connect()`，把扫描打断
//!    （返回 `ESP_ERR_WIFI_STATE`）。这正是「scan 永远扫到 0 个 AP」的成因之一。
//!    主动扫描参数照抄 C 版：`show_hidden = true`、active min 100 / max 300 ms。
//!    一次扫描最多试 `SCAN_ATTEMPTS` 次；`ESP_ERR_WIFI_STATE` 说明被连接打断，
//!    等 `SCAN_RETRY_DELAY_MS` 再来；返回 OK 但 0 个 AP 也重试一次。
//!
//! 3. **PMF / 安全协商**（main.c 89–113 `wifi_fill_config()`）
//!    WPA2/WPA3 过渡模式的 AP 只有一种正确组合：`capable = true, required = false`。
//!    - `capable = false` → 不广播 PMF 能力，过渡模式 AP 直接拒，reason = 210
//!      （"找到 AP，但加密方式不兼容"）。
//!    - `required = true` → 纯 WPA2 的 AP 连不上（对端不支持 PMF）。
//!
//!    同时认证阈值取**更强**的那一档：`WPA2WPA3Personal`
//!    （在 esp-idf-svc 里映射到 `WIFI_AUTH_WPA2_WPA3_PSK`），
//!    它落在驱动 `threshold.authmode` 上。有密码才设，开放网络用 `None`。
//!
//! # 与 C 版的差异
//!
//! - C 版把「关闭 modem sleep」写成 `esp_wifi_set_ps(WIFI_PS_NONE)`（数值 0）。
//!   esp-idf-svc 没有包装这个函数，这里直接走 `sys`。
//! - C 版把发射功率写在 `wifi_apply_tx_power()` 里，单位是 0.25 dBm（乘 4）。
//!   esp-idf-svc 同样没有包装 `esp_wifi_set_max_tx_power`，也走 `sys`。
//! - C 版的 NTP 定义了常量却写死 `pool.ntp.org`；这里**真的**用
//!   `consts::SNTP_SERVER_1` / `SNTP_SERVER_2`，并且因为本项目
//!   `CONFIG_LWIP_SNTP_MAX_SERVERS=1`（lwIP 只有一个服务器槽位），
//!   两个常量是**主/备故障切换**的关系，不是同时塞进 `SntpConf`。
//! - C 版用 FreeRTOS event group 广播 `BIT_WIFI_UP`；这里换成
//!   `AtomicBool`，由 `IpEvent::DhcpIpAssigned`（IP_EVENT 基的 GOT_IP）
//!   回调置位 —— 不依赖任何人去调 `wait_netif_up()`，语义相同。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::eventloop::{EspSubscription, EspSystemEventLoop, System};
use esp_idf_svc::hal::modem::WifiModemPeripheral;
use esp_idf_svc::netif::IpEvent;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::{EspSntp, OperatingMode, SntpConf, SyncMode, SyncStatus};
use esp_idf_svc::sys;
use esp_idf_svc::wifi::config::{ScanConfig, ScanType};
use esp_idf_svc::wifi::{
    AccessPointInfo, AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi,
    PmfConfiguration, ScanMethod, ScanSortMethod, WifiEvent,
};

use crate::consts::{
    SNTP_RETRY_FAST_S, SNTP_RETRY_SLOW_S, SNTP_SERVER_1, SNTP_SERVER_2, SNTP_SYNC_TIMEOUT_S,
    WIFI_TX_POWER_DBM,
};

// ---------------------------------------------------------------------------
//  常量
// ---------------------------------------------------------------------------

/// 前几次断线走快速重连。对应 C 版 `RECONNECT_IMMEDIATE`（main.c:65）。
///
/// 注意：`consts::RECONNECT_BASE_MS` / `RECONNECT_MAX_MS` 是 **MQTT** 的退避，
/// 不是 Wi-Fi 的。别混用 —— 数值（1000 / 60000）和这里（500 / 5000）完全不同。
const RECONNECT_IMMEDIATE: u32 = 5;

/// 快速重连间隔。对应 C 版 `RECONNECT_FAST_MS`（main.c:66）。
const RECONNECT_FAST_MS: u32 = 500;

/// 慢速重连间隔。对应 C 版 `RECONNECT_SLOW_MS`（main.c:67）。
const RECONNECT_SLOW_MS: u32 = 5_000;

/// 一次 `scan` 命令最多试几次。对应 C 版循环里的 `3`（main.c:245 附近）。
const SCAN_ATTEMPTS: u32 = 3;

/// 两次扫描尝试之间的间隔。对应 C 版 `vTaskDelay(500ms)`。
const SCAN_RETRY_DELAY_MS: u32 = 500;

/// 强制扫描前等待断开生效的时间。对应 C 版 `wifi_scan_now()` 里的 400ms。
const SCAN_DISCONNECT_SETTLE_MS: u32 = 400;

/// 单次扫描最多返回多少个 AP。对应 C 版把 `n` 截到 20。
const MAX_SCAN_APS: usize = 20;

/// 主动扫描每信道的 min/max 驻留时间（ms）。对应 C 版 `active.min/max`。
const SCAN_ACTIVE_MIN_MS: u64 = 100;
const SCAN_ACTIVE_MAX_MS: u64 = 300;

// ---------------------------------------------------------------------------
//  错误类型
// ---------------------------------------------------------------------------

/// Wi-Fi 模块的错误。fail-closed：任何一步失败都往上抛，绝不「差不多算了」。
#[derive(Debug)]
pub enum WifiError {
    /// 底层 esp-idf 错误。
    Esp(sys::EspError),
    /// SNTP 在给定时间内没同步上。**调用方必须把它当失败处理** ——
    /// 时间不可信就意味着命令的 ts/ttl 窗口不可信，不能放行。
    TimeSyncTimeout { server: &'static str, secs: u32 },
    /// 主备 NTP 服务器都试过了还是没同步。
    TimeSyncExhausted,
    /// 调用的前置状态不满足（例如还没 `start()` 就 `scan()`）。
    NotStarted,
}

impl std::fmt::Display for WifiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WifiError::Esp(e) => write!(f, "esp-idf 错误: {e}"),
            WifiError::TimeSyncTimeout { server, secs } => {
                write!(f, "NTP 同步超时：服务器 {server}，{secs}s 内未完成")
            }
            WifiError::TimeSyncExhausted => write!(f, "NTP 主备服务器都同步失败"),
            WifiError::NotStarted => write!(f, "Wi-Fi 尚未 start()"),
        }
    }
}

impl std::error::Error for WifiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WifiError::Esp(e) => Some(e),
            _ => None,
        }
    }
}

impl From<sys::EspError> for WifiError {
    fn from(e: sys::EspError) -> Self {
        WifiError::Esp(e)
    }
}

/// 本模块统一的结果类型。
pub type WifiResult<T> = Result<T, WifiError>;

// ---------------------------------------------------------------------------
//  扫描结果
// ---------------------------------------------------------------------------

/// 一条扫描结果。只保留串口日志/自检需要的那几个字段，避免把
/// `AccessPointInfo` 整个搬来搬去。
#[derive(Debug, Clone)]
pub struct ScanAp {
    pub ssid: String,
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i8,
    pub auth: Option<AuthMethod>,
}

impl From<&AccessPointInfo> for ScanAp {
    fn from(ap: &AccessPointInfo) -> Self {
        Self {
            ssid: ap.ssid.as_str().to_string(),
            bssid: ap.bssid,
            channel: ap.channel,
            rssi: ap.signal_strength,
            auth: ap.auth_method,
        }
    }
}

// ---------------------------------------------------------------------------
//  共享状态（事件回调 <-> 主线程）
// ---------------------------------------------------------------------------

/// 事件回调和主线程之间共享的状态。
///
/// 事件回调是 `'static + Send`，拿不到 `&mut self`，所以计数器和标志
/// 必须走 `Arc<Atomic*>`。C 版用 `static` 全局变量 + event group，
/// 这里是同一份状态，只是换了个更 Rust 的载体。
struct Shared {
    /// 连续重连失败次数。拿到 IP 时清零。
    reconnect_tries: AtomicU32,
    /// 扫描模式标志（C 版 `s_scan_mode`）。重连前必须查。
    scan_mode: AtomicBool,
    /// 是否已经拿到 IP（C 版 `BIT_WIFI_UP`）。
    net_up: AtomicBool,
}

/// 免锁的「已拿到 IP」观察器。
///
/// `WifiManager` 本身要独占一个 `Mutex`，而 SNTP 对时会长时间持有它
/// （`sync()` 最长 60s）。主循环只想知道「能不能发网络请求」，用这个
/// 就够，不必去抢那把锁。
#[derive(Clone)]
pub struct NetUpWatch(Arc<Shared>);

impl NetUpWatch {
    /// 已拿到 IP 且当前未断开。
    ///
    /// 两条来源，任一成立即为「有网」：
    ///   1. 事件回调置的 `net_up`（`IpEvent::DhcpIpAssigned`）；
    ///   2. 直接向 netif 问当前 IP（[`netif_has_ip`]）。
    ///
    /// 为什么需要第 2 条：esp-idf-svc 0.53 + IDF 6.1 下 `IpEvent::DhcpIpAssigned`
    /// **实测不触发**，日志是 `esp_idf_svc::netif: IpEvent: unknown event ID 9,
    /// ignoring`（svc 的事件号映射和 IDF 6.1 的 netif 事件号对不上）。
    /// 只靠第 1 条的话 `net_up` 恒 false，主循环永远进不去 `ddns_tick`。
    /// 事件路径保留作双保险 —— 哪天 svc 修好了，这里仍然一致。
    ///
    /// 成本：每次一次 FFI 调用（`esp_netif_get_handle_from_ifkey` +
    /// `esp_netif_get_ip_info`，都是本地查表，不发包）。主循环 5s 一次，可忽略。
    #[allow(dead_code)] // 事件路径的便捷入口；main 直接用 is_up_live()
    pub fn is_up(&self) -> bool {
        self.is_up_live()
    }

    /// 同上，但查到 IP 时顺手把事件路径的状态也补上。
    ///
    /// 事件回调不触发时（见上），`net_up` 和 `reconnect_tries` 会一直停在
    /// 「未连上 / 退避中」的状态：重连计数永远不清零，退避会固化成慢速档。
    /// 这里查到真实 IP 就等价于「DHCP 成功」，把这两样一起复位，让两条
    /// 路径最终收敛到同一份状态。
    pub fn is_up_live(&self) -> bool {
        if self.0.net_up.load(Ordering::SeqCst) {
            return true;
        }
        if !netif_has_ip() {
            return false;
        }
        self.0.reconnect_tries.store(0, Ordering::SeqCst);
        self.0.net_up.store(true, Ordering::SeqCst);
        true
    }
}

/// STA netif 的接口键。`EspWifi` 建 STA 时用的就是这个 key。
const STA_IFKEY: &[u8] = b"WIFI_STA_DEF\0";

/// 直接问 netif：STA 接口当前是否拿到了非零 IPv4 地址。
///
/// 不持任何锁、不依赖事件，因此可以从任意任务/线程调用。
/// netif 句柄和 IP 信息都由 IDF 内部锁保护，短查询是线程安全的。
pub fn netif_has_ip() -> bool {
    // SAFETY: `STA_IFKEY` 是带结尾 `\0` 的静态字节串，指针在整个进程
    // 生命周期内有效；`esp_netif_get_handle_from_ifkey` 只读该字符串。
    let handle = unsafe {
        sys::esp_netif_get_handle_from_ifkey(STA_IFKEY.as_ptr() as *const core::ffi::c_char)
    };
    if handle.is_null() {
        return false;
    }

    // SAFETY: `esp_netif_ip_info_t` 是 POD（三个 u32 的 `esp_ip4_addr_t`），
    // 全零是合法的「未配置」表示，因此零初始化不构成 UB。`handle` 上面已判非空，
    // 且由 IDF 保证在 netif 存活期间有效；`&mut info` 指向本栈帧的局部变量。
    let mut info: sys::esp_netif_ip_info_t = unsafe { core::mem::zeroed() };
    let rc = unsafe { sys::esp_netif_get_ip_info(handle, &mut info) };
    if rc != sys::ESP_OK {
        return false;
    }

    // 0.0.0.0 表示 DHCP 还没完成（或在断开时被清空）。
    info.ip.addr != 0
}

// ---------------------------------------------------------------------------
//  WifiManager
// ---------------------------------------------------------------------------

/// Wi-Fi STA 管理器。
///
/// 持有 `BlockingWifi<EspWifi>`、事件订阅句柄和共享状态。
/// 生命周期 `'d` 与底层驱动一致。
pub struct WifiManager<'d> {
    wifi: BlockingWifi<EspWifi<'d>>,
    shared: Arc<Shared>,
    /// 事件订阅必须一直活着 —— drop 掉就等于注销回调（见
    /// `EspSubscription::drop`，eventloop.rs:374）。所以它被存在结构体里，
    /// 而不是 `let _sub = ...`。
    _sub: EspSubscription<'static, System>,
    /// 第二路订阅：IP 事件基（GOT_IP / LOST_IP）。
    ///
    /// `WifiEvent` 只覆盖 WIFI_EVENT 基，GOT_IP 属于 IP_EVENT 基，
    /// 两个事件基要分别订阅。同样必须常驻，drop 掉 `net_up` 就再也没人置位了。
    _sub_ip: EspSubscription<'static, System>,
    started: bool,
    // 注意：`TimeSync` **不**在这里。
    //
    // 以前它挂在 `WifiManager` 上，导致 `time_sync_loop` 为了调 `sync()`
    // 必须先拿 Wi-Fi 互斥锁，而 `sync()` 最长阻塞约 60s —— 期间控制台的
    // `wifi_reconnect` / `txpower` / `scan` 全部卡在 `lock()` 上。
    // SNTP 和 Wi-Fi 驱动之间没有数据依赖（只需要「已经连上」这个事实），
    // 所以对时器改由 `main.rs` 单独持有，两把锁互不相干。
}

impl<'d> WifiManager<'d> {
    /// 建驱动、装事件回调，但**不**启动、**不**连接。
    ///
    /// C 版的对应关系：`wifi_start()`（main.c:354–392）里建驱动那一半。
    /// 真正的连接动作留给 [`WifiManager::start`]。
    pub fn new<M: WifiModemPeripheral + 'd>(
        modem: M,
        sysloop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
    ) -> WifiResult<Self> {
        let shared = Arc::new(Shared {
            reconnect_tries: AtomicU32::new(0),
            scan_mode: AtomicBool::new(false),
            net_up: AtomicBool::new(false),
        });

        let esp_wifi = EspWifi::new(modem, sysloop.clone(), Some(nvs))?;
        // esp-idf-svc 0.53 起 `BlockingWifi::wrap` 要求显式传入事件循环
        // （svc 源码 `blocking::wifi::wrap(wifi, event_loop)`），不再是单参数。
        let wifi = BlockingWifi::wrap(esp_wifi, sysloop.clone())?;

        // ---- 事件回调：重连退避 + 扫描饥饿两道闸 ----
        let cb_shared = shared.clone();
        let sub = sysloop.subscribe::<WifiEvent, _>(move |event| match event {
            // 首次连接必须在这里发起：start() 只打开驱动，驱动不会自己连。
            // 对应 C 版 main.c:134–137。
            WifiEvent::StaStarted => {
                log::info!("STA 已启动，发起首次连接");
                let _ = unsafe { sys::esp_wifi_connect() };
            }

            WifiEvent::StaDisconnected(d) => {
                let reason = d.reason();
                let rssi = d.rssi();

                cb_shared.net_up.store(false, Ordering::SeqCst);

                // 把真实原因码翻译成人话 —— 排查 Wi-Fi 连不上唯一的可靠证据。
                // 逐条对照 C 版 main.c:145–162。
                log::warn!(
                    "Wi-Fi 断开: reason={} (0x{:x}) rssi={} —— {}",
                    reason,
                    reason,
                    rssi,
                    reason_text(reason)
                );

                // ---- 扫描饥饿 · 第一道闸（C 版 main.c:166–169）----
                // 扫描期间不要抢着重连：驱动不允许「连接中」扫描，
                // 自动重连一开，scan 命令就会永远返回 ESP_ERR_WIFI_STATE。
                if cb_shared.scan_mode.load(Ordering::SeqCst) {
                    log::info!("扫描模式：暂不自动重连");
                    return;
                }

                // ---- 重连退避（C 版 main.c:171–187）----
                let tries = cb_shared.reconnect_tries.fetch_add(1, Ordering::SeqCst) + 1;
                let backoff_ms = if tries <= RECONNECT_IMMEDIATE {
                    RECONNECT_FAST_MS
                } else {
                    RECONNECT_SLOW_MS
                };
                log::info!("第 {} 次连续重连失败，{} ms 后重试", tries, backoff_ms);

                // 注意：这里跑在事件循环任务上，阻塞它 = 阻塞后续事件派发。
                // C 版在同一个位置用 vTaskDelay，行为一致；退避最长 5s，
                // 对本项目的观感（串口 + 30s 看门狗）是可接受的。
                std::thread::sleep(Duration::from_millis(backoff_ms as u64));

                // ---- 扫描饥饿 · 第二道闸（C 版 main.c:189–198）----
                // ⚠️ 必须**再查一次**。上面那次检查在睡眠**之前**：
                // 如果这 2~5 秒里用户敲了 `scan`，醒来就会在扫描进行中
                // 调用 esp_wifi_connect()，把扫描打断（驱动里连接优先级
                // 高于扫描，会 abort scan 并返回 ESP_ERR_WIFI_STATE）。
                // 这正是「scan 命令永远扫到 0 个 AP」的成因之一。
                if cb_shared.scan_mode.load(Ordering::SeqCst) {
                    log::info!("扫描模式：放弃本次重连");
                    return;
                }

                let _ = unsafe { sys::esp_wifi_connect() };
            }

            // 注意：`WifiEvent` 里**没有** GOT_IP —— IP 事件属于另一个事件基
            // （IP_EVENT），所以置 net_up 的活儿在下面的 `IpEvent` 订阅里做。
            _ => {}
        })?;

        // ---- 第二路订阅：IP 事件基 ----
        // C 版在 GOT_IP 回调里做「计数清零 + 置 BIT_WIFI_UP」（main.c:204–211）。
        // 这里是唯一真正会把 `net_up` 置 true 的路径：驱动自己连上并完成 DHCP
        // 之后必发 IP_EVENT_STA_GOT_IP，不依赖任何人去调 `wait_netif_up()`。
        //
        // ⚠️ 订阅句柄必须和 `_sub` 一样常驻结构体 —— drop 掉就注销回调，
        // `net_up` 会重新变成永远 false（这正是「拿到 IP 仍报未连接」的成因）。
        let cb_shared_ip = shared.clone();
        let ip_sub = sysloop.subscribe::<IpEvent, _>(move |event| {
            // DhcpIpAssigned 同时覆盖 STA/ETH/PPP 的 GOT_IP；
            // 本项目只有 STA，用 `is_for_handle` 没法拿 netif 句柄，所以直接认变体。
            if let IpEvent::DhcpIpAssigned(assigned) = event {
                cb_shared_ip.reconnect_tries.store(0, Ordering::SeqCst);
                cb_shared_ip.net_up.store(true, Ordering::SeqCst);
                log::info!("已拿到 IP {}，net_up = true", assigned.ip());
            }
        })?;

        Ok(Self {
            wifi,
            shared,
            _sub: sub,
            _sub_ip: ip_sub,
            started: false,
        })
    }

    /// 写 STA 配置、关 modem sleep、设发射功率、启动、发起连接。
    ///
    /// 对应 C 版 `wifi_start()`（main.c:354–392）。
    ///
    /// `ssid` 空 → 视为未配置，直接报错（fail-closed，不猜）。
    /// `password` 空 → 开放网络：`auth_method = None`、不启用 PMF。
    pub fn start(&mut self, ssid: &str, password: &str) -> WifiResult<()> {
        if ssid.is_empty() {
            log::error!("Wi-Fi SSID 为空，拒绝启动（fail-closed）");
            return Err(WifiError::NotStarted);
        }

        // ---- 修复 3：PMF / 安全协商 ----
        // 有密码 → WPA2/WPA3 过渡模式的正确组合：
        //   auth_method = WPA2WPA3Personal  → 驱动 threshold.authmode
        //                                     = WIFI_AUTH_WPA2_WPA3_PSK
        //   pmf_cfg     = Capable { required: false }
        // 没密码 → 开放网络，不广播 PMF 能力。
        //
        // 为什么是这两个值（C 版 main.c:89–113 的注释）：
        //   capable = false  → 过渡模式 AP 拒绝，reason = 210
        //   required = true  → 纯 WPA2 的 AP 连不上
        // 只有 capable=true / required=false 两边都通。
        let secured = !password.is_empty();
        let client = ClientConfiguration {
            ssid: ssid.try_into().map_err(|_| {
                WifiError::Esp(sys::EspError::from_infallible::<{ sys::ESP_ERR_INVALID_ARG }>())
            })?,
            bssid: None,
            auth_method: if secured {
                AuthMethod::WPA2WPA3Personal
            } else {
                AuthMethod::None
            },
            password: password.try_into().map_err(|_| {
                WifiError::Esp(sys::EspError::from_infallible::<{ sys::ESP_ERR_INVALID_ARG }>())
            })?,
            channel: None,
            // 完整扫描 + 按信号排序：热点多的环境里比 fast scan 可靠。
            scan_method: ScanMethod::CompleteScan(ScanSortMethod::Signal),
            pmf_cfg: if secured {
                PmfConfiguration::Capable { required: false }
            } else {
                PmfConfiguration::NotCapable
            },
        };

        self.wifi
            .set_configuration(&Configuration::Client(client))?;

        // ---- 关闭 modem sleep（C 版 main.c:375–380）----
        // `WIFI_PS_NONE` 数值就是 0。开着省电模式会出 reason=4（ASSOC_EXPIRE）
        // 且 DHCP 永远拿不到地址。esp-idf-svc 没包装这个函数，走 sys。
        unsafe {
            sys::esp!(sys::esp_wifi_set_ps(0))?;
        }

        self.wifi.start()?;
        self.started = true;

        // ---- 发射功率（C 版 main.c:115–128）----
        self.apply_tx_power()?;

        // 首次连接**只**由 `WifiEvent::StaStarted` 回调发起，这里不再抢跑。
        //
        // 原来这里是 `self.wifi.start()?` 后紧跟 `self.wifi.connect()?`，而
        // `StaStarted` 回调里也调了 `esp_wifi_connect()`。两次撞车，硬件实测：
        //   E wifi:sta is connecting, return error
        //   E powerswitch: Wi-Fi 重连失败: esp-idf 错误: ESP_ERR_WIFI_CONN
        // 回调一定在 start 之后触发，比在 start 里抢跑可靠，故删掉这一行。

        log::info!("Wi-Fi 已启动，等待拿到 IP");
        Ok(())
    }

    /// 设置发射功率。
    ///
    /// 驱动单位是 0.25 dBm，所以 `dbm * 4`。对应 C 版 `wifi_apply_tx_power()`
    /// （main.c:115–128）。esp-idf-svc 没有包装 `esp_wifi_set_max_tx_power`，
    /// 只能走 `sys`。
    pub fn apply_tx_power(&self) -> WifiResult<()> {
        self.set_tx_power(WIFI_TX_POWER_DBM)
    }

    /// 把指定的发射功率推给驱动（驱动单位 0.25 dBm，所以乘 4）。
    ///
    /// 控制台 `txpower <dBm>` 要求「按新值立刻生效」，不能只认编译期常量，
    /// 所以实际动作拆到这里；[`Self::apply_tx_power`] 退化成用常量的薄封装。
    pub fn set_tx_power(&self, dbm: i8) -> WifiResult<()> {
        // 驱动只接受 int8_t，且实际会被量化到 {8,20,28,...,80}。
        let raw = dbm * 4;
        unsafe {
            sys::esp!(sys::esp_wifi_set_max_tx_power(raw))?;
        }

        // 回读实际值 —— 量化后的真实功率和请求值可能不同，值得记一笔。
        let mut actual: i8 = 0;
        unsafe {
            sys::esp!(sys::esp_wifi_get_max_tx_power(&mut actual))?;
        }
        log::info!(
            "发射功率 = {} dBm（驱动实际 {}.{:02} dBm）",
            dbm,
            actual / 4,
            (actual % 4) * 25
        );
        Ok(())
    }

    /// 阻塞等待拿到 IP，超时返回错误。
    ///
    /// 对应 C 版 app_main 里那个边喂看门狗边等的循环（main.c:607–617）。
    /// esp-idf-svc 的 `BlockingWifi::wait_netif_up()` 封装了这件事，
    /// 这里再加上「成功即清零重连计数」—— 对应 C 版 GOT_IP 回调
    /// （main.c:204–211）。
    ///
    /// ⚠️ `wait_netif_up()` 内部会阻塞当前任务。如果调用方在 app_main
    /// 里直接用，记得本项目开了 30s 任务看门狗（trigger_panic）——
    /// 不要把超时设得比看门狗还长而不喂狗。
    // 故意不调用：`main` 用 `net_up_watch` 轮询，避免 30s 阻塞撞上看门狗。
    #[allow(dead_code)]
    pub fn wait_netif_up(&self) -> WifiResult<()> {
        self.wifi.wait_netif_up()?;
        self.mark_net_up();
        Ok(())
    }

    /// 拿到 IP 之后的收尾：清零重连计数 + 置 net_up。
    ///
    /// C 版把这两件事放在 GOT_IP 事件回调里（main.c:204–211）。
    /// 用 GOT_IP 而不是 STA_CONNECTED 的理由：关联成功但 DHCP 拿不到 IP
    /// 不算「连上了」，那种情况该继续按失败退避。
    fn mark_net_up(&self) {
        self.shared.reconnect_tries.store(0, Ordering::SeqCst);
        self.shared.net_up.store(true, Ordering::SeqCst);
    }

    /// 当前是否已拿到 IP。
    pub fn is_connected(&self) -> WifiResult<bool> {
        Ok(self.wifi.is_connected()? && self.shared.net_up.load(Ordering::SeqCst))
    }

    /// 取一个免锁观察器，供主循环 / DDNS 调度使用。
    pub fn net_up_watch(&self) -> NetUpWatch {
        NetUpWatch(self.shared.clone())
    }

    /// 扫描一次并返回结果。
    ///
    /// 对应 C 版 `wifi_scan_log()`（main.c:221–300）。
    ///
    /// 扫描饥饿对策在这里落地：置 `scan_mode` → 断开 → 等驱动稳定 →
    /// 扫 → 清 `scan_mode` → 重连。最多试 `SCAN_ATTEMPTS` 次，
    /// `ESP_ERR_WIFI_STATE`（被连接打断）和「OK 但 0 个 AP」都重试。
    ///
    /// 注意：本函数会主动断开当前连接。C 版就是这么做的
    /// （`wifi_scan_now()`，main.c:336–352），因为驱动不允许在连接中扫描。
    pub fn scan(&mut self) -> WifiResult<Vec<ScanAp>> {
        if !self.started {
            return Err(WifiError::NotStarted);
        }

        // 置标志：告诉事件回调里的重连逻辑「现在是扫描模式，别抢空口」。
        // 对应 C 版 `s_scan_mode = true`。
        self.shared.scan_mode.store(true, Ordering::SeqCst);

        // 断开当前连接并给驱动一点时间落地。C 版等 400ms。
        // 断开失败无所谓（本来就没连），不往上抛。
        if let Err(e) = self.wifi.disconnect() {
            log::debug!("扫描前断开返回错误（可忽略）: {e}");
        }
        std::thread::sleep(Duration::from_millis(SCAN_DISCONNECT_SETTLE_MS as u64));

        // ---- 扫描参数，逐项对照 C 版 main.c:234–242 ----
        let scan_config = ScanConfig {
            bssid: None,
            ssid: None,    // 不限定 SSID：要看到所有 AP
            channel: None, // 0 = 全信道
            // 主动扫描，每信道 100/300 ms。被动扫描在拥挤环境里太慢。
            scan_type: ScanType::Active {
                min: Duration::from_millis(SCAN_ACTIVE_MIN_MS),
                max: Duration::from_millis(SCAN_ACTIVE_MAX_MS),
            },
            // 显示隐藏 SSID。C 版显式设了 true。
            show_hidden: true,
        };

        // `ran` 用来区分两种「0 个 AP」：
        //   - 三次全被 ESP_ERR_WIFI_STATE 拒掉 → 结果无效，射频未知；
        //   - 真的扫了 N 次但都是 0 → 现在可以怀疑射频了。
        // 这是 C 版 main.c:290 附近那个 `ran` 标志的同一件事。
        let mut ran = false;
        let mut last_err: Option<WifiError> = None;
        let mut result: Vec<ScanAp> = Vec::new();

        for attempt in 1..=SCAN_ATTEMPTS {
            match self.wifi.wifi_mut().start_scan(&scan_config, true) {
                Ok(()) => {
                    ran = true;
                    match self.wifi.wifi_mut().get_scan_result_n::<MAX_SCAN_APS>() {
                        Ok((aps, _n)) => {
                            result = aps.iter().map(ScanAp::from).collect();
                            if result.is_empty() {
                                log::warn!(
                                    "第 {}/{} 次扫描返回 0 个 AP（疑似被重连打断），{} ms 后重试",
                                    attempt,
                                    SCAN_ATTEMPTS,
                                    SCAN_RETRY_DELAY_MS
                                );
                                std::thread::sleep(Duration::from_millis(
                                    SCAN_RETRY_DELAY_MS as u64,
                                ));
                                continue;
                            }
                            break;
                        }
                        Err(e) => {
                            last_err = Some(WifiError::Esp(e));
                            log::warn!("取扫描结果失败: {e}");
                        }
                    }
                }
                Err(e) => {
                    // ESP_ERR_WIFI_STATE = 驱动正忙（连接中）。等一会儿再来。
                    // 对应 C 版 `ESP_ERR_WIFI_STATE` 分支。
                    log::warn!(
                        "第 {}/{} 次扫描被拒（驱动忙/连接中）: {e}，{} ms 后重试",
                        attempt,
                        SCAN_ATTEMPTS,
                        SCAN_RETRY_DELAY_MS
                    );
                    last_err = Some(WifiError::Esp(e));
                }
            }
            std::thread::sleep(Duration::from_millis(SCAN_RETRY_DELAY_MS as u64));
        }

        // ---- 收尾：清标志 + 重连 ----
        self.shared.scan_mode.store(false, Ordering::SeqCst);

        // 重新连回去。失败也不抛 —— 扫描已经拿到了，重连交给退避逻辑。
        if let Err(e) = self.wifi.connect() {
            log::warn!("扫描后重连发起失败: {e}");
        }

        if !ran {
            // 三次全被拒：结果不可信，不能据此判断射频好坏。
            log::error!(
                "{} 次扫描全部被驱动拒绝，结果无效（射频状态未知）",
                SCAN_ATTEMPTS
            );
            return Err(last_err.unwrap_or(WifiError::NotStarted));
        }

        log::info!("扫描完成：{} 个 AP", result.len());
        for ap in &result {
            log::info!(
                "  SSID=\"{}\" BSSID={:02x?} ch={} rssi={} auth={:?}",
                ap.ssid,
                ap.bssid,
                ap.channel,
                ap.rssi,
                ap.auth
            );
        }
        if result.is_empty() {
            log::warn!("扫描到了但一个 AP 都没有 —— 现在可以怀疑射频了");
        }
        Ok(result)
    }

    /// 断开当前连接（不关驱动）。
    ///
    /// 当前无调用者：`start()` 内部的重连走自己的路径，控制台也没有
    /// `wifi disconnect` 子命令。保留它是因为 `wifi_reconnect` 若改成
    /// 「先断后连」会用到 —— 那样能避免旧连接上的 DHCP 租约干扰。
    #[allow(dead_code)]
    pub fn disconnect(&mut self) -> WifiResult<()> {
        self.wifi.disconnect()?;
        self.shared.net_up.store(false, Ordering::SeqCst);
        Ok(())
    }

    // 对时入口**不在**这里。
    //
    // 以前有 `fn sync(&mut self)`，调用方必须先拿 Wi-Fi 互斥锁，再在里面
    // 阻塞最长 60s（两台 NTP 各 15s 超时 + 中间 30s 换台）。那 60 秒里
    // 控制台的 `wifi_reconnect` / `txpower` / `scan` 全部卡在 `lock()` 上。
    // SNTP 只需要「已经拿到 IP」这个事实，不需要 `&mut WifiManager`，
    // 所以 `TimeSync` 搬到 `main.rs` 的 `Backend` 里单独加锁（见
    // `time_sync_loop`）。这里不再提供任何对时入口。
}

// ---------------------------------------------------------------------------
//  时间同步
// ---------------------------------------------------------------------------

/// NTP 时间同步。fail-closed：同步不上就是失败，调用方不得放行命令。
///
/// C 版 `time_sync_task()`（main.c:400–419）定义了一堆常量却写死
/// `pool.ntp.org`。这里**真的**用 `consts::SNTP_SERVER_1` / `SNTP_SERVER_2`。
///
/// 关键约束：本项目 `sdkconfig` 里 `CONFIG_LWIP_SNTP_MAX_SERVERS=1`，
/// lwIP 只编译进**一个**服务器槽位。所以两个常量不是「同时传进 SntpConf」，
/// 而是**主/备故障切换**：先用 SNTP_SERVER_1，超时后 drop 掉重建，
/// 换 SNTP_SERVER_2。`EspSntp` 是单例（内部 `TAKEN` 互斥锁），
/// drop 会释放这个槽位，所以切换是可行的。
pub struct TimeSync {
    /// 当前活动的 SNTP 实例。`None` 表示还没起或者已经失败释放了。
    sntp: Option<EspSntp<'static>>,
    /// 已经试过几台服务器（用于主备切换）。
    tried: u32,
}

impl TimeSync {
    /// 新建，未启动。
    pub fn new() -> Self {
        Self {
            sntp: None,
            tried: 0,
        }
    }

    /// 用指定的服务器启动一次同步。
    fn start_with(&mut self, server: &'static str) -> WifiResult<()> {
        // SntpConf::servers 是 `[&str; SNTP_SERVER_NUM]`，而 SNTP_SERVER_NUM
        // 是私有常量 —— 没法直接写字面量。用 Default 起手再按下标赋值，
        // 不管数组多长都成立。
        let mut conf = SntpConf::default();
        conf.servers[0] = server;
        conf.operating_mode = OperatingMode::Poll;
        // Immediate：同步完成立刻跳时钟。命令校验要的是「现在是准的」，
        // smooth 那种慢慢校正对短时间窗没意义。
        conf.sync_mode = SyncMode::Immediate;

        // 先释放旧的（单例槽位），再建新的。
        self.sntp = None;
        self.sntp = Some(EspSntp::new(&conf)?);
        log::info!("NTP 同步启动，服务器 = {}", server);
        Ok(())
    }

    /// 阻塞等待同步完成，最多 `SNTP_SYNC_TIMEOUT_S` 秒。
    ///
    /// 轮询 `get_sync_status()`，睡 `SNTP_RETRY_FAST_S` 一次。
    fn wait_once(&self, server: &'static str) -> WifiResult<()> {
        let sntp = self.sntp.as_ref().ok_or(WifiError::NotStarted)?;
        let timeout = Duration::from_secs(SNTP_SYNC_TIMEOUT_S as u64);
        let step = Duration::from_secs(SNTP_RETRY_FAST_S as u64);
        let deadline = std::time::Instant::now() + timeout;

        loop {
            match sntp.get_sync_status() {
                SyncStatus::Completed => {
                    log::info!("NTP 同步完成（{}）", server);
                    return Ok(());
                }
                SyncStatus::Reset | SyncStatus::InProgress => {}
            }
            if std::time::Instant::now() >= deadline {
                log::warn!("NTP 同步超时（{}，{}s）", server, SNTP_SYNC_TIMEOUT_S);
                return Err(WifiError::TimeSyncTimeout {
                    server,
                    secs: SNTP_SYNC_TIMEOUT_S,
                });
            }
            std::thread::sleep(step);
        }
    }

    /// 主备切换地同步时间。这是给业务用的入口。
    ///
    /// 先用 `SNTP_SERVER_1`；超时则等 `SNTP_RETRY_SLOW_S` 再换
    /// `SNTP_SERVER_2`。两台都失败 → `TimeSyncExhausted`。
    ///
    /// **调用方必须在拿到 `Err` 时拒绝带时间窗的命令**（fail-closed）。
    pub fn sync(&mut self) -> WifiResult<()> {
        let servers: [&'static str; 2] = [SNTP_SERVER_1, SNTP_SERVER_2];

        for (i, server) in servers.iter().enumerate() {
            self.tried += 1;
            self.start_with(server)?;
            match self.wait_once(server) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!("NTP 服务器 {} 失败: {e}", server);
                    // 最后一台失败就不用再等了。
                    if i + 1 < servers.len() {
                        log::info!("{}s 后切换到备用 NTP 服务器", SNTP_RETRY_SLOW_S);
                        std::thread::sleep(Duration::from_secs(SNTP_RETRY_SLOW_S as u64));
                    }
                }
            }
        }

        // 两台都试过了。释放实例，别占着单例槽位。
        self.sntp = None;
        log::error!("NTP 主备服务器都同步失败");
        Err(WifiError::TimeSyncExhausted)
    }

    /// 已经试过几台服务器（诊断用）。
    #[allow(dead_code)] // 诊断接口，留给 `sntp status` 之类命令
    pub fn tried(&self) -> u32 {
        self.tried
    }
}

impl Default for TimeSync {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
//  原因码翻译
// ---------------------------------------------------------------------------

/// 把 Wi-Fi 断开原因码翻译成人话。
///
/// 逐条对照 C 版 `wifi_event_handler()` 里的 switch（main.c:145–162）。
/// 这是排查「连不上」唯一的可靠证据 —— 别删。
fn reason_text(reason: u16) -> &'static str {
    match reason {
        // 2 发生在认证帧阶段（4-way 之前）：STA 发出认证请求后没等到 AP 的
        // 认证响应。**不是密码错** —— 密码错会落在 15/204（握手阶段）。
        2 => "认证超时：AP 没回认证帧（加密/PMF 不匹配、AP 拒绝、或信号差）",
        // 15 和 204 是同一件事：204 是 Espressif 私有的 HANDSHAKE_TIMEOUT，
        // 官方说明其成因与 15（四次握手超时）相同。最常见是密码错，
        // 但信号太差也会触发 —— 别一口咬定密码。
        15 => "四次握手超时（最常见是密码错，也可能是信号太差）",
        200 => "信标丢失（信号太弱）",
        201 => "没找到这个 AP（SSID 错 / 5GHz？）",
        202 => "认证被 AP 拒绝（MAC 过滤 / 加密方式不兼容）",
        203 => "关联被 AP 拒绝（PMF 强制 / 连接数满）",
        204 => "四次握手超时（同 15：密码错或信号太差）",
        205 => "连接失败",
        // 210 / 211 / 212 就是修复 3 要消灭的那一族。
        210 => "找到 AP，但加密方式不兼容（很可能是 WPA3）",
        211 => "找到 AP，但加密方式低于我们设的阈值",
        212 => "找到 AP，但信号低于阈值",
        _ => "未知",
    }
}
