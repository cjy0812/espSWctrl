//! 编译期常量 —— 对应 C 版 `main/app_config.h`
//!
//! 改这里就能调，不用翻 main.rs。

// ---- 引脚（务必避开 strapping pin: GPIO2/8/9）----
/// 输出：驱动第一个 817D，模拟按下电源键
pub const PIN_PWR_SW: i32 = 5;
/// 输入：第二个 817D，读 SATA 5V 判断 PC 开关机
pub const PIN_PC_STATE: i32 = 6;

// ---- 脉冲参数（毫秒）----
// 主板识别阈值：>=100ms 有效；>=4000ms 触发强制断电
//
// 注意 PULSE_MS_MIN/MAX 是【安全边界】，只在这里定义，不进 NVS：
// 运行时的 pulse 命令只能在这个区间内调，改不出边界。
// 这两个值必须是 const —— 它们的安全性是靠「运行期改不了」保证的，
// 一旦变成 NVS 可写，整条防线就没了。
pub const PULSE_MS_MIN: u16 = 100;
pub const PULSE_MS_MAX: u16 = 3000;
/// 两次脉冲最小间隔
pub const PULSE_GAP_MIN_MS: u32 = 2000;

// 下面几个是【首次上电的默认值】。第一次启动时 NVS 是空的，用它们；
// 之后串口改过的值存在 NVS 里，会覆盖这里。
pub const PULSE_MS_DEFAULT: u16 = 300;
/// 秒：ts 允许的时间窗口
pub const CMD_TTL_DEFAULT: u32 = 30;
/// 记住最近多少个 nonce 用于去重
pub const NONCE_HISTORY: usize = 16;

// ---- Wi-Fi 射频 ----
/// 发射功率，单位 dBm。驱动内部单位是 0.25dBm，换算在 cfg/main 里做。
/// C3 硬件支持 2..20 dBm；驱动会把实际值量化到 {8,20,28,...,80}。
pub const WIFI_TX_POWER_DBM: i8 = 17;

// ---- DDNS ----
/// 默认更新周期（秒）。DNSHE 服务端下限是 60，这里取 300（5 分钟）：
/// 家用宽带的公网 IP 不会 5 分钟就变一次，打太勤只会把自己打成 abuse。
pub const DDNS_INTERVAL_DEFAULT_S: u32 = 300;
/// 允许配置的最短周期。`ddns::DDNS_MIN_INTERVAL_S` 是最终执行点，
/// 两处必须一致 —— 这里只是让「配置」和「执行」用同一个数字，
/// 免得串口里填 60 却被另一处夹成 60 之外的数。
pub const DDNS_INTERVAL_MIN_S: u32 = 60;
/// 上限：一天一次
pub const DDNS_INTERVAL_MAX_S: u32 = 86_400;

// ---- MQTT ----
pub const MQTT_KEEPALIVE_S: u32 = 60;
/// 状态回读上报周期
pub const STATUS_PUBLISH_S: u32 = 30;
/// 指数退避起点
pub const RECONNECT_BASE_MS: u32 = 1000;
/// 退避上限（防烧 Serverless 额度）
pub const RECONNECT_MAX_MS: u32 = 60_000;

// ---- 功能开关 ----
pub const ENABLE_STATUS_READBACK: bool = true;

// ---- 时间同步 ----
// 指令校验依赖时间窗（ts + ttl），所以 SNTP 必须成功，否则所有指令被拒。
// 不用 pool.ntp.org：国内经常能解析但 NTP 包不通。用国内可达的服务器。
//
// ⚠️ C 版在这里留了个坑：这五个常量定义得好好的，main.c 却写了
//    ESP_NETIF_SNTP_DEFAULT_CONFIG("pool.ntp.org") —— 常量全没用上，
//    而 pool.ntp.org 正是注释警告不可达的那个。Rust 版必须真的用这两个。
pub const SNTP_SERVER_1: &str = "ntp.aliyun.com";
pub const SNTP_SERVER_2: &str = "cn.pool.ntp.org";
pub const SNTP_SYNC_TIMEOUT_S: u32 = 15;
/// 前几次重试间隔
pub const SNTP_RETRY_FAST_S: u32 = 2;
/// 一直失败后的重试间隔
pub const SNTP_RETRY_SLOW_S: u32 = 30;
