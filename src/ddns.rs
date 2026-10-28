//! DDNS —— 把本机当前公网 IP 推送到 DNS 记录
//!
//! 对应 `plans/rust-rewrite.md` §9。与 C 版一样，本模块只做一件事：
//! **改写某条已存在记录的 IP 内容**。不改记录名、不改类型、不改 TTL、
//! 不碰同一域名下的其他记录 —— DNSHE 的 DDNS token 权限本来也只到这里。
//!
//! # 设计要点
//!
//! * **provider / 传输分离**：`DdnsProvider` 只做「构造请求」和「解释响应」，
//!   这两件事是纯函数，能在主机上跑单元测试，不烧 flash。
//!   真正的 HTTPS 调用在 `esp_impl` 里，是一层薄壳。
//! * **token 绝不进日志**：`HttpRequest` 手写了 `Debug`，会把
//!   `Authorization` / `X-DDNS-Token` 的值掩码后再打印。任何
//!   `debug!("{req:?}")` 的调用点都自动安全，不依赖调用者自觉。
//! * **token 绝不进 URL / body**：只在 header 里。`build_request` 里有一道
//!   结构性检查（`assert_token_absent`），万一以后有人改错，会直接报错
//!   而不是把凭据发出去。
//! * **不解析自己的域名**：只推送 IP，不做 DNS 查询，规避 Split-DNS 封禁陷阱。
//! * **IPv6 双重默认关闭**：编译期 `DDNS_IPV6_COMPILE_ENABLE` 与运行期
//!   `DnsheProvider::enable_ipv6` 必须同时为 true 才可用。
//!   Espressif 默认关掉 `CONFIG_LWIP_IPV6_AUTOCONFIG` 是有道理的 ——
//!   开了设备会拿到全局 v6 地址，在用户不知情的情况下可被公网直连。
//! * **不依赖 serde_json**：主 crate 的 `Cargo.toml` 里没有它，所以
//!   JSON body 是手工拼的（IP 地址只含十六进制字符、`.` 和 `:`，
//!   没有任何需要转义的字符），响应也是手工扫描的。

#![allow(dead_code)]

use core::fmt;
use std::net::IpAddr;

// ============================================================================
// 常量
// ============================================================================

/// DNSHE 的 DDNS 更新端点。**不带任何参数** —— token 绑定哪条记录，
/// 这个端点就改哪条记录。
pub const DNSHE_UPDATE_URL: &str =
    "https://api005.dnshe.com/index.php?m=domain_hub&endpoint=ddns&action=update";

/// 客户端硬下限：不管配置里写多少，两次更新之间至少隔 60 秒。
/// 服务端自己也有 60 秒的限制，这里再夹一道，避免把自己打成 abuse。
pub const DDNS_MIN_INTERVAL_S: u64 = 60;

/// 5xx 之后至少退避这么久。需求是「至少 10 分钟」。
pub const DDNS_5XX_BACKOFF_S: u64 = 600;

/// 429（限流）之后的退避。
pub const DDNS_429_BACKOFF_S: u64 = 900;

/// 单次 HTTPS 请求的超时。
pub const DDNS_HTTP_TIMEOUT_S: u64 = 15;

/// 响应体最多读这么多字节，防止对端抽风把内存吃光。
pub const DDNS_MAX_BODY_BYTES: usize = 1024;

/// **编译期总闸**：IPv6 / AAAA 支持。默认关。
///
/// 想打开必须同时满足两个条件：把这里改成 `true`，**并且**在运行期
/// `DnsheProvider::with_ipv6(true)`。任何一个是 false，AAAA 更新都会被拒绝。
/// 双闸门是故意的 —— 单靠运行期配置，一次 NVS 误写就可能让设备暴露在公网 v6 上。
pub const DDNS_IPV6_COMPILE_ENABLE: bool = false;

// ============================================================================
// 基本类型
// ============================================================================

/// DNS 记录类型。目前只关心 A / AAAA —— 别的一律不碰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordType {
    A,
    Aaaa,
}

impl RecordType {
    pub const fn as_str(self) -> &'static str {
        match self {
            RecordType::A => "A",
            RecordType::Aaaa => "AAAA",
        }
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条要更新的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdnsRecord {
    /// 目标记录类型。
    pub record: RecordType,
    /// 想推送的 IP。`None` = 让服务端用请求来源 IP 推断。
    ///
    /// 只有 A 记录允许 `None`；AAAA 记录必须显式给地址（见 `build_request`）。
    pub ip: Option<IpAddr>,
}

impl DdnsRecord {
    /// A 记录、blind push（服务端认来源 IP）。
    pub fn blind_v4() -> Self {
        Self {
            record: RecordType::A,
            ip: None,
        }
    }

    /// A 记录、显式推送指定 IPv4。
    pub fn explicit_v4(ip: std::net::Ipv4Addr) -> Self {
        Self {
            record: RecordType::A,
            ip: Some(IpAddr::V4(ip)),
        }
    }

    /// AAAA 记录、显式推送指定 IPv6。
    pub fn explicit_v6(ip: std::net::Ipv6Addr) -> Self {
        Self {
            record: RecordType::Aaaa,
            ip: Some(IpAddr::V6(ip)),
        }
    }
}

/// 一个已经构造好的 HTTP 请求 —— 纯数据，不含任何 esp 类型。
///
/// `Debug` 是手写的，会掩码凭据 header。**不要**给它加 `#[derive(Debug)]`。
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 只在这一处做掩码 —— 所有打印路径都汇到这里，就不会漏。
        let headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), mask_header_value(k, v)))
            .collect();

        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &self.body)
            .finish()
    }
}

/// 一次更新的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// `good` —— DNS 记录真的被改写了。
    Updated { ip: Option<IpAddr> },
    /// `nochg` —— 记录本来就是这个 IP，服务端没写 DNS。
    /// 算成功（不是错误），但**不是一次变更**。
    NoChange { ip: Option<IpAddr> },
}

impl UpdateOutcome {
    /// 这次调用是否真的产生了 DNS 写操作？
    ///
    /// 用来决定要不要打一条「已更新」的日志 —— `nochg` 每次都打会淹掉日志。
    pub fn is_change(self) -> bool {
        matches!(self, UpdateOutcome::Updated { .. })
    }

    pub fn ip(self) -> Option<IpAddr> {
        match self {
            UpdateOutcome::Updated { ip } | UpdateOutcome::NoChange { ip } => ip,
        }
    }
}

/// 更新失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdnsError {
    /// **配置错误**：token 不对、没权限、记录不存在。
    /// 重试一万次也不会好，必须停下来并且大声报出来。
    FatalConfig { status: u16, detail: String },

    /// 服务端 5xx。稍后重试，退避 >= 10 分钟。
    ServerError { status: u16, retry_after_s: u64 },

    /// 其他可重试的状态码（429 等）。
    Transient { status: u16, retry_after_s: u64 },

    /// **服务端明确拒绝了这次更新**：HTTP 2xx，但 JSON body 里带非空的
    /// `error` 字段（如 `{"error":"notgood"}`）。
    ///
    /// 单独一个变体，是为了和 `Protocol`（我们看不懂响应）区分开：
    /// 这里是「对端明说了不行」，日志要打出来、要重试，但绝不能记成成功。
    Rejected { code: String },

    /// 拿到了 2xx，但响应体看不懂。可能是对端改协议了 —— 冷却后再试。
    Protocol(String),

    /// 本地就没构造出请求（v6 被禁用、IP 家族对不上、HTTP 栈报错）。
    /// 重试没有意义，要改的是本地配置/代码。
    Local(String),
}

impl DdnsError {
    /// 是否应当**彻底停止**重试（而不是退避后再来）。
    pub fn is_fatal(&self) -> bool {
        matches!(self, DdnsError::FatalConfig { .. } | DdnsError::Local(_))
    }

    /// 建议的重试间隔（秒）。`None` = 别重试了。
    ///
    /// 返回的数值已经夹过下限，调用者可以直接用。
    pub fn retry_after_s(&self) -> Option<u64> {
        match self {
            // 配置错 / 本地错：重试无用
            DdnsError::FatalConfig { .. } => None,
            DdnsError::Local(_) => None,

            // 5xx：至少 10 分钟，且尊重服务端给的更长的值
            DdnsError::ServerError { retry_after_s, .. } => {
                Some((*retry_after_s).max(DDNS_5XX_BACKOFF_S))
            }

            // 429 之类：至少 60 秒
            DdnsError::Transient { retry_after_s, .. } => {
                Some((*retry_after_s).max(DDNS_MIN_INTERVAL_S))
            }

            // 看不懂响应：按 5xx 同等待遇，别去 hammer 对端
            DdnsError::Protocol(_) => Some(DDNS_5XX_BACKOFF_S),

            // 对端明说不行：可重试，但同样要冷却，别去 hammer 对端
            DdnsError::Rejected { .. } => Some(DDNS_5XX_BACKOFF_S),
        }
    }

    /// 给日志用的一句话。**不含 token**（构造时就不该把 token 放进来）。
    pub fn summary(&self) -> String {
        match self {
            DdnsError::FatalConfig { status, detail } => {
                format!("FATAL 配置错误 HTTP {status} ({detail}) —— 停止重试，请检查 token / 记录")
            }
            DdnsError::ServerError {
                status,
                retry_after_s,
            } => {
                format!("服务端错误 HTTP {status}，{retry_after_s}s 后重试")
            }
            DdnsError::Transient {
                status,
                retry_after_s,
            } => {
                format!("临时错误 HTTP {status}，{retry_after_s}s 后重试")
            }
            DdnsError::Rejected { code } => {
                format!("服务端拒绝更新：{}", truncate(code, 64))
            }
            DdnsError::Protocol(msg) => format!("响应无法解析：{msg}"),
            DdnsError::Local(msg) => format!("本地错误：{msg}"),
        }
    }
}

impl fmt::Display for DdnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

/// 下一次更新该等多久。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextDelay {
    /// 一切正常，按配置的常规周期（已夹到 >= 60s）。
    Scheduled(u64),
    /// 失败，稍后重试，至少这么多秒。
    Retry(u64),
    /// 停止。需要人工介入。
    Stop,
}

impl NextDelay {
    pub fn seconds(self) -> Option<u64> {
        match self {
            NextDelay::Scheduled(s) | NextDelay::Retry(s) => Some(s),
            NextDelay::Stop => None,
        }
    }
}

/// 根据上一次的结果决定下一次更新要等多久。
///
/// 这是纯函数，主机上可测。
pub fn next_delay_s(outcome: &Result<UpdateOutcome, DdnsError>, configured_s: u64) -> NextDelay {
    match outcome {
        Ok(_) => NextDelay::Scheduled(clamp_interval_s(configured_s)),
        Err(e) => match e.retry_after_s() {
            None => NextDelay::Stop,
            Some(s) => NextDelay::Retry(s.max(DDNS_MIN_INTERVAL_S)),
        },
    }
}

/// 把配置里的周期夹进合法范围。
///
/// **这是 60 秒硬下限的唯一执行点** —— 任何拿到周期的路径都应该过这个函数。
pub fn clamp_interval_s(configured_s: u64) -> u64 {
    configured_s.max(DDNS_MIN_INTERVAL_S)
}

// ============================================================================
// 凭据掩码 —— 安全关键
// ============================================================================

/// 掩码一个凭据：最多保留前 8 个字符。
///
/// 8 个字符足够在日志里区分「换没换 token」，又远不足以还原。
pub fn mask_token(token: &str) -> String {
    if token.is_empty() {
        return "(unset)".to_string();
    }
    let head: String = token.chars().take(8).collect();
    format!("{head}***")
}

/// 这个 header 的值是不是凭据？
fn is_sensitive_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("x-ddns-token")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("x-auth-token")
}

/// 掩码一个 header 的值。保留 `Bearer` 这类 scheme 前缀，只掩凭据本体。
fn mask_header_value(name: &str, value: &str) -> String {
    if !is_sensitive_header(name) {
        return value.to_string();
    }
    match value.split_once(' ') {
        Some((scheme, cred))
            if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token") =>
        {
            format!("{scheme} {}", mask_token(cred))
        }
        _ => mask_token(value),
    }
}

/// 断言凭据没有混进这个字符串。混进去了就返回 `Local` 错误 —— 宁可发不出去，
/// 也不能把 token 写进 URL / body。
fn assert_token_absent(haystack: &str, token: &str) -> Result<(), DdnsError> {
    if token.is_empty() || !haystack.contains(token) {
        Ok(())
    } else {
        Err(DdnsError::Local(
            "拒绝发送：凭据出现在了 URL 或 body 里（只允许走 header）".to_string(),
        ))
    }
}

/// 一条可以安全写进日志的请求摘要。**已掩码**。
pub fn redacted_request_line(req: &HttpRequest) -> String {
    format!(
        "{} {} ({} headers, body {} bytes)",
        req.method,
        req.url,
        req.headers.len(),
        req.body.as_deref().map(str::len).unwrap_or(0)
    )
}

// ============================================================================
// provider trait
// ============================================================================

/// 一个 DDNS 服务商。
///
/// 只负责「构造请求」和「解释响应」—— 都是纯函数。
/// 真正的网络 I/O 在调用方（`DdnsClient` + `esp_impl`）。
///
/// 加 Cloudflare 只要再写一个 impl，调用方一行都不用改。
pub trait DdnsProvider: Send + Sync {
    /// 用于日志的服务商名字。
    fn name(&self) -> &'static str;

    /// 服务端能不能自己推断来源 IP？（能的话 v4 可以不传 IP）
    fn ip_optional(&self) -> bool;

    /// 是否支持 AAAA / IPv6。**必须把编译期与运行期两道闸门都算进去。**
    fn supports_v6(&self) -> bool;

    /// 构造更新请求。
    ///
    /// `ip` 覆盖 `rec.ip`（调用方刚探测到的地址可以直接传进来，
    /// 不用回头改 record）。
    fn build_request(
        &self,
        rec: &DdnsRecord,
        ip: Option<&IpAddr>,
    ) -> Result<HttpRequest, DdnsError>;

    /// 解释响应。
    fn parse_response(&self, status: u16, body: &str) -> Result<UpdateOutcome, DdnsError>;
}

// ============================================================================
// DNSHE
// ============================================================================

/// DNSHE（`lyg.ddns.ge`）的 DDNS provider。
///
/// 协议事实：
/// * 端点固定为 [`DNSHE_UPDATE_URL`]，**不带参数** —— token 决定改哪条记录。
/// * 认证走自定义 header `X-DDNS-Token: <token>`，值是裸 token，**没有**
///   `Bearer` 前缀（那是 Cloudflare 的写法，两边不能混用）。
///   token **绝不**进 URL、**绝不**进 body。
/// * 不传 IP 时服务端用请求来源 IP 推断（所以 v4 可以 blind push）。
/// * AAAA 必须显式在 JSON body 里给 IP `{"ip":"..."}`，不能用来源推断。
/// * `nochg` = 记录已经是这个 IP，没有发生 DNS 写入。
/// * 401 / 403 / 404 = 配置错误，停止重试。
/// * 5xx = 至少退避 10 分钟。
pub struct DnsheProvider {
    /// **绝不打印。** 只能通过 `token_masked()` 外泄（且已掩码）。
    token: String,
    /// 记录名，仅用于日志。请求里不带它 —— token 已经绑定了记录。
    hostname: String,
    /// 运行期 IPv6 开关。默认 `false`。
    enable_ipv6: bool,
}

impl DnsheProvider {
    /// 构造。**IPv6 默认关闭** —— 要开得再调 `with_ipv6(true)`，
    /// 且编译期 `DDNS_IPV6_COMPILE_ENABLE` 也必须是 `true`。
    pub fn new(token: impl Into<String>, hostname: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            hostname: hostname.into(),
            enable_ipv6: false,
        }
    }

    /// 运行期打开 / 关闭 AAAA 支持。默认关。
    pub fn with_ipv6(mut self, on: bool) -> Self {
        self.enable_ipv6 = on;
        self
    }

    /// 记录名，给日志用。
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// **唯一**允许外泄 token 信息的入口，且已经掩码。
    pub fn token_masked(&self) -> String {
        mask_token(&self.token)
    }
}

/// DNSHE 的更新端点。
pub fn dnshe_update_url() -> String {
    DNSHE_UPDATE_URL.to_string()
}

/// 解析 dyndns2 风格的响应体。
///
/// 正常形态是 `<code> [ip]`，比如 `good 1.2.3.4` / `nochg 1.2.3.4`。
///
/// 兜底：如果第一个 token 不认识，而 body 看起来像 JSON / XML
/// （`{` 或 `<` 开头），就在里面扫那几个特征串。DNSHE 的端点叫
/// `index.php?...&action=update`，有包一层 JSON 的可能，所以这里宽进严出。
/// 没有用 serde_json —— 主 crate 里没有这个依赖。
pub fn parse_ddns2_body(body: &str) -> Result<UpdateOutcome, DdnsError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(DdnsError::Protocol("响应体为空".to_string()));
    }

    // body 里出现的第一个可解析地址，就是服务端认定的当前 IP（可能没有）
    let ip_in_body = trimmed
        .split(|c: char| c.is_whitespace() || c == '"' || c == ',' || c == ':')
        .find_map(|tok| tok.parse::<IpAddr>().ok());

    let first = trimmed
        .split(|c: char| c.is_whitespace() || c == '"' || c == '{' || c == ',' || c == ':')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_ascii_lowercase();

    // JSON 形态下，`error` 字段的优先级高于一切状态码扫描。
    //
    // 这是「对端明说了不行」和「我们看不懂」的分界点：
    //   - 有非空 error 字段 -> Rejected，可重试但要冷却；
    //   - 没有 error 字段   -> 落到下面的状态码扫描。
    // 对端明说不行时，body 里哪怕同时夹着 good 也绝不能算成功 ——
    // 宁可漏一次更新，不可假成功。
    if trimmed.starts_with('{') {
        if let Some(err_code) = find_json_error_field(trimmed) {
            return Err(DdnsError::Rejected { code: err_code });
        }
    }

    let code = if is_known_code(&first) {
        first
    } else if trimmed.starts_with('{') || trimmed.starts_with('<') {
        // 兜底扫描。
        //
        // ⚠️ 这里曾经写成 `KNOWN_CODES.iter().find(|c| lower.contains(**c))` ——
        // 裸子串匹配。后果是 {"error":"notgood"} 里的 "good" 会被当成成功，
        // 含 "good" 一词的 HTML 门户页、错误页同理，畸形响应变成假成功。
        // 这是整个解析链上唯一会 fail-open 的地方，而它恰好在 2xx 路径上，
        // 一旦命中就会把「实际没更新」记成 Updated，还可能把错误页里的
        // 任意 IP 当成本机地址写进日志。
        //
        // 现在两条硬规则：
        //   1. 只认「独立 token」形态的 code —— 前后必须是非字母数字边界
        //      （引号 / 空白 / 冒号 / 逗号 / 尖括号 / = 都算边界）。
        //      "good"、<good>、code=good 命中；notgood、goodness 不命中。
        //   2. 失败码优先于成功码。body 里同时出现 good 和 badauth 时按失败
        //      处理 —— 宁可漏一次更新，不可假成功。
        find_delimited_code(trimmed).ok_or_else(|| {
            DdnsError::Protocol(format!("无法识别的响应：{}", truncate(trimmed, 120)))
        })?
    } else {
        return Err(DdnsError::Protocol(format!(
            "无法识别的响应：{}",
            truncate(trimmed, 120)
        )));
    };

    match code.as_str() {
        "good" => Ok(UpdateOutcome::Updated { ip: ip_in_body }),
        "nochg" => Ok(UpdateOutcome::NoChange { ip: ip_in_body }),
        "badauth" => Err(DdnsError::FatalConfig {
            status: 401,
            detail: "badauth".to_string(),
        }),
        "nohost" => Err(DdnsError::FatalConfig {
            status: 404,
            detail: "nohost".to_string(),
        }),
        "abuse" => Err(DdnsError::FatalConfig {
            status: 403,
            detail: "abuse".to_string(),
        }),
        "notfqdn" | "numhost" => Err(DdnsError::FatalConfig {
            status: 400,
            detail: code,
        }),
        "911" => Err(DdnsError::ServerError {
            status: 500,
            retry_after_s: DDNS_5XX_BACKOFF_S,
        }),
        other => Err(DdnsError::Protocol(format!("无法识别的状态码：{other}"))),
    }
}

const KNOWN_CODES: &[&str] = &[
    "nochg", "badauth", "nohost", "abuse", "notfqdn", "numhost", "good", "911",
];

fn is_known_code(s: &str) -> bool {
    KNOWN_CODES.contains(&s)
}

/// 在 JSON/XML 兜底形态的 body 里找一个「独立 token」形态的状态码。
///
/// 规则（与 parse_ddns2_body 的注释对应）：
///   1. code 的前后必须是非字母数字边界 —— 引号 / 空白 / 冒号 / 逗号 /
///      尖括号 / `=` / 花括号都算边界。这样 `"good"`、`<good>`、`code=good`
///      命中，而 `notgood`、`goodness`、时间戳里的 `911` 不命中。
///   2. 失败码优先于成功码。body 里同时出现 good 和 badauth 时按失败处理 ——
///      宁可漏一次更新，不可假成功。
///
/// 返回小写后的 code（原 body 已 lower，这里只在其中找）。
fn find_delimited_code(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let bytes = lower.as_bytes();

    // 顺序即优先级：先失败码，后成功码。
    const PRIORITY: &[&str] = &[
        "badauth", "nohost", "abuse", "notfqdn", "numhost", "911", "nochg", "good",
    ];

    PRIORITY.iter().find_map(|code| {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(code) {
            let start = from + rel;
            if is_token_boundary(bytes, start, start + code.len()) {
                return Some((*code).to_string());
            }
            // 越过这个命中点继续找。+1（而非 +len）以免漏掉重叠情形；
            // start 是 ASCII 匹配的起点，+1 必然仍是 char 边界。
            from = start + 1;
            if from >= lower.len() {
                break;
            }
        }
        None
    })
}

/// 在 JSON 形态的 body 里定位**非空**的 `error` 字段，返回其值。
///
/// 手写扫描，不引入 serde_json 依赖。规则：
///   1. 键必须是独立的 `"error"`（匹配串自带收尾引号），所以 `"errors"`、
///      `"error_code"` 不会被误当成 error 字段。
///   2. 键与值之间允许空白，但必须有冒号。
///   3. 值必须是被双引号包裹的字符串；空串 `""` 视为「没有拒绝」，
///      交回原有状态码扫描逻辑（免得把 `{"error":""}` 判成失败）。
///   4. `none` / `ok` / `null` / `false` / `0` 这类成功占位也不算拒绝。
///
/// 返回值截断到 64 字符 —— 它是服务端原话，进日志前先夹一下。
fn find_json_error_field(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    const KEY: &str = "\"error\"";
    // lower 只做 ASCII 折叠，字节长度与 body 一致，所以字节下标可以直接回切 body。
    let bytes = lower.as_bytes();

    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(KEY) {
        let start = from + rel;
        let mut i = start + KEY.len();

        // 跳过空白，要求冒号
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b':' {
            from = start + 1;
            continue;
        }
        i += 1;

        // 跳过空白，要求开引号
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            from = start + 1;
            continue;
        }
        i += 1;

        // 扫到收尾引号
        let val_start = i;
        while i < bytes.len() && bytes[i] != b'"' {
            i += 1;
        }
        if i >= bytes.len() {
            from = start + 1;
            continue;
        }

        let raw = body[val_start..i].trim();
        if raw.is_empty() {
            // 空 error 字段：不构成拒绝，继续看后面的内容
            from = i + 1;
            continue;
        }
        let low = raw.to_ascii_lowercase();
        if matches!(low.as_str(), "none" | "ok" | "null" | "false" | "0") {
            from = i + 1;
            continue;
        }
        return Some(truncate(raw, 64));
    }
    None
}

/// `start..end` 是 code 在 bytes 里的半开区间。
/// 边界定义为「非 ASCII 字母数字」—— 这既排除了 `notgood` 这类前缀粘连，
/// 也排除了 IP / 时间戳里的数字被当成状态码（`911` 前面是数字就不算）。
fn is_token_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
    let right_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
    left_ok && right_ok
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

impl DdnsProvider for DnsheProvider {
    fn name(&self) -> &'static str {
        "dnshe"
    }

    fn ip_optional(&self) -> bool {
        // 服务端会用请求来源 IP
        true
    }

    fn supports_v6(&self) -> bool {
        // 双闸门：编译期 && 运行期。任一为 false 都返回 false。
        DDNS_IPV6_COMPILE_ENABLE && self.enable_ipv6
    }

    fn build_request(
        &self,
        rec: &DdnsRecord,
        ip: Option<&IpAddr>,
    ) -> Result<HttpRequest, DdnsError> {
        // 闸门只在这一处判定。真正的逻辑在自由函数里，方便主机测试。
        build_dnshe_request(&self.token, rec, ip, self.supports_v6())
    }

    fn parse_response(&self, status: u16, body: &str) -> Result<UpdateOutcome, DdnsError> {
        parse_dnshe_response(status, body)
    }
}

/// DNSHE 的请求构造 —— 纯函数，`allow_v6` 由调用方传入。
///
/// 把逻辑抽成自由函数是为了让它可测：编译期闸门 `DDNS_IPV6_COMPILE_ENABLE`
/// 默认是关的，如果 v6 的构造逻辑埋在 trait impl 里，主机上就永远测不到它
/// 「两把闸门都开了之后到底发什么」。现在闸门判定留在
/// `DnsheProvider::build_request` 一处，这里只负责构造。
fn build_dnshe_request(
    token: &str,
    rec: &DdnsRecord,
    ip: Option<&IpAddr>,
    allow_v6: bool,
) -> Result<HttpRequest, DdnsError> {
    // 显式传入的 ip 优先，其次用 record 自带的
    let effective = ip.or(rec.ip.as_ref());

    // ---- 家族校验 + v6 闸门 ----
    // 顺便把「要不要显式推 IP」定下来：
    //   A  + None     -> None      （blind push，服务端认来源 IP）
    //   A  + Some(v4) -> Some(v4)  （显式推）
    //   AAAA + Some(v6) -> Some(v6)（必须显式，见下）
    let push_ip: Option<IpAddr> = match rec.record {
        RecordType::Aaaa => {
            if !allow_v6 {
                return Err(DdnsError::Local(
                        "AAAA 更新被禁用：编译期 DDNS_IPV6_COMPILE_ENABLE 与运行期 enable_ipv6 必须同时打开"
                            .to_string(),
                    ));
            }
            match effective {
                Some(v6 @ IpAddr::V6(_)) => Some(*v6),
                Some(IpAddr::V4(_)) => {
                    return Err(DdnsError::Local(
                        "记录类型是 AAAA，但给的是 IPv4 地址".to_string(),
                    ))
                }
                None => {
                    return Err(DdnsError::Local(
                        "AAAA 更新必须显式提供 IPv6 地址（服务端不对 v6 做来源 IP 推断）"
                            .to_string(),
                    ))
                }
            }
        }
        RecordType::A => match effective {
            Some(v4 @ IpAddr::V4(_)) => Some(*v4),
            Some(IpAddr::V6(_)) => {
                return Err(DdnsError::Local(
                    "记录类型是 A，但给的是 IPv6 地址".to_string(),
                ))
            }
            None => None,
        },
    };

    // 手工拼 JSON。IP 地址只含 0-9a-fA-F、`.`、`:`，没有需要转义的字符，
    // 所以这里不需要 serde_json（主 crate 里也没这个依赖）。
    let body = push_ip.map(|ip| format!("{{\"ip\":\"{ip}\"}}"));

    let mut headers: Vec<(String, String)> = Vec::new();

    // 唯一的凭据通道。绝不进 URL，绝不进 body。
    headers.push((
        // DNSHE 用自定义 header `X-DDNS-Token`，值是裸 token（无 Bearer 前缀）
        "X-DDNS-Token".to_string(),
        token.to_string(),
    ));

    if body.is_some() {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }

    let content_len = body.as_deref().map(str::len).unwrap_or(0).to_string();
    headers.push(("Content-Length".to_string(), content_len));

    let req = HttpRequest {
        // 统一用 POST：v6 需要 body，v4 也没有理由用 GET 把参数暴露在 URL 上。
        method: "POST",
        url: dnshe_update_url(),
        headers,
        body,
    };

    // 结构性双保险：万一以后有人往 URL / body 里塞了 token，宁可发不出去。
    assert_token_absent(&req.url, token)?;
    if let Some(b) = req.body.as_deref() {
        assert_token_absent(b, token)?;
    }

    Ok(req)
}

/// DNSHE 的响应解释 —— 纯函数，不碰任何 esp 类型。
fn parse_dnshe_response(status: u16, body: &str) -> Result<UpdateOutcome, DdnsError> {
    match status {
        // 配置错误三兄弟：token 错 / 没权限 / 记录不存在。
        // 重试不会好，必须停下来大声报。
        401 | 403 | 404 => {
            return Err(DdnsError::FatalConfig {
                status,
                detail: "token 无效、无权限，或记录不存在".to_string(),
            })
        }

        // 限流
        429 => {
            return Err(DdnsError::Transient {
                status,
                retry_after_s: DDNS_429_BACKOFF_S,
            })
        }

        // 5xx：至少退避 10 分钟
        s if (500..=599).contains(&s) => {
            return Err(DdnsError::ServerError {
                status: s,
                retry_after_s: DDNS_5XX_BACKOFF_S,
            })
        }

        // 其他 4xx：可重试，但至少等 60 秒
        s if (400..500).contains(&s) => {
            return Err(DdnsError::Transient {
                status: s,
                retry_after_s: DDNS_MIN_INTERVAL_S,
            })
        }

        // 2xx：继续看 body（good / nochg）
        s if (200..300).contains(&s) => {}

        s => return Err(DdnsError::Protocol(format!("意外的 HTTP 状态码 {s}"))),
    }

    parse_ddns2_body(body)
}

// ============================================================================
// Cloudflare —— 预留
// ============================================================================

/// Cloudflare 的 provider **占位**。
///
/// 架构上已经留好位置（这就是 provider trait 存在的意义），但还没实现。
/// 之所以不做成「看起来能跑」的半成品：Cloudflare 的更新语义和 DNSHE 完全不同 ——
/// 需要 zone id + record id，认证走 `Authorization: Bearer <API_TOKEN>`
/// 或 `X-Auth-Email` + `X-Auth-Key`，响应是 JSON
/// `{"success":true,"result":{...}}`，而主 crate 里没有 serde_json。
///
/// 要真正实现，需要补上：zone/record id 的持有、`PATCH
/// https://api.cloudflare.com/client/v4/zones/{zone}/dns_records/{id}`
/// 的请求构造、以及 JSON 响应解析（要么手工扫，要么把 serde_json 加进主 crate）。
///
/// 在实现之前，它的每个方法都明确失败 —— **不会**静默地什么都不做。
pub struct CloudflareProvider {
    token: String,
    zone_id: String,
    record_id: String,
}

impl CloudflareProvider {
    pub fn new(
        token: impl Into<String>,
        zone_id: impl Into<String>,
        record_id: impl Into<String>,
    ) -> Self {
        Self {
            token: token.into(),
            zone_id: zone_id.into(),
            record_id: record_id.into(),
        }
    }

    pub fn token_masked(&self) -> String {
        mask_token(&self.token)
    }

    fn todo(&self) -> DdnsError {
        DdnsError::Local(
            "cloudflare provider 尚未实现（预留占位）—— 见 ddns.rs 里的说明".to_string(),
        )
    }
}

impl DdnsProvider for CloudflareProvider {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    fn ip_optional(&self) -> bool {
        // Cloudflare 必须显式给出 IP
        false
    }

    fn supports_v6(&self) -> bool {
        false
    }

    fn build_request(
        &self,
        _rec: &DdnsRecord,
        _ip: Option<&IpAddr>,
    ) -> Result<HttpRequest, DdnsError> {
        let _ = (&self.zone_id, &self.record_id);
        Err(self.todo())
    }

    fn parse_response(&self, _status: u16, _body: &str) -> Result<UpdateOutcome, DdnsError> {
        Err(self.todo())
    }
}

// ============================================================================
// 高层入口
// ============================================================================

/// 把 provider 和调度策略绑在一起。
///
/// 纯逻辑部分（`plan` / `classify` / `interval_s`）在主机上可测；
/// 只有 `run_once` 会真的走网络（且只在 esp 目标上编译）。
pub struct DdnsClient<P: DdnsProvider> {
    provider: P,
    /// 常规更新周期，**已经夹到 >= 60s**。
    interval_s: u64,
}

impl<P: DdnsProvider> DdnsClient<P> {
    /// `configured_interval_s` 会被夹到 >= [`DDNS_MIN_INTERVAL_S`]。
    /// 配置里写 5 秒也没用 —— 下限是硬编码的。
    pub fn new(provider: P, configured_interval_s: u64) -> Self {
        Self {
            provider,
            interval_s: clamp_interval_s(configured_interval_s),
        }
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// 生效的更新周期（已夹到下限）。
    pub fn interval_s(&self) -> u64 {
        self.interval_s
    }

    /// 纯函数：只构造请求，不发出去。
    pub fn plan(&self, rec: &DdnsRecord, ip: Option<&IpAddr>) -> Result<HttpRequest, DdnsError> {
        self.provider.build_request(rec, ip)
    }

    /// 纯函数：只解释响应，不碰网络。
    pub fn classify(&self, status: u16, body: &str) -> Result<UpdateOutcome, DdnsError> {
        self.provider.parse_response(status, body)
    }

    /// 根据结果决定下次等多久。纯函数。
    pub fn next_delay(&self, outcome: &Result<UpdateOutcome, DdnsError>) -> NextDelay {
        next_delay_s(outcome, self.interval_s)
    }
}

#[cfg(target_os = "espidf")]
impl<P: DdnsProvider> DdnsClient<P> {
    /// 跑一次更新：构造请求 -> HTTPS -> 解释响应。
    ///
    /// 调用方拿 `next_delay()` 决定下次什么时候再来。
    /// 这个函数**不睡眠、不重试** —— 调度是调用方的事。
    pub fn run_once(
        &self,
        rec: &DdnsRecord,
        ip: Option<&IpAddr>,
    ) -> Result<UpdateOutcome, DdnsError> {
        let req = self.plan(rec, ip)?;
        let (status, body) = esp_impl::execute(&req)?;
        self.classify(status, &body)
    }
}

// ============================================================================
// esp 传输层 —— 薄壳，主机测试不覆盖
// ============================================================================

#[cfg(target_os = "espidf")]
mod esp_impl {
    use super::{DdnsError, HttpRequest, DDNS_HTTP_TIMEOUT_S, DDNS_MAX_BODY_BYTES};
    use core::time::Duration;
    use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
    use esp_idf_svc::http::Method;

    /// 发一个请求，返回 (status, body)。
    ///
    /// 用 `crt_bundle` 证书包做 TLS 校验 —— 和 C 版 `main.c` 里的
    /// `esp_crt_bundle_attach` 一致，不需要往固件里塞 PEM。
    ///
    /// 全程只用 `EspHttpConnection` 的**固有方法**，不引入
    /// `embedded_svc::io::Read/Write` trait，少一层依赖。
    pub fn execute(req: &HttpRequest) -> Result<(u16, String), DdnsError> {
        let config = Configuration {
            timeout: Some(Duration::from_secs(DDNS_HTTP_TIMEOUT_S)),
            // TLS 根证书：用 IDF 自带的证书包
            crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
            // 我们自己给 Content-Length，不要 chunked
            raw_request_body: true,
            buffer_size: Some(1024),
            buffer_size_tx: Some(1024),
            ..Default::default()
        };

        let mut conn = EspHttpConnection::new(&config)
            .map_err(|e| DdnsError::Local(format!("HTTPS 初始化失败：{e}")))?;

        let method = match req.method {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            other => return Err(DdnsError::Local(format!("不支持的 HTTP 方法：{other}"))),
        };

        let body: &str = req.body.as_deref().unwrap_or("");

        let headers: Vec<(&str, &str)> = req
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        conn.initiate_request(method, &req.url, &headers)
            .map_err(|e| DdnsError::Local(format!("发起请求失败：{e}")))?;

        if !body.is_empty() {
            conn.write_all(body.as_bytes())
                .map_err(|e| DdnsError::Local(format!("发送 body 失败：{e}")))?;
        }

        conn.initiate_response()
            .map_err(|e| DdnsError::Local(format!("读取响应头失败：{e}")))?;

        let status = conn.status();

        let mut buf = [0u8; 512];
        let mut out = String::new();
        loop {
            let n = conn
                .read(&mut buf)
                .map_err(|e| DdnsError::Local(format!("读取响应体失败：{e}")))?;
            if n == 0 {
                break;
            }
            if out.len() + n > DDNS_MAX_BODY_BYTES {
                // 对端抽风，读够了就停 —— 后面的解析也不需要更多
                break;
            }
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
        }

        Ok((status, out))
    }
}

// ============================================================================
// 主机单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const TOKEN: &str = "sk_test_fake_000000000000DEADBEEF";
    const HOST: &str = "lyg.ddns.ge";

    fn dnshe() -> DnsheProvider {
        DnsheProvider::new(TOKEN, HOST)
    }

    fn dnshe_v6() -> DnsheProvider {
        DnsheProvider::new(TOKEN, HOST).with_ipv6(true)
    }

    // ---- 1. 成功响应 ----

    #[test]
    fn parses_successful_update() {
        let r = dnshe().parse_response(200, "good 203.0.113.7").unwrap();
        assert_eq!(
            r,
            UpdateOutcome::Updated {
                ip: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)))
            }
        );
        assert!(r.is_change(), "good 必须算一次真正的变更");
    }

    #[test]
    fn parses_success_without_ip_in_body() {
        let r = dnshe().parse_response(200, "good").unwrap();
        assert_eq!(r, UpdateOutcome::Updated { ip: None });
        assert!(r.is_change());
    }

    #[test]
    fn parses_success_with_trailing_newline_and_spaces() {
        let r = dnshe()
            .parse_response(200, "  good   203.0.113.7\r\n")
            .unwrap();
        assert_eq!(r.ip(), Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))));
    }

    // ---- 2. nochg = 成功，但不是变更 ----

    #[test]
    fn parses_nochg_as_success_but_not_a_change() {
        let r = dnshe().parse_response(200, "nochg 203.0.113.7").unwrap();

        // 是成功 —— 不能当错误处理
        assert_eq!(
            r,
            UpdateOutcome::NoChange {
                ip: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)))
            }
        );
        // 但不是变更 —— 没发生 DNS 写入
        assert!(!r.is_change(), "nochg 不能算作一次 DNS 变更");

        // 走 next_delay 也应该按「一切正常」调度，而不是退避
        assert_eq!(next_delay_s(&Ok(r), 300), NextDelay::Scheduled(300));
    }

    // ---- 3. 401 / 403 / 404 = FATAL，停止重试 ----

    #[test]
    fn auth_and_missing_record_are_fatal() {
        for status in [401u16, 403, 404] {
            let err = dnshe().parse_response(status, "").unwrap_err();
            assert!(
                matches!(err, DdnsError::FatalConfig { .. }),
                "HTTP {status} 必须是 FatalConfig，实际 {err:?}"
            );
            assert!(err.is_fatal(), "HTTP {status} 必须被标记为 fatal");
            assert_eq!(
                err.retry_after_s(),
                None,
                "HTTP {status} 不该给出重试间隔 —— 重试无用"
            );
            assert_eq!(
                next_delay_s(&Err(err), 300),
                NextDelay::Stop,
                "HTTP {status} 之后必须停止重试"
            );
        }
    }

    #[test]
    fn badauth_and_nohost_in_body_are_fatal() {
        for (status, body) in [(200u16, "badauth"), (200, "nohost")] {
            let err = dnshe().parse_response(status, body).unwrap_err();
            assert!(
                matches!(err, DdnsError::FatalConfig { .. }),
                "body {body:?} 必须被判成配置错误，实际 {err:?}"
            );
            assert_eq!(next_delay_s(&Err(err), 300), NextDelay::Stop);
        }
    }

    // ---- 4. 5xx = 退避 >= 10 分钟 ----

    #[test]
    fn server_error_backs_off_at_least_ten_minutes() {
        for status in [500u16, 502, 503, 504, 599] {
            let err = dnshe().parse_response(status, "").unwrap_err();
            assert!(
                matches!(err, DdnsError::ServerError { .. }),
                "HTTP {status} 必须是 ServerError，实际 {err:?}"
            );

            let wait = err
                .retry_after_s()
                .unwrap_or_else(|| panic!("HTTP {status} 应该给出重试间隔"));
            assert!(wait >= 600, "HTTP {status} 的退避 {wait}s 少于 10 分钟");

            // 走完整决策路径也要满足
            match next_delay_s(&Err(err), 300) {
                NextDelay::Retry(s) => assert!(s >= 600, "HTTP {status} 实际调度 {s}s"),
                other => panic!("HTTP {status} 应该重试，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn malformed_body_cools_down_like_a_server_error() {
        let err = dnshe().parse_response(200, "wat").unwrap_err();
        assert!(matches!(err, DdnsError::Protocol(_)));
        let wait = err.retry_after_s().expect("看不懂也要退避，不能立刻重试");
        assert!(wait >= 600, "看不懂响应时退避 {wait}s，太短了");
    }

    // ---- 5. token 绝不进日志 ----

    #[test]
    fn token_never_appears_in_any_log_bound_string() {
        let p = dnshe();
        let rec = DdnsRecord::explicit_v4(Ipv4Addr::new(203, 0, 113, 7));
        let req = p.build_request(&rec, None).expect("v4 请求应该能构造");

        let masked = mask_token(TOKEN);
        assert_eq!(masked, "sk_test_fake_***");

        // 所有可能被打印出来的字符串
        let log_bound: Vec<(&str, String)> = vec![
            ("HttpRequest Debug", format!("{req:?}")),
            ("redacted_request_line", redacted_request_line(&req)),
            ("token_masked", p.token_masked()),
            ("url", req.url.clone()),
            ("body", req.body.clone().unwrap_or_default()),
            (
                "all headers joined",
                req.headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(" | "),
            ),
        ];

        for (label, s) in &log_bound {
            // url / body / header-join 里本来就**不该**有 token，
            // 但即使有，只要是被掩码过的形态就还能接受 —— 所以这里
            // 断言的是「完整 token 一定不出现」。
            if label == &"HttpRequest Debug" || label == &"token_masked" {
                assert!(!s.contains(TOKEN), "{label} 泄露了完整 token：{s}");
                assert!(
                    s.contains("sk_test_fake_"),
                    "{label} 应该保留前 8 字符用于辨识：{s}"
                );
            } else {
                assert!(!s.contains(TOKEN), "{label} 里出现了完整 token：{s}");
            }
        }

        // 最严格的一条：Debug 输出里连「token 的第 9 个字符起」都不该有
        let dbg = format!("{req:?}");
        let tail: String = TOKEN.chars().skip(8).collect();
        assert!(
            !dbg.contains(&tail),
            "Debug 输出泄露了 token 的后半段：{dbg}"
        );
    }

    #[test]
    fn masking_edge_cases() {
        assert_eq!(mask_token(""), "(unset)");
        assert_eq!(mask_token("abc"), "abc***");
        assert_eq!(mask_token("12345678"), "12345678***");
        assert_eq!(mask_token("123456789"), "12345678***");

        // header 掩码要保留 scheme
        assert_eq!(
            mask_header_value("Authorization", "Bearer sk_test_fake_000000000000"),
            "Bearer sk_test_fake_***"
        );
        assert_eq!(
            mask_header_value("X-DDNS-Token", "sk_test_fake_000000000000"),
            "sk_test_fake_***"
        );
        // 非凭据 header 原样保留
        assert_eq!(
            mask_header_value("Content-Type", "application/json"),
            "application/json"
        );
    }

    #[test]
    fn token_is_only_ever_in_the_ddns_token_header() {
        let p = dnshe();
        let rec = DdnsRecord::explicit_v4(Ipv4Addr::new(203, 0, 113, 7));
        let req = p.build_request(&rec, None).unwrap();

        assert!(!req.url.contains(TOKEN), "token 进了 URL：{}", req.url);
        assert!(
            !req.body.as_deref().unwrap_or("").contains(TOKEN),
            "token 进了 body"
        );

        // 而且只应该出现在一个 header 里
        let carriers: Vec<&String> = req
            .headers
            .iter()
            .filter(|(_, v)| v.contains(TOKEN))
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            carriers.len(),
            1,
            "token 出现在多个 header 里：{carriers:?}"
        );
        assert_eq!(carriers[0], "X-DDNS-Token");
    }

    // ---- 6. URL 构造：v4 与 v6 ----

    #[test]
    fn builds_v4_blind_push_without_body() {
        let p = dnshe();
        let rec = DdnsRecord::blind_v4();
        let req = p.build_request(&rec, None).unwrap();

        assert_eq!(req.method, "POST");
        assert_eq!(req.url, DNSHE_UPDATE_URL);
        assert!(req.body.is_none(), "blind push 不该带 body");

        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-ddns-token"))
            .expect("必须有 X-DDNS-Token header");
        assert_eq!(auth.1, TOKEN);

        assert!(!req.headers.iter().any(|(k, _)| k == "Content-Type"));
        assert_eq!(
            req.headers
                .iter()
                .find(|(k, _)| k == "Content-Length")
                .map(|(_, v)| v.as_str()),
            Some("0")
        );
    }

    #[test]
    fn builds_v4_explicit_push_with_json_body() {
        let p = dnshe();
        let rec = DdnsRecord::blind_v4();
        let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9));
        let req = p.build_request(&rec, Some(&ip)).unwrap();

        assert_eq!(req.url, DNSHE_UPDATE_URL);
        assert_eq!(req.body.as_deref(), Some("{\"ip\":\"198.51.100.9\"}"));
        assert_eq!(
            req.headers
                .iter()
                .find(|(k, _)| k == "Content-Type")
                .map(|(_, v)| v.as_str()),
            Some("application/json")
        );
        assert_eq!(
            req.headers
                .iter()
                .find(|(k, _)| k == "Content-Length")
                .map(|(_, v)| v.as_str()),
            Some("25")
        );
    }

    #[test]
    fn builds_v6_with_explicit_ip_in_body() {
        let p = dnshe_v6();
        let rec = DdnsRecord::explicit_v6("2001:db8::1".parse().unwrap());
        let req = p.build_request(&rec, None).unwrap();

        assert_eq!(req.url, DNSHE_UPDATE_URL);
        assert_eq!(req.body.as_deref(), Some("{\"ip\":\"2001:db8::1\"}"));
        assert!(!req.url.contains("2001"), "v6 地址不该出现在 URL 里");
    }

    #[test]
    fn v6_requires_an_explicit_ip() {
        let p = dnshe_v6();
        let rec = DdnsRecord {
            record: RecordType::Aaaa,
            ip: None,
        };
        let err = p.build_request(&rec, None).unwrap_err();

        assert!(
            matches!(err, DdnsError::Local(_)),
            "v6 不给 IP 必须报本地错，实际 {err:?}"
        );
        assert!(
            err.summary().contains("必须显式"),
            "错误信息要说清楚：{err}"
        );
        // 本地错重试无用
        assert_eq!(next_delay_s(&Err(err), 300), NextDelay::Stop);
    }

    // ---- 7. IPv6 双重闸门默认关闭 ----

    #[test]
    fn ipv6_is_off_by_default_on_both_gates() {
        // 编译期闸门在源码里就是 false
        assert!(!DDNS_IPV6_COMPILE_ENABLE, "编译期 v6 闸门必须默认关");

        // 运行期闸门默认 false -> supports_v6() 为 false
        assert!(!dnshe().supports_v6(), "运行期默认必须关掉 v6");

        // 只开运行期、编译期还关着 -> 依然不可用
        assert!(!dnshe_v6().supports_v6(), "只开一道闸门不能启用 v6");

        // 而且 AAAA 请求会被直接拒绝
        let rec = DdnsRecord::explicit_v6(Ipv6Addr::LOCALHOST);
        let err = dnshe_v6().build_request(&rec, None).unwrap_err();
        assert!(matches!(err, DdnsError::Local(_)));
    }

    #[test]
    fn rejects_mismatched_address_families() {
        // A 记录 + v6 地址
        let rec = DdnsRecord {
            record: RecordType::A,
            ip: None,
        };
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(matches!(
            dnshe().build_request(&rec, Some(&v6)).unwrap_err(),
            DdnsError::Local(_)
        ));

        // AAAA 记录 + v4 地址（这里必须用开了 v6 的 provider，
        // 否则会先被闸门拦下，测不到家族校验）
        let rec6 = DdnsRecord {
            record: RecordType::Aaaa,
            ip: None,
        };
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(matches!(
            dnshe_v6().build_request(&rec6, Some(&v4)).unwrap_err(),
            DdnsError::Local(_)
        ));
    }

    // ---- 8. 60 秒硬下限 ----

    #[test]
    fn sixty_second_floor_cannot_be_configured_away() {
        assert_eq!(clamp_interval_s(0), 60);
        assert_eq!(clamp_interval_s(1), 60);
        assert_eq!(clamp_interval_s(59), 60);
        assert_eq!(clamp_interval_s(60), 60);
        assert_eq!(clamp_interval_s(61), 61);
        assert_eq!(clamp_interval_s(3600), 3600);

        // 走客户端也一样
        let c = DdnsClient::new(dnshe(), 5);
        assert_eq!(c.interval_s(), 60, "配置 5 秒必须被夹到 60 秒");

        let c = DdnsClient::new(dnshe(), 0);
        assert_eq!(c.interval_s(), 60);

        // 成功路径的调度也不能低于 60
        let ok = Ok(UpdateOutcome::NoChange { ip: None });
        assert_eq!(next_delay_s(&ok, 5), NextDelay::Scheduled(60));
    }

    #[test]
    fn provider_metadata() {
        let p = dnshe();
        assert_eq!(p.name(), "dnshe");
        assert!(p.ip_optional(), "DNSHE 可以用来源 IP 推断");
        assert_eq!(p.hostname(), HOST);

        let cf = CloudflareProvider::new(TOKEN, "zone", "rec");
        assert_eq!(cf.name(), "cloudflare");
        assert!(!cf.ip_optional());
        assert!(!cf.supports_v6());
        // 预留实现必须明确失败，不能静默成功
        let rec = DdnsRecord::blind_v4();
        assert!(cf.build_request(&rec, None).is_err());
        assert!(cf.parse_response(200, "{}").is_err());
        assert!(!cf.token_masked().contains(TOKEN));
    }

    // ---- 9. 客户端串起来 ----

    #[test]
    fn client_plan_and_classify_round_trip() {
        let c = DdnsClient::new(dnshe(), 300);

        let rec = DdnsRecord::explicit_v4(Ipv4Addr::new(203, 0, 113, 7));
        let req = c.plan(&rec, None).unwrap();
        assert_eq!(req.url, DNSHE_UPDATE_URL);

        let outcome = c.classify(200, "good 203.0.113.7").unwrap();
        assert!(outcome.is_change());
        assert_eq!(c.next_delay(&Ok(outcome)), NextDelay::Scheduled(300));

        // 失败路径
        let err = c.classify(401, "").unwrap_err();
        assert_eq!(c.next_delay(&Err(err)), NextDelay::Stop);

        let err = c.classify(503, "").unwrap_err();
        match c.next_delay(&Err(err)) {
            NextDelay::Retry(s) => assert!(s >= 600),
            other => panic!("503 应该退避重试，实际 {other:?}"),
        }
    }
}
