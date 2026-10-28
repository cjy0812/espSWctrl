//! 串口控制台命令 —— 对应 C 版 `main/console_cmds.c`
//!
//! 目的：把「改个 Wi-Fi 密码要重新编译 + 烧录」这件事彻底消掉。
//!
//! 用法（COM11 上敲）：
//! ```text
//!   help                     列出所有命令
//!   cfg                      打印当前配置
//!   wifi <ssid> [pass]       改 Wi-Fi 并存 NVS，立即重连
//!   txpower <dBm>            改发射功率（2..20），立即生效
//!   pulse <ms>               改默认脉冲宽度（100..3000）
//!   ttl <秒>                 改指令时间窗（5..300）
//!   status <秒>              改状态上报周期（5..3600）
//!   keepalive <秒>           改 MQTT keepalive（10..3600，重连后生效）
//!   scan                     立刻扫描一次 Wi-Fi
//!   press [ms]               手动触发一次电源键脉冲
//!   reset                    清空 NVS，恢复编译期默认值
//!   reboot                   软重启
//! ```
//!
//! # 安全说明（本模块存在的意义）
//!
//! - **控制台命令绝不做 GPIO 操作**。所有「按电源键」都走
//!   [`ConsoleBackend::pulse_request`]，也就是和 MQTT 完全相同的那条队列，
//!   所以不可能绕开「脉冲独立任务」这条安全线。本文件里没有
//!   `set_high` / `set_low`，也不 import 任何 GPIO 类型。
//! - **密码不会回显明文**：`wifi` 命令只打印 `***` 或 `(空)`。
//!
//! # 为什么直接用 raw bindings
//!
//! `esp-idf-svc` 0.53 没有包装 `esp_console`，所以这里直接调用
//! `esp_idf_svc::sys` 下的 C API。

use core::ffi::{c_char, c_int};
use core::mem::MaybeUninit;
use std::ffi::{CStr, CString};
use std::io::Write as _;
use std::sync::OnceLock;

use esp_idf_svc::sys;
use esp_idf_svc::sys::EspError;
use log::{error, info};

use crate::consts::{PULSE_MS_MAX, PULSE_MS_MIN};

// ---------------------------------------------------------------------------
//  对外接口：宿主必须提供的后端能力
// ---------------------------------------------------------------------------

/// 命令执行失败的原因。
///
/// 刻意不直接用 `EspError`：`pulse_request` 的「队列满」在 C 版里被翻译成
/// 一句「忙：上一次脉冲还没结束」，它不是设备错误。用这个枚举把
/// 「忙」和「保存失败」分开，输出才能和 C 版逐字一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdError {
    /// 脉冲队列满 —— 上一次脉冲还没结束
    Busy,
    /// 其它失败（NVS 保存失败、Wi-Fi 未就绪等），附带可打印的原因
    Other(String),
}

/// 控制台需要宿主提供的一切能力。
///
/// 由 `main.rs` 实现（通常就是转发给 `cfg` / `wifi_ctrl` / `pulse` 三个模块）。
/// 之所以用 trait 而不是直接 `use crate::cfg::*`：
///   - 本模块只依赖 `consts`，不依赖 `cfg.rs` / `wifi_ctrl` 的具体签名，
///     这样各模块可以并行重写、互不阻塞；
///   - `cfg` 的读写要在控制台任务里跨线程用，trait 上带 `Send + Sync`
///     把这个要求显式化，编译期就能挡住「后端里塞了个 Rc」这种错误。
///
/// 所有方法都可能从 REPL 任务线程调用，实现必须线程安全。
pub trait ConsoleBackend: Send + Sync {
    // ---- 只读查询 ----
    /// 打印当前全部配置（密码自行掩码）
    fn cfg_dump(&self);
    /// 当前发射功率，dBm
    fn tx_power_dbm(&self) -> i32;
    /// 当前默认脉冲宽度，ms
    fn pulse_ms(&self) -> u16;
    /// 当前指令时间窗，秒
    fn cmd_ttl(&self) -> u16;
    /// 当前状态上报周期，秒
    fn status_period_s(&self) -> u16;
    /// 当前 MQTT keepalive，秒
    fn keepalive_s(&self) -> u16;

    // ---- MQTT 只读查询 ----
    //
    // 与 DDNS 同一规矩：trait 上没有「读密码明文」的方法。
    /// broker 主机名（不含 scheme / 端口），未配置返回空串
    fn mqtt_host(&self) -> String;
    /// MQTT 用户名，未配置返回空串
    fn mqtt_user(&self) -> String;
    /// MQTT 密码的掩码形式（未配置返回 "(未设置)"），**永不返回明文**
    fn mqtt_pass_masked(&self) -> String;
    /// MQTT 开关是否打开
    fn mqtt_enabled(&self) -> bool;
    /// MQTT 连接状态。`None` = 客户端未启动（未启用或建连失败）；
    /// `Some(true)` = 已连上 broker；`Some(false)` = 已启动但当前断开（自动重连中）。
    fn mqtt_online(&self) -> Option<bool>;
    /// 请求 MQTT 线程立刻重发一次 retained 的 PC 状态。
    ///
    /// 与 [`Self::ddns_trigger_now`] 同一纪律：**只在队列里投一个请求**，
    /// 绝不在 REPL 任务里做网络 I/O。返回 `false` 表示客户端未启动或队列已满。
    fn mqtt_refresh_state(&self) -> bool;

    // ---- 设备 ID ----
    /// 设备 ID（决定 topic 前缀与 client_id）
    fn dev_id(&self) -> String;

    // ---- DDNS 只读查询 ----
    //
    // 注意这里**没有**「读 token 明文」的方法。控制台永远只拿得到掩码，
    // 明文只在 main.rs 主循环内部直接向 cfg 取用 —— 少一条通路就少一个泄漏点。
    /// DDNS 主机名（未配置返回空串）
    fn ddns_host(&self) -> String;
    /// DDNS token 的掩码形式（未配置返回 "(未设置)"），**永不返回明文**
    fn ddns_token_masked(&self) -> String;
    /// DDNS 开关是否打开
    fn ddns_enabled(&self) -> bool;
    /// DDNS 常规更新周期，秒
    fn ddns_interval_s(&self) -> u32;
    /// 上次成功更新的 unix 时间戳；0 = 从未成功
    fn ddns_last_ok(&self) -> u64;

    // ---- DDNS 立即触发 ----
    /// 请求「下一轮主循环立刻跑一次 DDNS」。
    ///
    /// 只置一个 `AtomicBool`，**绝不做网络 I/O** —— 命令回调跑在 REPL 任务上，
    /// 在这里发 HTTPS 会卡住串口十几秒。真正的请求由主循环发出。
    fn ddns_trigger_now(&self);

    // ---- 写入（已含 NVS 保存）----
    /// 保存 Wi-Fi 凭据到 NVS。**不负责重连** —— 重连由 `wifi_reconnect` 做，
    /// 和 C 版 `cfg_set_wifi` / `wifi_reconnect_with_cfg` 的拆分保持一致。
    fn set_wifi(&self, ssid: &str, pass: &str) -> Result<(), CmdError>;
    fn set_tx_power_dbm(&self, dbm: i8) -> Result<(), CmdError>;
    fn set_pulse_ms(&self, ms: u16) -> Result<(), CmdError>;
    fn set_cmd_ttl(&self, s: u16) -> Result<(), CmdError>;
    fn set_status_period_s(&self, s: u16) -> Result<(), CmdError>;
    fn set_keepalive_s(&self, s: u16) -> Result<(), CmdError>;

    // ---- MQTT 写入 ----
    //
    // 与 DDNS 同一拆分：这里只落 NVS，不建连、不发网络。
    // MQTT 客户端在 Phase 5 才存在，改完凭据需重启生效。
    fn set_mqtt_host(&self, host: &str) -> Result<(), CmdError>;
    fn set_mqtt_user(&self, user: &str) -> Result<(), CmdError>;
    fn set_mqtt_pass(&self, pass: &str) -> Result<(), CmdError>;
    fn set_mqtt_enabled(&self, en: bool) -> Result<(), CmdError>;
    /// 设备 ID。改动会同时改变 topic 前缀与 client_id，需重启生效。
    fn set_dev_id(&self, id: &str) -> Result<(), CmdError>;

    fn set_ddns_host(&self, host: &str) -> Result<(), CmdError>;
    fn set_ddns_token(&self, token: &str) -> Result<(), CmdError>;
    fn set_ddns_enabled(&self, en: bool) -> Result<(), CmdError>;
    fn set_ddns_interval_s(&self, s: u32) -> Result<(), CmdError>;

    // ---- 立即生效的副作用 ----
    /// 用当前 NVS 里的配置重新连接 Wi-Fi
    fn wifi_reconnect(&self);
    /// 立刻把发射功率推给驱动（C 版 `wifi_set_tx_power_now`）
    fn wifi_set_tx_power_now(&self, dbm: i8);
    /// 立刻扫描一次 Wi-Fi（C 版 `wifi_scan_now`，约 2 秒）
    fn wifi_scan_now(&self);
    /// 清空 NVS 并恢复编译期默认值
    fn cfg_reset(&self) -> Result<(), CmdError>;

    // ---- 安全线 ----
    /// 投递一次电源键脉冲。
    ///
    /// **这是本模块触碰电源键的唯一途径**，实现必须直接转发到
    /// `PulseHandle::request`，不得自己拉 GPIO。
    fn pulse_request(&self, width_ms: u16) -> Result<(), CmdError>;
}

// ---------------------------------------------------------------------------
//  全局状态
// ---------------------------------------------------------------------------

/// 后端句柄。只在 [`init`] 里写入一次，之后所有 `extern "C"` 命令处理器
/// 从这里取。用 `OnceLock` 而不是 `static mut`：初始化是显式的一次性动作，
/// 且读路径无锁、无 unsafe。
static BACKEND: OnceLock<&'static dyn ConsoleBackend> = OnceLock::new();

/// REPL 句柄的落脚点。
///
/// **为什么必须留一个 static**：`esp_console_new_repl_usb_serial_jtag` 返回的
/// 是一个堆上对象，REPL 后台任务持有它。只要我们不调用它的 `del`，它就
/// 一直有效；但如果我们把这个指针只放在 `init` 的局部变量里，函数返回后
/// 指针值就丢了，后续也没法再引用它 —— 更糟的是，任何「顺手清理一下」的
/// 改动都可能在不知情的情况下销毁 REPL。把它钉在 static 里，语义就变成
/// 「进程级存活，永不销毁」，这正是我们要的。
///
/// 存成 `usize` 而不是裸指针，是为了让这个 static 自动满足 `Sync`。
static REPL: OnceLock<usize> = OnceLock::new();

/// FreeRTOS 的 `tskNO_AFFINITY`。
/// bindgen 不一定导出这个宏，所以写字面值 + 注释。
const TSK_NO_AFFINITY: i32 = 0x7FFF_FFFF;

// ---------------------------------------------------------------------------
//  小工具
// ---------------------------------------------------------------------------

/// 把字符串泄漏成 `'static` 的 C 字符串指针。
///
/// IDF 只在注册时**借用**这些指针（`esp_console_cmd_t` 的注释明确说
/// struct 本身可以是临时值），但字符串本身必须活到 `esp_console_deinit`。
/// 命令表是固定的一小撮字面量、且注册只发生一次，所以这里主动 `into_raw`
/// 泄漏是正确取舍：泄漏量是常量级（十几条命令的名称与帮助文本），
/// 换来的是「绝不会有悬垂指针」这条硬保证。
fn leak_cstr(s: &str) -> *const c_char {
    CString::new(s).expect("控制台字面量不含 NUL").into_raw() as *const c_char
}

/// 安全地取第 `i` 个参数。
///
/// # Safety
///
/// `argv` 必须是 `esp_console` 传给命令处理器的那个数组，且在本次调用期间
/// 有效；`argc` 必须是它的元素个数。这是 IDF 的契约，处理器只被 IDF 调用。
unsafe fn arg<'a>(argc: c_int, argv: *mut *mut c_char, i: usize) -> Option<&'a str> {
    // 先做边界检查再解引用 —— 不能信任 argc 与 argv 的一致性之外的东西。
    if argv.is_null() || i >= argc as usize {
        return None;
    }
    // SAFETY: 上面已确认 i < argc，且 argv 非空，所以 argv.add(i) 落在数组内。
    let p = unsafe { *argv.add(i) };
    if p.is_null() {
        return None;
    }
    // SAFETY: 按 IDF 契约，argv[i] 非空时指向以 NUL 结尾的 C 字符串，
    // 生命周期覆盖整个命令调用（'a 由调用方保证不超过这一点）。
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// 把 `arg` 的结果和解析失败统一成 `Result`。
///
/// 刻意不用 `atoi`：`atoi` 遇到垃圾输入返回 0，会静默地变成「设为 0」
/// 然后再被范围检查拒绝，错误信息指向错误的方向。这里直接报「不是整数」。
fn parse_i32(s: &str) -> Result<i32, ()> {
    s.trim().parse::<i32>().map_err(|_| ())
}

fn backend() -> Option<&'static dyn ConsoleBackend> {
    BACKEND.get().copied()
}

/// 后端没初始化时统一回一句，避免每个命令里重复判空。
macro_rules! backend_or_fail {
    () => {
        match backend() {
            Some(b) => b,
            None => {
                println!("控制台后端未初始化");
                return 1;
            }
        }
    };
}

// ---------------------------------------------------------------------------
//  命令实现
//
//  统一用 argv 手工解析：命令都很短，引 argtable 反而更绕（与 C 版一致）。
//  每个处理器都是 `unsafe extern "C"`，因为 IDF 以 C ABI 回调它们。
// ---------------------------------------------------------------------------

/// `cfg` —— 打印当前配置
unsafe extern "C" fn cmd_cfg(_argc: c_int, _argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();
    b.cfg_dump();
    0
}

/// `wifi <ssid> [pass]` —— 改 Wi-Fi 并存 NVS，立即重连
unsafe extern "C" fn cmd_wifi(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供，见 arg() 的契约。
    let Some(ssid) = (unsafe { arg(argc, argv, 1) }) else {
        println!("用法: wifi <ssid> [password]");
        println!("      不写 password = 开放网络");
        println!("      SSID 里有空格时用引号: wifi \"my net\" secret");
        return 1;
    };
    // 允许 `wifi <ssid>` 表示开放网络（空密码）。
    // SAFETY: 同上。
    let pass = unsafe { arg(argc, argv, 2) }.unwrap_or("");

    if let Err(e) = b.set_wifi(ssid, pass) {
        println!("保存失败: {}", err_text(&e));
        return 1;
    }
    // 关键安全点：只回显掩码，绝不打印 pass 明文。
    println!(
        "已保存: ssid='{}' pass={}",
        ssid,
        if pass.is_empty() { "(空)" } else { "***" }
    );
    println!("正在重连…（用 cfg 命令确认，或看日志）");

    // 真正去连：把新配置推给 Wi-Fi 驱动并重连。
    b.wifi_reconnect();
    0
}

/// `txpower <dBm>` —— 改发射功率（2..20），立即生效
unsafe extern "C" fn cmd_txpower(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(raw) = (unsafe { arg(argc, argv, 1) }) else {
        println!(
            "用法: txpower <dBm>   当前 {} dBm（范围 2..20）",
            b.tx_power_dbm()
        );
        return 1;
    };
    let Ok(dbm) = parse_i32(raw) else {
        println!("不是整数: {raw}");
        return 1;
    };
    if !(2..=20).contains(&dbm) {
        println!("超出范围：驱动只支持 2..20 dBm");
        return 1;
    }
    if let Err(e) = b.set_tx_power_dbm(dbm as i8) {
        println!("保存失败: {}", err_text(&e));
        return 1;
    }
    b.wifi_set_tx_power_now(dbm as i8);
    println!("发射功率已设为 {dbm} dBm");
    0
}

/// `pulse <ms>` —— 改默认脉冲宽度
unsafe extern "C" fn cmd_pulse(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(raw) = (unsafe { arg(argc, argv, 1) }) else {
        println!(
            "用法: pulse <ms>   当前 {} ms（范围 {}..{}）",
            b.pulse_ms(),
            PULSE_MS_MIN,
            PULSE_MS_MAX
        );
        return 1;
    };
    let Ok(ms) = parse_i32(raw) else {
        println!("不是整数: {raw}");
        return 1;
    };
    // 先在这里夹一遍，给用户一条明确的信息；后端仍会再夹一次。
    if ms < PULSE_MS_MIN as i32 || ms > PULSE_MS_MAX as i32 {
        println!(
            "超出范围或保存失败：允许 {}..{} ms",
            PULSE_MS_MIN, PULSE_MS_MAX
        );
        return 1;
    }
    if let Err(e) = b.set_pulse_ms(ms as u16) {
        println!(
            "超出范围或保存失败：允许 {}..{} ms",
            PULSE_MS_MIN, PULSE_MS_MAX
        );
        let _ = e;
        return 1;
    }
    println!("默认脉冲宽度已设为 {ms} ms");
    0
}

/// `ttl <秒>` —— 改指令时间窗
unsafe extern "C" fn cmd_ttl(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(raw) = (unsafe { arg(argc, argv, 1) }) else {
        println!("用法: ttl <秒>   当前 {} s（范围 5..300）", b.cmd_ttl());
        return 1;
    };
    let Ok(s) = parse_i32(raw) else {
        println!("不是整数: {raw}");
        return 1;
    };
    if !(5..=300).contains(&s) {
        println!("超出范围（5..300）或保存失败");
        return 1;
    }
    if b.set_cmd_ttl(s as u16).is_err() {
        println!("超出范围（5..300）或保存失败");
        return 1;
    }
    println!("指令时间窗已设为 {s} 秒");
    0
}

/// `status <秒>` —— 改状态上报周期
unsafe extern "C" fn cmd_status(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(raw) = (unsafe { arg(argc, argv, 1) }) else {
        println!(
            "用法: status <秒>   当前 {} s（范围 5..3600）",
            b.status_period_s()
        );
        return 1;
    };
    let Ok(s) = parse_i32(raw) else {
        println!("不是整数: {raw}");
        return 1;
    };
    if !(5..=3600).contains(&s) {
        println!("超出范围（5..3600）或保存失败");
        return 1;
    }
    if b.set_status_period_s(s as u16).is_err() {
        println!("超出范围（5..3600）或保存失败");
        return 1;
    }
    println!("状态上报周期已设为 {s} 秒（下一次循环生效）");
    0
}

/// `keepalive <秒>` —— 改 MQTT keepalive（重连后生效）
unsafe extern "C" fn cmd_keepalive(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(raw) = (unsafe { arg(argc, argv, 1) }) else {
        println!(
            "用法: keepalive <秒>   当前 {} s（范围 10..3600）",
            b.keepalive_s()
        );
        return 1;
    };
    let Ok(s) = parse_i32(raw) else {
        println!("不是整数: {raw}");
        return 1;
    };
    if !(10..=3600).contains(&s) {
        println!("超出范围（10..3600）或保存失败");
        return 1;
    }
    if b.set_keepalive_s(s as u16).is_err() {
        println!("超出范围（10..3600）或保存失败");
        return 1;
    }
    println!("MQTT keepalive 已设为 {s} 秒（重连后生效）");
    0
}

/// `ddns` 的用法串。缺子命令和子命令拼错时都打这一行。
const DDNS_USAGE: &str =
    "用法: ddns show | host <域名> | token <值> | enable | disable | interval <秒> | now";

/// `ddns ...` —— 查看/修改 DDNS 配置，或请求立刻更新一次
///
/// # 为什么这里绝不发 HTTPS
///
/// 命令回调跑在 REPL 任务上。在这里发一个 HTTPS 请求会卡住串口十几秒，
/// 期间任何输入都会被吞掉。所以 `now` 只置位 [`ConsoleBackend::ddns_trigger_now`]，
/// 真正的请求由主循环下一轮发出。
///
/// # token 安全
///
/// `token` 子命令**任何路径都不回显入参** —— 成功只打掩码，失败只打通用错误。
/// 串口日志经常被截屏/粘贴，明文一旦出现就收不回来了。
unsafe extern "C" fn cmd_ddns(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(sub) = (unsafe { arg(argc, argv, 1) }) else {
        println!("{DDNS_USAGE}");
        return 1;
    };

    match sub {
        "show" => {
            println!(
                "ddns.enabled  = {}",
                if b.ddns_enabled() { "是" } else { "否" }
            );

            let host = b.ddns_host();
            println!(
                "ddns.host     = {}",
                if host.is_empty() {
                    "(未设置)"
                } else {
                    &host
                }
            );

            // 只有掩码。trait 上根本没有「读明文」的方法，这是结构性保证。
            println!("ddns.token    = {}", b.ddns_token_masked());
            println!("ddns.interval = {} s", b.ddns_interval_s());

            let last = b.ddns_last_ok();
            println!(
                "ddns.last_ok  = {}",
                if last == 0 {
                    "(从未成功)".to_string()
                } else {
                    last.to_string()
                }
            );
            0
        }

        "host" => {
            // SAFETY: 同上。
            let Some(raw) = (unsafe { arg(argc, argv, 2) }) else {
                println!("用法: ddns host <域名>");
                return 1;
            };
            if let Err(e) = b.set_ddns_host(raw) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            println!("DDNS 主机名已设为 {raw}");
            0
        }

        "token" => {
            // SAFETY: 同上。
            let Some(raw) = (unsafe { arg(argc, argv, 2) }) else {
                println!("用法: ddns token <值>");
                return 1;
            };
            if let Err(e) = b.set_ddns_token(raw) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            // 只打掩码：入参 raw 出了这个 match 分支就再也不会被打印。
            println!("DDNS token 已保存: {}", crate::ddns::mask_token(raw));
            0
        }

        "enable" | "disable" => {
            let on = sub == "enable";
            if let Err(e) = b.set_ddns_enabled(on) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            println!("DDNS 已{}", if on { "启用" } else { "停用" });
            0
        }

        "interval" => {
            let cur = b.ddns_interval_s();
            // SAFETY: 同上。
            let Some(raw) = (unsafe { arg(argc, argv, 2) }) else {
                println!(
                    "用法: ddns interval <秒>   当前 {cur} s（范围 {}..{}）",
                    crate::consts::DDNS_INTERVAL_MIN_S,
                    crate::consts::DDNS_INTERVAL_MAX_S
                );
                return 1;
            };
            let Ok(s) = parse_i32(raw) else {
                println!("不是整数: {raw}");
                return 1;
            };
            if s < crate::consts::DDNS_INTERVAL_MIN_S as i32
                || s > crate::consts::DDNS_INTERVAL_MAX_S as i32
            {
                println!(
                    "超出范围 {}..{}（当前 {cur} s）",
                    crate::consts::DDNS_INTERVAL_MIN_S,
                    crate::consts::DDNS_INTERVAL_MAX_S
                );
                return 1;
            }
            if let Err(e) = b.set_ddns_interval_s(s as u32) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            // 执行侧还有一道 60s 硬下限（ddns::clamp_interval_s），这里只按配置范围校验。
            println!("DDNS 更新周期已设为 {s} 秒");
            0
        }

        "now" => {
            // 只置位，不发请求。理由见函数头注释。
            b.ddns_trigger_now();
            println!("已触发，等待下次调度");
            0
        }

        _ => {
            println!("未知子命令: {sub}");
            println!("{DDNS_USAGE}");
            1
        }
    }
}

/// `mq` 的用法串。缺子命令和子命令拼错时都打这一行。
const MQ_USAGE: &str =
    "用法: mq show | host <主机名> | user <用户名> | pass <密码> | enable | disable | refresh";

/// `mq ...` —— 查看/修改 MQTT 凭据与开关
///
/// # 为什么这里绝不建连
///
/// 命令回调跑在 REPL 任务上，建 MQTT 客户端会阻塞到 TCP + TLS 握手完成，
/// 最坏十几秒，串口命令通道会被整个卡死。这里只落 NVS；客户端（Phase 5）
/// 在启动时读配置，所以改完要 `reboot` 才生效。
///
/// 端口与 scheme 不在此处可改：固定 `mqtts://` + 8883，避免被降级成明文。
unsafe extern "C" fn cmd_mq(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(sub) = (unsafe { arg(argc, argv, 1) }) else {
        println!("{MQ_USAGE}");
        return 1;
    };

    match sub {
        "show" => {
            println!(
                "mq.enabled = {}",
                if b.mqtt_enabled() { "是" } else { "否" }
            );

            let host = b.mqtt_host();
            println!(
                "mq.host    = {}",
                if host.is_empty() {
                    "(未设置)"
                } else {
                    &host
                }
            );
            // scheme 与端口不可配，打出来是为了让「为什么填不了端口」一眼可见。
            println!(
                "mq.port    = {} (固定 mqtts，不可配)",
                crate::consts::MQTT_PORT
            );

            let user = b.mqtt_user();
            println!(
                "mq.user    = {}",
                if user.is_empty() {
                    "(未设置)"
                } else {
                    &user
                }
            );

            // 只有掩码。trait 上根本没有「读明文」的方法，这是结构性保证。
            println!("mq.pass    = {}", b.mqtt_pass_masked());
            println!("dev.id     = {}", b.dev_id());

            // 连接状态是「运行时」而非「配置」，单独一行，让人一眼分清
            // 「配了但连不上」和「没配」。
            println!(
                "mq.link    = {}",
                match b.mqtt_online() {
                    Some(true) => "已连接",
                    Some(false) => "已启动，断开中（自动重连）",
                    None => "客户端未启动（未启用或初始化失败）",
                }
            );
            0
        }

        "host" => {
            // SAFETY: 同上。
            let Some(raw) = (unsafe { arg(argc, argv, 2) }) else {
                println!("用法: mq host <主机名>    （只填主机名，端口固定 8883）");
                return 1;
            };
            if let Err(e) = b.set_mqtt_host(raw) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            println!("MQTT 主机已设为 {raw}（重启后生效）");
            0
        }

        "user" => {
            // SAFETY: 同上。
            let Some(raw) = (unsafe { arg(argc, argv, 2) }) else {
                println!("用法: mq user <用户名>");
                return 1;
            };
            if let Err(e) = b.set_mqtt_user(raw) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            println!("MQTT 用户名已设为 {raw}（重启后生效）");
            0
        }

        "pass" => {
            // SAFETY: 同上。
            let Some(raw) = (unsafe { arg(argc, argv, 2) }) else {
                println!("用法: mq pass <密码>");
                return 1;
            };
            if let Err(e) = b.set_mqtt_pass(raw) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            // 只打掩码：入参 raw 出了这个分支就再也不会被打印。
            println!(
                "MQTT 密码已保存: {}（重启后生效）",
                crate::ddns::mask_token(raw)
            );
            0
        }

        "enable" | "disable" => {
            let on = sub == "enable";
            if let Err(e) = b.set_mqtt_enabled(on) {
                println!("保存失败: {}", err_text(&e));
                return 1;
            }
            println!("MQTT 已{}（重启后生效）", if on { "启用" } else { "停用" });
            0
        }

        // 只是往 mqtt 线程的队列里投一个请求，不等结果 —— 真正的 publish
        // 发生在 mqtt 线程里（`state` 是 retained 的，重发一次让上位机立刻看到当前状态）。
        "refresh" => {
            if b.mqtt_refresh_state() {
                println!("已请求重发 PC 状态");
                0
            } else {
                println!("请求未投递：MQTT 客户端未启动或队列已满");
                1
            }
        }

        _ => {
            println!("未知子命令: {sub}");
            println!("{MQ_USAGE}");
            1
        }
    }
}

/// `devid [新ID]` —— 查看/修改设备 ID
///
/// 设备 ID 决定 topic 前缀（`psw/<id>/...`）与 client_id 前缀，
/// 改完需重启：已建连的客户端仍订阅着旧 topic。
unsafe extern "C" fn cmd_devid(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    // SAFETY: argc/argv 由 IDF 提供。
    let Some(raw) = (unsafe { arg(argc, argv, 1) }) else {
        println!("dev.id = {}", b.dev_id());
        println!("用法: devid <ASCII 字母数字下划线短横线>");
        return 1;
    };
    if let Err(e) = b.set_dev_id(raw) {
        println!("保存失败: {}", err_text(&e));
        return 1;
    }
    println!("设备 ID 已设为 {raw}（重启后生效，topic 前缀随之改变）");
    0
}

/// `reset` —— 清空 NVS，恢复编译期默认值
unsafe extern "C" fn cmd_reset(_argc: c_int, _argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();
    if let Err(e) = b.cfg_reset() {
        println!("复位失败: {}", err_text(&e));
        return 1;
    }
    println!("已清空 NVS，恢复编译期默认值。");
    println!("Wi-Fi 需要重连才生效，敲 wifi 命令或 reboot。");
    b.cfg_dump();
    0
}

/// `scan` —— 立刻扫描一次 Wi-Fi
unsafe extern "C" fn cmd_scan(_argc: c_int, _argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();
    println!("扫描中（约 2 秒）…");
    b.wifi_scan_now();
    0
}

/// `press [ms]` —— 手动触发一次电源键脉冲
///
/// 用于不接 MQTT 也能验证硬件。**走和 MQTT 完全相同的那条队列**，
/// 不绕过脉冲任务 —— 这是本模块的安全底线。
unsafe extern "C" fn cmd_press(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let b = backend_or_fail!();

    let mut width = b.pulse_ms();
    // SAFETY: argc/argv 由 IDF 提供。
    if let Some(raw) = unsafe { arg(argc, argv, 1) } {
        let Ok(ms) = parse_i32(raw) else {
            println!("不是整数: {raw}");
            return 1;
        };
        if ms < PULSE_MS_MIN as i32 || ms > PULSE_MS_MAX as i32 {
            println!("宽度超出安全范围 {}..{} ms", PULSE_MS_MIN, PULSE_MS_MAX);
            return 1;
        }
        width = ms as u16;
    }

    // 唯一一条通往 GPIO 的路径，且是异步投递。
    match b.pulse_request(width) {
        Ok(()) => {
            println!("已投递脉冲 {width} ms");
            0
        }
        Err(CmdError::Busy) => {
            println!("忙：上一次脉冲还没结束");
            1
        }
        Err(e) => {
            println!("投递失败: {}", err_text(&e));
            1
        }
    }
}

/// `reboot` —— 软重启
unsafe extern "C" fn cmd_reboot(_argc: c_int, _argv: *mut *mut c_char) -> c_int {
    println!("重启中…");
    // 给 stdout 一点时间把上面那行送出去，否则串口刚看到就断了。
    // （对应 C 版的 fflush(stdout)）
    let _ = std::io::stdout().flush();

    // SAFETY: esp_restart 是 IDF 的进程级重启原语，无参数、无前置条件，
    // 可以从任意任务调用。它不返回。
    unsafe { sys::esp_restart() }
}

fn err_text(e: &CmdError) -> String {
    match e {
        CmdError::Busy => "忙".to_string(),
        CmdError::Other(s) => s.clone(),
    }
}

// ---------------------------------------------------------------------------
//  注册表
// ---------------------------------------------------------------------------

type Handler = unsafe extern "C" fn(c_int, *mut *mut c_char) -> c_int;

struct CmdDef {
    name: &'static str,
    help: &'static str,
    func: Handler,
}

/// 命令表。顺序与 C 版 `register_all()` 一致。
///
/// 注意：C 版头注释里提到的 `save` **不是一个命令** —— 它只是提示
/// 「各命令已自带保存」。所以这里没有 `save`，以免凭空多出一条命令。
static COMMANDS: &[CmdDef] = &[
    CmdDef {
        name: "cfg",
        help: "打印当前配置",
        func: cmd_cfg,
    },
    CmdDef {
        name: "wifi",
        help: "改 Wi-Fi: wifi <ssid> [pass]",
        func: cmd_wifi,
    },
    CmdDef {
        name: "txpower",
        help: "改发射功率: txpower <dBm>",
        func: cmd_txpower,
    },
    CmdDef {
        name: "pulse",
        help: "改脉冲宽度: pulse <ms>",
        func: cmd_pulse,
    },
    CmdDef {
        name: "ttl",
        help: "改指令时间窗: ttl <秒>",
        func: cmd_ttl,
    },
    CmdDef {
        name: "status",
        help: "改状态上报周期: status <秒>",
        func: cmd_status,
    },
    CmdDef {
        name: "keepalive",
        help: "改 MQTT keepalive: keepalive <秒>",
        func: cmd_keepalive,
    },
    CmdDef {
        name: "ddns",
        help: "DDNS: ddns show|host|token|enable|disable|interval|now",
        func: cmd_ddns,
    },
    // 以下两条是 Rust 版新增（C 版没有 MQTT）。放在 `ddns` 之后，
    // 让配置类命令聚在一起；C 版顺序比对只覆盖它原有的那 12 条。
    CmdDef {
        name: "mq",
        help: "MQTT: mq show|host|user|pass|enable|disable|refresh",
        func: cmd_mq,
    },
    CmdDef {
        name: "devid",
        help: "设备 ID: devid [新ID]（决定 topic 前缀）",
        func: cmd_devid,
    },
    CmdDef {
        name: "scan",
        help: "立刻扫描一次 Wi-Fi",
        func: cmd_scan,
    },
    CmdDef {
        name: "press",
        help: "手动触发一次电源键脉冲",
        func: cmd_press,
    },
    CmdDef {
        name: "reset",
        help: "清空 NVS 恢复默认配置",
        func: cmd_reset,
    },
    CmdDef {
        name: "reboot",
        help: "软重启",
        func: cmd_reboot,
    },
];

/// 注册所有命令 + help。
fn register_all() -> Result<(), EspError> {
    for def in COMMANDS {
        let mut cmd = MaybeUninit::<sys::esp_console_cmd_t>::zeroed();
        // SAFETY: esp_console_cmd_t 的所有字段都是可空指针或函数指针，
        // 全零是合法初值（func/func_w_context 为零 = 都没有设置）。
        let cmd = unsafe { cmd.assume_init_mut() };

        cmd.command = leak_cstr(def.name);
        cmd.help = leak_cstr(def.help);
        cmd.hint = core::ptr::null();
        cmd.func = Some(def.func);
        cmd.argtable = core::ptr::null_mut();
        cmd.func_w_context = None;
        cmd.context = core::ptr::null_mut();

        // SAFETY: cmd 指向一个已填满的栈上结构体。IDF 文档明确说
        // `cmd` 可以是临时值 —— 它只借用结构体本身，但结构体里指向的
        // command/help 字符串必须活到 deinit；我们用 leak_cstr 保证这点。
        let err = unsafe { sys::esp_console_cmd_register(cmd) };
        if err != sys::ESP_OK {
            error!("注册命令 {} 失败: {err}", def.name);
            return Err(EspError::from(err).expect("err 已判定非 ESP_OK，不可能为 None"));
        }
    }

    // SAFETY: 无参数，且此刻 console 已由 new_repl 初始化完成。
    let err = unsafe { sys::esp_console_register_help_command() };
    if err != sys::ESP_OK {
        error!("注册 help 命令失败: {err}");
        return Err(EspError::from(err).expect("err 已判定非 ESP_OK，不可能为 None"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
//  启动控制台
// ---------------------------------------------------------------------------

/// 启动串口控制台。**应在 `nvs_flash_init()` / `cfg_init()` 之后调用**，
/// 因为命令处理器会立刻读写 NVS 配置。
///
/// `backend` 必须活到进程结束（传 `Box::leak` 出来的引用即可）。
///
/// 返回 `Err` 时控制台不可用，但按 C 版的语义，**其余功能不受影响** ——
/// 调用方应当只打日志、不要因此 abort。
///
/// # 为什么必须用 USB Serial/JTAG 后端
///
/// 这块板子的 COM11 是芯片自带的 USB Serial/JTAG（VID 303A:PID 1001），
/// 不是 USB-UART 桥。而 USB-JTAG 只有在被设为「主控制台」时才有输入；
/// 作为「副控制台」时按 IDF 文档只支持单向输出、不支持 REPL。
/// 所以 `sdkconfig.defaults` 里必须：
/// ```text
///     CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y     (primary)
///     CONFIG_ESP_CONSOLE_UART_DEFAULT 关掉
///     CONFIG_ESP_CONSOLE_SECONDARY_NONE=y
/// ```
pub fn init(backend: &'static dyn ConsoleBackend) -> Result<(), EspError> {
    if BACKEND.set(backend).is_err() {
        // 重复初始化：不致命，但也别把 REPL 再起一遍。
        error!("控制台已初始化过，忽略重复调用");
        return Ok(());
    }

    // ---- REPL 配置（对应 ESP_CONSOLE_REPL_CONFIG_DEFAULT + C 版的两处改动）----
    let mut repl_cfg = MaybeUninit::<sys::esp_console_repl_config_t>::zeroed();
    // SAFETY: 该结构体全字段都是整数/可空指针，全零是合法初值；
    // 下面把每一项都显式赋成有意义的值，不留零值语义的歧义。
    let repl_cfg = unsafe { repl_cfg.assume_init_mut() };
    repl_cfg.max_history_len = 16; // C 版: repl_cfg.max_history_len = 16
    repl_cfg.history_save_path = core::ptr::null(); // 不落盘
    repl_cfg.task_stack_size = 4096;
    repl_cfg.task_priority = 2;
    repl_cfg.task_core_id = TSK_NO_AFFINITY;
    repl_cfg.prompt = leak_cstr("psw>"); // C 版: repl_cfg.prompt = "psw>"
    repl_cfg.max_cmdline_length = 0; // 0 = 用默认值
    repl_cfg.max_cmdline_args = 0; // 0 = 用默认值

    // ---- 用 stdio 版 REPL ----
    // 为什么不是 esp_console_new_repl_usb_serial_jtag：
    //   该函数在 IDF 头文件里被 `#if CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG` 包着
    //   （esp_console.h:387），而 esp-idf-sys 的 bindgen 没把它导出来 ——
    //   bindings.rs 里只有 new_repl_uart / new_repl_stdio，连
    //   dev_usb_serial_jtag_config_t 这个类型都缺。硬调就是 E0425。
    //
    // esp_console_new_repl_stdio 是 IDF 6.x 的等价入口：它跟随当前**活动的**
    // console 驱动，而 sdkconfig 已把主 console 设成 USB Serial/JTAG
    // （CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y、SECONDARY_NONE=y），
    // 所以行为与 C 版一致，且不依赖那个没导出的符号。
    let mut repl: *mut sys::esp_console_repl_t = core::ptr::null_mut();

    // SAFETY: repl_cfg 指向本次调用期间存活的栈上结构体，IDF 只在调用期间
    // 读它；&mut repl 是合法的出参。
    let err = unsafe { sys::esp_console_new_repl_stdio(repl_cfg, &mut repl) };
    if err != sys::ESP_OK || repl.is_null() {
        error!("控制台初始化失败: {err}（串口配置命令将不可用，其余功能不受影响）");
        // ⚠️ 这里的 err 可能是 ESP_OK 而 repl 仍是空指针（走了 `||` 的右半边）。
        // 此时 `EspError::from(0)` 返回 None，直接 .expect() 会 panic ——
        // 而本函数的契约是「控制台起不来也不许拖垮固件」。所以用 ESP_FAIL 兜底，
        // 保证这条路径永远返回 Err 而不是 panic。
        let code = if err != sys::ESP_OK {
            err
        } else {
            sys::ESP_FAIL
        };
        return Err(EspError::from(code).expect("code 已保证非 0"));
    }

    // 把句柄钉在 static 上，确保 REPL 存活到进程结束（见 REPL 的说明）。
    let _ = REPL.set(repl as usize);

    // 先建 REPL（它会初始化 console 模块），再注册命令，最后启动循环。
    // 顺序必须和 C 版一致：注册命令要求 console 已 init。
    register_all()?;

    // SAFETY: repl 由 new_repl 返回且非空，已在上面的分支里检查过。
    let err = unsafe { sys::esp_console_start_repl(repl) };
    if err != sys::ESP_OK {
        error!("控制台启动失败: {err}");
        return Err(EspError::from(err).expect("err 已判定非 ESP_OK，不可能为 None"));
    }

    info!("串口控制台已就绪 —— 敲 help 看命令");
    Ok(())
}

/// REPL 是否已经成功启动过。
/// 给 `main.rs` 做断言/日志用，也方便测试。
#[allow(dead_code)]
pub fn is_running() -> bool {
    REPL.get().is_some()
}
