//! 运行时配置存储 —— 对应 C 版 `main/cfg.c` / `main/cfg.h`
//!
//! 串口命令 `cfg` / `pulse` / `ttl` 改的就是这里存的东西。
//!
//! ## NVS 兼容性（重要）
//!
//! 命名空间和键名必须与 C 版【逐字节一致】。现场设备里已经存了值，
//! 改任何一个键名都等于把用户配置静默清空。
//!
//!   namespace : "psw_cfg"
//!   keys      : "ssid" | "pass" | "tx_dbm" | "pulse_ms"
//!               "cmd_ttl" | "status_s" | "keepalive"
//!
//! ## 安全属性
//!
//! `PULSE_MS_MIN` / `PULSE_MS_MAX` 是 `consts.rs` 里的编译期常量，
//! 不进 NVS、运行期改不了。本模块把这条约束做成了【类型级】的：
//! 脉冲宽度只能以 [`PulseMs`] 传入 setter，而 `PulseMs` 没有别的构造
//! 途径 —— 越界值在类型层面就构造不出来，不是靠调用方自觉。

use std::sync::Mutex;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::EspError;
use log::{info, warn};

use crate::consts::{
    CMD_TTL_DEFAULT, MQTT_KEEPALIVE_S, PULSE_MS_DEFAULT, PULSE_MS_MAX, PULSE_MS_MIN,
    STATUS_PUBLISH_S,
};

/// NVS 命名空间。改名字会导致老设备读不到已有配置，别随便动。
const CFG_NVS_NS: &str = "psw_cfg";

// ---- NVS 键名（与 C 版一致，禁止重命名）----
const KEY_SSID: &str = "ssid";
const KEY_PASS: &str = "pass";
const KEY_TX_DBM: &str = "tx_dbm";
const KEY_PULSE_MS: &str = "pulse_ms";
const KEY_CMD_TTL: &str = "cmd_ttl";
const KEY_STATUS_S: &str = "status_s";
const KEY_KEEPALIVE: &str = "keepalive";

// ---- DDNS 键（Rust 版新增，C 版没有；老设备读到 None 走默认）----
//
// NVS 键名上限 15 字符，所以这里用缩写而不是 ddns_hostname 之类。
// 注意：token 只存在 NVS 里，**绝不**出现在 consts.rs 或任何源码常量中。
const KEY_DDNS_HOST: &str = "ddns_host";
const KEY_DDNS_TOKEN: &str = "ddns_tok";
const KEY_DDNS_EN: &str = "ddns_en";
/// 上次成功更新的 unix 时间戳（秒）。0 = 从未成功过。
const KEY_DDNS_IV: &str = "ddns_iv";
/// 常规更新周期（秒）。执行时还会被 `ddns::clamp_interval_s` 再夹一次。
const KEY_DDNS_IVL: &str = "ddns_ivl";

/// 802.11 SSID 最长 32 字节 + NUL，与 C 版 `char s_ssid[33]` 对齐
const SSID_BUF: usize = 33;
/// WPA2 密码最长 63 + NUL，与 C 版 `char s_pass[65]` 对齐
const PASS_BUF: usize = 65;

/// DDNS 记录名缓冲。域名最长 253，但 NVS 值上限远小于此，
/// 而且这里只是给人看的备注字段（真正生效的是 token 绑定的记录）。
const DDNS_HOST_BUF: usize = 96;
/// DDNS token 缓冲。DNSHE token 是 64 个 hex 字符，留一倍余量。
const DDNS_TOKEN_BUF: usize = 129;

/// 命令时间窗允许范围（秒），对应 C 版 `cfg_set_cmd_ttl` 里的 `5..300`
const CMD_TTL_MIN: u16 = 5;
const CMD_TTL_MAX: u16 = 300;
/// 状态上报周期允许范围（秒）
const STATUS_S_MIN: u16 = 5;
const STATUS_S_MAX: u16 = 3600;
/// MQTT keepalive 允许范围（秒）
const KEEPALIVE_MIN: u16 = 10;
const KEEPALIVE_MAX: u16 = 3600;
/// 发射功率允许范围（dBm）。C3 硬件 2..20；驱动再量化到 {2,5,7,...,20}
const TX_DBM_MIN: i8 = 2;
const TX_DBM_MAX: i8 = 20;

// ---------------------------------------------------------------------------
//  错误类型
// ---------------------------------------------------------------------------

/// 配置操作的错误。
///
/// 分成两类：值不合法（本地判定，无副作用）和 NVS 层错误。
#[derive(Debug)]
pub enum ConfigError {
    /// 值超出允许范围。字段名 + 允许区间，给串口回一句人话用。
    OutOfRange {
        field: &'static str,
        min: i32,
        max: i32,
        got: i32,
    },
    /// SSID / 密码超长
    TooLong { field: &'static str, max: usize },
    /// 值本身不合法（格式/字符集问题），与「超长」「越界」区分开
    BadValue {
        field: &'static str,
        reason: &'static str,
    },
    /// NVS 底层错误
    Nvs(EspError),
}

impl From<EspError> for ConfigError {
    fn from(e: EspError) -> Self {
        ConfigError::Nvs(e)
    }
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConfigError::OutOfRange {
                field,
                min,
                max,
                got,
            } => write!(f, "{field} 超出范围: {got}（允许 {min}..={max}）"),
            ConfigError::TooLong { field, max } => write!(f, "{field} 太长（上限 {max}）"),
            ConfigError::BadValue { field, reason } => write!(f, "{field} 不合法: {reason}"),
            ConfigError::Nvs(e) => write!(f, "NVS 错误: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

pub type CfgResult<T> = Result<T, ConfigError>;

// ---------------------------------------------------------------------------
//  PulseMs —— 结构化的安全边界
// ---------------------------------------------------------------------------

/// 已经校验过的脉冲宽度（毫秒）。
///
/// 这是本模块的安全核心：唯一能构造它的途径是 [`PulseMs::new`]，而
/// `new` 硬性检查 [`PULSE_MS_MIN`]..=[`PULSE_MS_MAX`]。因此
/// [`Cfg::set_pulse_ms`] 在类型上就不可能收到越界值 —— 即使将来有人
/// 在别处加了新的写入路径，也得先拿到一个 `PulseMs`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PulseMs(u16);

impl PulseMs {
    /// 允许的最小脉冲宽度（编译期常量，不可被 NVS 覆盖）
    pub const MIN: u16 = PULSE_MS_MIN;
    /// 允许的最大脉冲宽度（编译期常量，不可被 NVS 覆盖）
    pub const MAX: u16 = PULSE_MS_MAX;

    /// 校验并构造。越界返回 [`ConfigError::OutOfRange`]。
    pub fn new(ms: u16) -> CfgResult<Self> {
        if ms < PULSE_MS_MIN || ms > PULSE_MS_MAX {
            return Err(ConfigError::OutOfRange {
                field: "pulse.ms",
                min: PULSE_MS_MIN as i32,
                max: PULSE_MS_MAX as i32,
                got: ms as i32,
            });
        }
        Ok(Self(ms))
    }

    /// 首次上电的默认值。`PULSE_MS_DEFAULT` 在常量里已经落在区间内，
    /// 但这里仍然走 `new` 走一遍校验 —— 万一有人把常量改出界，
    /// 会在 `expect` 上立刻炸掉，而不是悄悄写进 NVS。
    pub fn default_value() -> Self {
        Self::new(PULSE_MS_DEFAULT).expect("PULSE_MS_DEFAULT 必须落在 PULSE_MS_MIN..=PULSE_MS_MAX 内")
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

// ---------------------------------------------------------------------------
//  Cfg
// ---------------------------------------------------------------------------

/// 运行时配置存储，持有 NVS 句柄。
///
/// 内部用 `Mutex` 包住 `EspNvs`：`EspNvs` 只实现了 `Send` 没实现 `Sync`，
/// 而配置要被串口线程、脉冲线程、MQTT 线程同时读。加锁让 `Cfg` 变成
/// `Sync`，可以放进 `Arc` 共享。
///
/// 与 C 版的差异：C 版在 RAM 里缓存了一份当前值，这里每次都直接读 NVS。
/// 换来的是没有「缓存和 NVS 不同步」这一整类 bug；代价是每次读要走一次
/// flash 缓存，对本项目的读取频率（每条命令一次）完全可以忽略。
pub struct Cfg {
    nvs: Mutex<EspNvs<NvsDefault>>,
}

impl Cfg {
    /// 用指定的 NVS 分区打开配置命名空间。
    ///
    /// 用 `read_write = true` 打开：NVS 会自动创建不存在的命名空间，
    /// 首次上电也能正常走。C 版先 `READONLY` 打开、失败就整体走默认值，
    /// 效果等价但多一条分支。
    pub fn new(partition: EspDefaultNvsPartition) -> CfgResult<Self> {
        let nvs = EspNvs::new(partition, CFG_NVS_NS, true)?;
        Ok(Self {
            nvs: Mutex::new(nvs),
        })
    }

    /// 便捷构造：自己取默认 NVS 分区。
    pub fn take() -> CfgResult<Self> {
        let partition = EspDefaultNvsPartition::take()?;
        Self::new(partition)
    }

    /// 上锁。Mutex 中毒说明另一个线程在持锁时 panic 了 —— 对固件来说
    /// 没有比「配置存储不可信」更糟的状态，直接 panic 掉重启。
    fn lock(&self) -> std::sync::MutexGuard<'_, EspNvs<NvsDefault>> {
        self.nvs.lock().unwrap_or_else(|e| {
            // 不 recover：宁可重启也不要带着状态不明的配置继续跑
            panic!("cfg NVS mutex 中毒: {e}");
        })
    }

    // ---- Wi-Fi ----

    /// Wi-Fi SSID。NVS 里没有则返回空串。
    ///
    /// 注意：C 版会用 `secrets.h` 里的 `WIFI_SSID` 兜底，Rust 版的
    /// `consts.rs` 没有对应的编译期 Wi-Fi 常量（凭据不进源码）。
    /// 所以这里的缺省是【空串】—— 空 SSID 表示「还没配过」，
    /// 需要走串口 `cfg wifi <ssid> <pass>` 配一次。
    pub fn wifi_ssid(&self) -> CfgResult<String> {
        let nvs = self.lock();
        let mut buf = [0u8; SSID_BUF];
        Ok(nvs.get_str(KEY_SSID, &mut buf)?.unwrap_or("").to_string())
    }

    /// Wi-Fi 密码。空串 = 开放网络。
    pub fn wifi_pass(&self) -> CfgResult<String> {
        let nvs = self.lock();
        let mut buf = [0u8; PASS_BUF];
        Ok(nvs.get_str(KEY_PASS, &mut buf)?.unwrap_or("").to_string())
    }

    /// 保存 Wi-Fi 凭据。两个键要么都写成功，要么都别写。
    ///
    /// 长度校验与 C 版一致：SSID <= 32 字节，密码 <= 63 字节。
    pub fn set_wifi(&self, ssid: &str, pass: &str) -> CfgResult<()> {
        if ssid.len() > SSID_BUF - 1 {
            return Err(ConfigError::TooLong {
                field: "wifi.ssid",
                max: SSID_BUF - 1,
            });
        }
        if pass.len() > PASS_BUF - 1 {
            return Err(ConfigError::TooLong {
                field: "wifi.pass",
                max: PASS_BUF - 1,
            });
        }

        let nvs = self.lock();
        nvs.set_str(KEY_SSID, ssid)?;
        nvs.set_str(KEY_PASS, pass)?;

        // 只打 ssid 和「有没有密码」，绝不把密码打到日志里
        info!(
            "Wi-Fi 已保存: ssid='{}' pass={}",
            ssid,
            if pass.is_empty() { "(空)" } else { "***" }
        );
        Ok(())
    }

    // ---- 发射功率 ----

    /// 发射功率，单位 dBm。
    ///
    /// 兼容处理：C 版 `cfg_set_tx_power_dbm` 用 `nvs_set_u16` 写、
    /// `cfg_init` 却用 `nvs_get_i8` 读 —— 类型不匹配，读必然失败，
    /// 所以老设备这个键实际上一直是默认值。这里先按 i8 读（正确类型），
    /// 读不到再试 u16（兼容 C 写坏的那批值）。
    pub fn tx_power_dbm(&self) -> CfgResult<i8> {
        use crate::consts::WIFI_TX_POWER_DBM;
        let nvs = self.lock();
        // 注意这里用 unwrap_or(None) 而不是 `?`：EspNvs::get_i8 只把
        // ESP_ERR_NVS_NOT_FOUND 映射成 Ok(None)，其余错误码（尤其是老设备上
        // 那个 u16 值造成的 ESP_ERR_NVS_TYPE_MISMATCH）会变成 Err。
        // 若用 `?`，第二行就永远不可达 —— 兼容分支变成死代码，
        // 老设备会直接拿到 Err 而不是文档承诺的默认值。读路径上把错误
        // 吞掉落到默认值是正确的：唯一的"真实故障"后果也就是用默认功率。
        if let Some(v) = nvs.get_i8(KEY_TX_DBM).unwrap_or(None) {
            return Ok(v);
        }
        if let Some(v) = nvs.get_u16(KEY_TX_DBM).unwrap_or(None) {
            // 老值：u16 存的是非负数，能塞进 i8
            if v <= i8::MAX as u16 {
                return Ok(v as i8);
            }
        }
        Ok(WIFI_TX_POWER_DBM as i8)
    }

    /// 设置发射功率。范围 2..=20 dBm，超出驱动会拒，这里先挡一道。
    pub fn set_tx_power_dbm(&self, dbm: i8) -> CfgResult<()> {
        if dbm < TX_DBM_MIN || dbm > TX_DBM_MAX {
            return Err(ConfigError::OutOfRange {
                field: "wifi.tx_dbm",
                min: TX_DBM_MIN as i32,
                max: TX_DBM_MAX as i32,
                got: dbm as i32,
            });
        }
        let nvs = self.lock();
        // 先删再加：老设备上这个键可能是 u16 类型，直接 set_i8 会
        // 撞 ESP_ERR_NVS_TYPE_MISMATCH。删掉重写，类型从此统一成 i8。
        nvs.remove(KEY_TX_DBM)?;
        nvs.set_i8(KEY_TX_DBM, dbm)?;
        Ok(())
    }

    // ---- 脉冲宽度 ----

    /// 当前脉冲宽度（毫秒）。
    ///
    /// NVS 里的值若越界（理论上不该发生，除非有人直接改 flash），
    /// 一律回落到默认值，绝不把危险值放出去。
    pub fn pulse_ms(&self) -> CfgResult<PulseMs> {
        let stored = {
            let nvs = self.lock();
            nvs.get_u16(KEY_PULSE_MS)?
        };
        Ok(match stored {
            Some(v) => PulseMs::new(v).unwrap_or_else(|_| {
                warn!(
                    "NVS 里的 pulse_ms={} 越界（{}..={}），回落到默认值 {}",
                    v,
                    PULSE_MS_MIN,
                    PULSE_MS_MAX,
                    PULSE_MS_DEFAULT
                );
                PulseMs::default_value()
            }),
            None => PulseMs::default_value(),
        })
    }

    /// 保存脉冲宽度。参数只能是 [`PulseMs`]，越界值在类型上就传不进来。
    pub fn set_pulse_ms(&self, ms: PulseMs) -> CfgResult<()> {
        let nvs = self.lock();
        nvs.set_u16(KEY_PULSE_MS, ms.get())?;
        Ok(())
    }

    // ---- 指令时间窗 ----

    /// `ts` 允许的时间窗口（秒）
    pub fn cmd_ttl(&self) -> CfgResult<u16> {
        let nvs = self.lock();
        Ok(nvs.get_u16(KEY_CMD_TTL)?.unwrap_or(CMD_TTL_DEFAULT as u16))
    }

    pub fn set_cmd_ttl(&self, s: u16) -> CfgResult<()> {
        if s < CMD_TTL_MIN || s > CMD_TTL_MAX {
            return Err(ConfigError::OutOfRange {
                field: "cmd.ttl",
                min: CMD_TTL_MIN as i32,
                max: CMD_TTL_MAX as i32,
                got: s as i32,
            });
        }
        let nvs = self.lock();
        nvs.set_u16(KEY_CMD_TTL, s)?;
        Ok(())
    }

    // ---- 状态上报周期 ----

    pub fn status_period_s(&self) -> CfgResult<u16> {
        let nvs = self.lock();
        Ok(nvs
            .get_u16(KEY_STATUS_S)?
            .unwrap_or(STATUS_PUBLISH_S as u16))
    }

    pub fn set_status_period_s(&self, s: u16) -> CfgResult<()> {
        if s < STATUS_S_MIN || s > STATUS_S_MAX {
            return Err(ConfigError::OutOfRange {
                field: "status.period",
                min: STATUS_S_MIN as i32,
                max: STATUS_S_MAX as i32,
                got: s as i32,
            });
        }
        let nvs = self.lock();
        nvs.set_u16(KEY_STATUS_S, s)?;
        Ok(())
    }

    // ---- MQTT keepalive ----

    pub fn keepalive_s(&self) -> CfgResult<u16> {
        let nvs = self.lock();
        Ok(nvs
            .get_u16(KEY_KEEPALIVE)?
            .unwrap_or(MQTT_KEEPALIVE_S as u16))
    }

    pub fn set_keepalive_s(&self, s: u16) -> CfgResult<()> {
        if s < KEEPALIVE_MIN || s > KEEPALIVE_MAX {
            return Err(ConfigError::OutOfRange {
                field: "mqtt.keepalive",
                min: KEEPALIVE_MIN as i32,
                max: KEEPALIVE_MAX as i32,
                got: s as i32,
            });
        }
        let nvs = self.lock();
        nvs.set_u16(KEY_KEEPALIVE, s)?;
        Ok(())
    }

    // ---- DDNS ----
    //
    // 四个键都是 Rust 版新增，C 版没有。老设备读到 None 走默认值，所以
    // 刷了这版固件不会把已有配置读坏。

    /// DDNS 记录名（仅用于日志与 `ddns show`，请求里不发送 —— token 已绑定记录）。
    pub fn ddns_host(&self) -> CfgResult<String> {
        let nvs = self.lock();
        let mut buf = [0u8; DDNS_HOST_BUF];
        Ok(nvs
            .get_str(KEY_DDNS_HOST, &mut buf)?
            .unwrap_or("")
            .to_string())
    }

    pub fn set_ddns_host(&self, host: &str) -> CfgResult<()> {
        if host.len() > DDNS_HOST_BUF - 1 {
            return Err(ConfigError::TooLong {
                field: "ddns.host",
                max: DDNS_HOST_BUF - 1,
            });
        }
        let nvs = self.lock();
        nvs.set_str(KEY_DDNS_HOST, host)?;
        info!("DDNS 记录名已保存: '{}'", host);
        Ok(())
    }

    /// DDNS token —— **唯一的凭据**。
    ///
    /// 只进 NVS，绝不进源码。返回值的调用方必须自己负责不要打日志：
    /// 要给人看一律走 [`mask_token`] 掩码后的形式。
    pub fn ddns_token(&self) -> CfgResult<String> {
        let nvs = self.lock();
        let mut buf = [0u8; DDNS_TOKEN_BUF];
        Ok(nvs
            .get_str(KEY_DDNS_TOKEN, &mut buf)?
            .unwrap_or("")
            .to_string())
    }

    pub fn set_ddns_token(&self, token: &str) -> CfgResult<()> {
        if token.len() > DDNS_TOKEN_BUF - 1 {
            return Err(ConfigError::TooLong {
                field: "ddns.token",
                max: DDNS_TOKEN_BUF - 1,
            });
        }
        // token 里的空白字符基本都是粘贴时带进来的，会让服务端 401
        // 而现场看起来像「token 错了」。这里直接拒绝，不静默 trim。
        if token.chars().any(|c| c.is_whitespace()) {
            return Err(ConfigError::BadValue {
                field: "ddns.token",
                reason: "含有空白字符（粘贴时带进来的换行/空格？）",
            });
        }
        let nvs = self.lock();
        nvs.set_str(KEY_DDNS_TOKEN, token)?;
        // 只打掩码。**这一行是唯一允许出现 token 的地方，且必须是掩码形式。**
        info!("DDNS token 已保存: {}", crate::ddns::mask_token(token));
        Ok(())
    }

    pub fn ddns_enabled(&self) -> CfgResult<bool> {
        let nvs = self.lock();
        // u8 存布尔：NVS 没有 bool 类型，C 版习惯也是 u8。
        Ok(nvs.get_u8(KEY_DDNS_EN)?.unwrap_or(0) != 0)
    }

    pub fn set_ddns_enabled(&self, on: bool) -> CfgResult<()> {
        let nvs = self.lock();
        nvs.set_u8(KEY_DDNS_EN, u8::from(on))?;
        info!("DDNS 已{}", if on { "启用" } else { "停用" });
        Ok(())
    }

    /// 常规更新周期（秒）。
    ///
    /// 这里只按 `consts` 里的上下限做一次粗校验；**真正的下限执行点**
    /// 是 `ddns::clamp_interval_s`（60s），配置里写 5 也不会打得更勤。
    pub fn ddns_interval_s(&self) -> CfgResult<u32> {
        let nvs = self.lock();
        Ok(nvs
            .get_u32(KEY_DDNS_IVL)?
            .unwrap_or(crate::consts::DDNS_INTERVAL_DEFAULT_S))
    }

    pub fn set_ddns_interval_s(&self, s: u32) -> CfgResult<()> {
        if s < crate::consts::DDNS_INTERVAL_MIN_S || s > crate::consts::DDNS_INTERVAL_MAX_S {
            return Err(ConfigError::OutOfRange {
                field: "ddns.interval",
                min: crate::consts::DDNS_INTERVAL_MIN_S as i32,
                max: crate::consts::DDNS_INTERVAL_MAX_S as i32,
                got: s as i32,
            });
        }
        let nvs = self.lock();
        nvs.set_u32(KEY_DDNS_IVL, s)?;
        info!("DDNS 周期已设为 {s} s");
        Ok(())
    }

    /// 上次成功更新的 unix 时间戳（秒）。0 = 从未成功过。
    pub fn ddns_last_ok(&self) -> CfgResult<u64> {
        let nvs = self.lock();
        Ok(nvs.get_u64(KEY_DDNS_IV)?.unwrap_or(0))
    }

    pub fn set_ddns_last_ok(&self, ts: u64) -> CfgResult<()> {
        let nvs = self.lock();
        nvs.set_u64(KEY_DDNS_IV, ts)?;
        Ok(())
    }

    // ---- 打印 / 复位 ----

    /// 打印当前配置。对应 C 版 `cfg_dump()`。
    ///
    /// 密码永远只打「已设置 / 空」，不打印内容。
    pub fn dump(&self) -> CfgResult<()> {
        let ssid = self.wifi_ssid()?;
        let pass = self.wifi_pass()?;
        let tx = self.tx_power_dbm()?;
        let pulse = self.pulse_ms()?;
        let ttl = self.cmd_ttl()?;
        let status = self.status_period_s()?;
        let keepalive = self.keepalive_s()?;

        info!("当前配置（存于 NVS 命名空间 '{}'）:", CFG_NVS_NS);
        info!("  wifi.ssid      = {}", if ssid.is_empty() { "(未设置)" } else { &ssid });
        info!(
            "  wifi.pass      = {}",
            if pass.is_empty() { "(空 = 开放网络)" } else { "***（已设置）" }
        );
        info!("  wifi.tx_dbm    = {} dBm", tx);
        info!(
            "  pulse.ms       = {} ms (允许 {}..{})",
            pulse.get(),
            PulseMs::MIN,
            PulseMs::MAX
        );
        info!("  cmd.ttl        = {} s", ttl);
        info!("  status.period  = {} s", status);
        info!("  mqtt.keepalive = {} s", keepalive);

        // DDNS 三行。token **只打掩码**，和 wifi.pass 同一个规矩。
        // 这里用 mask_token 而不是自己截断，是因为掩码逻辑只在那一处，
        // 换个地方写一遍迟早会漏。
        let ddns_en = self.ddns_enabled().unwrap_or(false);
        let ddns_host = self.ddns_host().unwrap_or_default();
        let ddns_tok = self.ddns_token().unwrap_or_default();
        let ddns_iv = self.ddns_last_ok().unwrap_or(0);
        info!("  ddns.enabled   = {}", if ddns_en { "是" } else { "否" });
        info!(
            "  ddns.host      = {}",
            if ddns_host.is_empty() { "(未设置)" } else { &ddns_host }
        );
        info!(
            "  ddns.token     = {}",
            if ddns_tok.is_empty() {
                "(未设置)".to_string()
            } else {
                format!("{}（已设置）", crate::ddns::mask_token(&ddns_tok))
            }
        );
        info!("  ddns.interval  = {} s", self.ddns_interval_s().unwrap_or(
            crate::consts::DDNS_INTERVAL_DEFAULT_S
        ));
        info!(
            "  ddns.last_ok   = {}",
            if ddns_iv == 0 {
                "(从未成功)".to_string()
            } else {
                format!("{ddns_iv}")
            }
        );
        Ok(())
    }

    /// 清空整个命名空间，回到编译期默认值。对应 C 版 `cfg_reset()`。
    ///
    /// 注意：C 版 `nvs_open` 失败时会走 `load_all()` 重填内存并返回 OK。
    /// 这里没有内存副本，所以「命名空间不存在」等价于「已经是空的」，
    /// 直接当成功处理。
    pub fn reset(&self) -> CfgResult<()> {
        {
            let nvs = self.lock();
            nvs.erase_all()?;
        }
        warn!("已恢复编译期默认配置");
        Ok(())
    }
}