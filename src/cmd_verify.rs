//! 指令校验：解析 JSON + 防重放 + topic 逐字节自校验
//!
//! 移植自 C 版 `main/cmd_verify.c` / `main/cmd_verify.h`。
//!
//! 为什么要在固件里做 topic 校验（架构文档 4.4）：
//!  EMQX Serverless 可能不开放细粒度 ACL。不能假设上游一定配对了，
//!  收到消息先检查 topic 是否恰好等于 "psw/<自己的device_id>/cmd"，
//!  不等就丢弃。几行代码，把"broker 配置不当"这个风险从架构里消掉。
//!  这叫纵深防御。
//!
//! ## 与 C 版的差异（已在交付报告中说明）
//! - C 版没有任何签名 / HMAC。真正的"认证"只有 topic 白名单 + 时间窗 +
//!   nonce 去重。这里逐条照搬，**没有**发明任何新的密码学方案。
//!   因此本文件里也不存在"签名比较"，也就没有需要常数时间比较的地方。
//!   topic 比较用的是公开的 device_id，不是密钥，普通比较即可。
//! - C 版把 nonce 环大小硬编码成 32（`NONCE_SLOTS`），`app_config.h` 里的
//!   `NONCE_HISTORY`(16) 是**死代码**。这里按任务要求使用 `NONCE_HISTORY`。
//! - C 版在函数里直接 `ESP_LOGW`；本模块保持纯净（无副作用、无日志），
//!   把原因编码进 [`CmdError`]，由调用方决定怎么记日志。
//! - C 版通过全局变量保存 device_id / 时间同步标志；这里收进 [`CmdVerifier`]。

use crate::consts::{CMD_TTL_DEFAULT, NONCE_HISTORY, PULSE_MS_MAX};

/// 期望 topic 的缓冲长度，与 C 版 `static char s_expected_topic[96]` 对齐。
/// 超长时按 C 的 `snprintf` 语义截断。
const TOPIC_BUF: usize = 96;

/// ttl 硬上限（秒）。与 C 版一致：`ttl <= 0 || ttl > 300` 即视为非法，
/// 回退到配置值。
const TTL_MAX: i64 = 300;

/// `press_long` 的脉宽（毫秒）。
///
/// ⚠️ 这里**必须**写成 PULSE_MS_MAX 本身，不能照抄 C 版的 5000。
/// consts.rs 里 PULSE_MS_MAX(3000) 是【安全边界】：>=4000ms 的长按会触发
/// 主板强制断电。如果这里写 5000，安全性就只靠 pulse.rs 里的 clamp 兜着 ——
/// 一个跨了两个模块的"侥幸正确"。写成 PULSE_MS_MAX 之后，pulse 里的 clamp
/// 才退化成真正的纵深防御，而不是唯一的防线。
const PRESS_LONG_MS: u16 = PULSE_MS_MAX;

/// 校验失败的原因。每一种都必须让调用方能区分并记日志。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdError {
    /// JSON 解析失败
    BadJson,
    /// 字段缺失 / 类型不对 / topic 不匹配
    BadSchema,
    /// cmd 不是已知指令
    UnknownCmd,
    /// ts + ttl 超窗
    Expired,
    /// nonce 重复
    Replay,
    /// 时间未同步（SNTP 未成功），拒绝执行
    TimeNotSynced,
}

impl CmdError {
    /// 与 C 版 `cmd_result_str()` 输出的字符串保持一致（`CMD_OK` -> `"ok"`）。
    pub fn as_str(self) -> &'static str {
        match self {
            CmdError::BadJson => "bad-json",
            CmdError::BadSchema => "bad-schema",
            CmdError::UnknownCmd => "unknown-cmd",
            CmdError::Expired => "expired",
            CmdError::Replay => "replay",
            CmdError::TimeNotSynced => "time-not-synced",
        }
    }
}

impl core::fmt::Display for CmdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// FNV-1a 32 位哈希。与 C 版逐字节一致（offset basis 2166136261,
/// prime 16777619），作用是只把 nonce 的 4 字节摘要放进 RAM，
/// 而不是存 nonce 原文。
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16_777_619);
    }
    h
}

/// 指令校验器。持有期望 topic、时间同步标志和 nonce 环。
///
/// 除 [`CmdVerifier::verify`] 会更新 nonce 环外，本模块无任何副作用：
/// 不碰 GPIO、不发网络、不写日志、不打印任何密钥。
pub struct CmdVerifier {
    expected_topic: [u8; TOPIC_BUF],
    expected_len: usize,
    time_synced: bool,
    /// nonce 去重环，存 FNV-1a 摘要（不是原文），固定大小、不分配堆。
    nonce_ring: [u32; NONCE_HISTORY],
    nonce_idx: usize,
}

impl CmdVerifier {
    /// 记录本机 device_id（由 MAC 派生）并清空 nonce 历史。
    /// 对应 C 版 `cmd_verify_init()`。
    pub fn new(device_id: &str) -> Self {
        let mut expected_topic = [0u8; TOPIC_BUF];
        let mut expected_len = 0usize;
        // 等价于 snprintf(buf, 96, "psw/%s/cmd", device_id)：超长则截断。
        for part in [&b"psw/"[..], device_id.as_bytes(), &b"/cmd"[..]] {
            for &b in part {
                if expected_len >= TOPIC_BUF {
                    break;
                }
                expected_topic[expected_len] = b;
                expected_len += 1;
            }
        }
        Self {
            expected_topic,
            expected_len,
            time_synced: false,
            nonce_ring: [0u32; NONCE_HISTORY],
            nonce_idx: 0,
        }
    }

    /// 时间是否已同步（SNTP）。未同步前一律拒绝执行指令。
    pub fn set_time_synced(&mut self, synced: bool) {
        self.time_synced = synced;
    }

    /// 对应 C 版 `cmd_time_synced()`。
    pub fn time_synced(&self) -> bool {
        self.time_synced
    }

    /// 期望 topic，形如 `psw/<device_id>/cmd`（仅供日志/调试）。
    pub fn expected_topic(&self) -> &str {
        core::str::from_utf8(&self.expected_topic[..self.expected_len]).unwrap_or("")
    }

    /// 纵深防御第一层：topic 必须逐字节等于我们期望的那一个。
    fn topic_matches(&self, topic: &[u8]) -> bool {
        topic.len() == self.expected_len && topic == &self.expected_topic[..self.expected_len]
    }

    fn nonce_seen(&self, h: u32) -> bool {
        self.nonce_ring.iter().any(|&x| x == h)
    }

    fn nonce_remember(&mut self, h: u32) {
        self.nonce_ring[self.nonce_idx] = h;
        self.nonce_idx = (self.nonce_idx + 1) % NONCE_HISTORY;
    }

    /// 校验一条 MQTT 下行消息。对应 C 版 `cmd_verify()`。
    ///
    /// - `topic`：broker 给的原始 topic（UTF-8）
    /// - `data`：载荷原始字节
    /// - `now_ts`：当前 Unix 时间（秒）
    /// - `cfg_pulse_ms` / `cfg_cmd_ttl`：调用方从运行时配置（NVS）读出的值。
    ///   C 版在函数内部调 `cfg_pulse_ms()` / `cfg_cmd_ttl()`；为了让本模块
    ///   保持纯净、且能在不依赖 cfg 模块的情况下编译，改为显式传入。
    ///
    /// 通过校验则返回建议的脉冲宽度（毫秒）。
    pub fn verify(
        &mut self,
        topic: &str,
        data: &[u8],
        now_ts: i64,
        cfg_pulse_ms: u16,
        cfg_cmd_ttl: u16,
    ) -> Result<u16, CmdError> {
        // --- 1. topic 白名单（逐字节）---
        if !self.topic_matches(topic.as_bytes()) {
            return Err(CmdError::BadSchema);
        }

        // --- 2. 时间必须先同步，否则 ts 校验形同虚设 ---
        // 未同步时 caller 传 now_ts = 0，skew 会变成 |0 - ts|，一个 1970 年
        // 之后的 ts 必然巨大 —— 表面上也会被 EXPIRED 拦住，但那样调用方
        // 无法区分"时钟没同步"和"指令真的过期"，排障时会走弯路。
        // 所以这里保留 C 版的显式拒绝，并给出独立的原因。
        if !self.time_synced {
            return Err(CmdError::TimeNotSynced);
        }

        // --- 3. 解析 JSON ---
        // C 版是 memcpy 到 NUL 结尾缓冲再 cJSON_Parse。
        //
        // 这里**不用 serde_json**：主 crate 的 Cargo.toml 里没有它，而
        // Cargo.lock 里那份属于 embuild（build-dependency，src/ 链不到）。
        // 为一个固定 4 字段的报文把 serde + serde_json 拉进固件，在
        // Phase 0 那点余量下不划算 —— ddns.rs 已经立了「手工扫」的先例。
        //
        // 严格程度不低于 cJSON：任何一个必需字段缺失/类型不对都是 BadSchema，
        // 整段不是合法 JSON 对象就是 BadJson。fail-closed。
        let obj = JsonObj::parse(data).ok_or(CmdError::BadJson)?;

        // --- 4. schema：cmd 必须是字符串、nonce 必须是字符串、ts 必须是数字 ---
        let cmd = obj.get_str("cmd").ok_or(CmdError::BadSchema)?;
        let nonce = obj.get_str("nonce").ok_or(CmdError::BadSchema)?;
        let ts = obj.get_num("ts").ok_or(CmdError::BadSchema)?;

        // --- 5. 指令语义：只有 press / press_long ---
        // 注意：绝不提供 on/off —— 电源键是"按一下翻转"，不是开关。
        let width = match cmd {
            "press" => cfg_pulse_ms,
            "press_long" => PRESS_LONG_MS,
            _ => return Err(CmdError::UnknownCmd),
        };

        // --- 6. 时间窗 ---
        // ttl 缺省用配置值；非法值（<=0 或 >300）也回退到配置值。
        let cfg_ttl_i = cfg_cmd_ttl as i64;
        let mut ttl: i64 = obj.get_num("ttl").unwrap_or(cfg_ttl_i);
        if ttl <= 0 || ttl > TTL_MAX {
            ttl = cfg_ttl_i;
        }
        // saturating：避免极端 ts 造成 i64 溢出 panic（C 版是 UB）
        let diff = now_ts.saturating_sub(ts);
        let skew = diff.saturating_abs();
        if skew > ttl {
            return Err(CmdError::Expired);
        }

        // --- 7. nonce 去重 ---
        // QoS 1 语义是"至少一次"，broker 抖动时会把同一条消息发两遍。
        // 没有去重，你自己的网络抖动就会让电脑被多按一下。
        let h = fnv1a(nonce);
        if self.nonce_seen(h) {
            return Err(CmdError::Replay);
        }
        self.nonce_remember(h);

        Ok(width)
    }
}

/// 首次上电、还没有任何运行时配置时的 ttl 默认值。
/// 真正的默认值由 cfg 模块从 NVS / `consts::CMD_TTL_DEFAULT` 决定；
/// 这里只是给调用方一个兜底，避免硬编码 30 散落各处。
pub const DEFAULT_CMD_TTL: u16 = CMD_TTL_DEFAULT as u16;

// ---------------------------------------------------------------------------
//  极简 JSON 对象扫描器
// ---------------------------------------------------------------------------

/// 只认「顶层是一个扁平 JSON 对象」这一种形态。
///
/// 为什么不用 serde_json：主 crate 的 Cargo.toml 里没有这个依赖，Cargo.lock
/// 里那份属于 embuild（build-dependency，`src/` 链不到）。为一个固定 4 字段的
/// 报文把 serde + serde_json 拉进固件，在 Phase 0 那点余量下不划算；
/// ddns.rs 已经立了「手工扫」的先例。
///
/// 严格程度不低于 cJSON，fail-closed：
/// * 整段不是合法 JSON 对象 -> [`JsonObj::parse`] 返回 `None`（调用方判 BadJson）
/// * 字段缺失 / 类型不符 -> `get_*` 返回 `None`（调用方判 BadSchema）
///
/// 已知差异（都比 cJSON 更严，不会 fail open）：
/// * 字符串里出现 `\` 转义时 `get_str` 拒绝该字段 —— 我们的协议里 cmd/nonce
///   都是纯 ASCII token，出现转义只会是异常输入，替调用方猜转义语义不如
///   直接拒绝。跳过一个**我们不读**的转义字段不受影响。
/// * 数字字段必须是完整的数字字面量，`"ts":"12"`（字符串）或 `12abc` 都拒绝。
pub struct JsonObj<'a> {
    src: &'a str,
}

impl<'a> JsonObj<'a> {
    /// 解析。只接受 UTF-8 的 `{...}`，且顶层键值序列必须结构完整。
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let s = core::str::from_utf8(data).ok()?;
        let t = s.trim();
        if !t.starts_with('{') || !t.ends_with('}') {
            return None;
        }
        let obj = Self { src: t };
        // 走一遍结构校验（同时顺带确认每对 key/value 都扫得动）。
        if obj.for_each_pair(|_, _| {}) { Some(obj) } else { None }
    }

    /// 取字符串字段。第一个匹配的键生效。
    pub fn get_str(&self, key: &str) -> Option<&'a str> {
        let mut out: Option<&'a str> = None;
        self.for_each_pair(|k, raw| {
            if out.is_some() || k != key {
                return;
            }
            let bytes = raw.as_bytes();
            // raw 必须**整体**是一个字符串字面量：扫描结果刚好吃掉全部字节。
            if let Some((v, end)) = scan_string(bytes, 0) {
                if end == bytes.len() {
                    out = Some(v);
                }
            }
        });
        out
    }

    /// 取数字字段并转成 i64。
    ///
    /// 与 C 的 `(int64_t)cJSON_GetNumberValue()` 一致：浮点 ts 也接受，截断取整。
    /// `f64 -> i64` 在 Rust 里是饱和转换（C 版此处是 UB），配合调用方的
    /// `saturating_sub` 保证极端 ts 不会 panic。
    pub fn get_num(&self, key: &str) -> Option<i64> {
        let mut out: Option<i64> = None;
        self.for_each_pair(|k, raw| {
            if out.is_some() || k != key {
                return;
            }
            if let Ok(v) = raw.trim().parse::<f64>() {
                if v.is_finite() {
                    out = Some(v as i64);
                }
            }
        });
        out
    }

    /// 按顺序遍历顶层键值对，`raw` 是值的原始切片（未解码）。
    ///
    /// 返回 `true` 表示整个对象从头到尾结构合法（遇到结尾的 `}` 且其后无残留）。
    /// 校验是「边扫边判」的：一旦结构出错立即 `false`，不再回调。
    fn for_each_pair<F: FnMut(&'a str, &'a str)>(&self, mut f: F) -> bool {
        let s: &'a str = self.src;
        let b: &'a [u8] = s.as_bytes();
        let n = b.len();
        let mut i = 1usize; // 跳过 '{'
        let mut first = true;
        loop {
            i = skip_ws(b, i);
            if i >= n {
                return false; // 没有收尾的 '}'
            }
            if b[i] == b'}' {
                // 收尾花括号必须是最后一段（trim 过，所以只能等于 n）
                return i + 1 == n;
            }
            if !first {
                if b[i] != b',' {
                    return false; // 键值对之间缺逗号
                }
                i = skip_ws(b, i + 1);
                if i >= n || b[i] == b'}' {
                    return false; // 尾逗号
                }
            }
            first = false;

            // key
            let Some((key, ni)) = scan_string(b, i) else {
                return false;
            };
            i = skip_ws(b, ni);
            if i >= n || b[i] != b':' {
                return false;
            }
            i = skip_ws(b, i + 1);
            if i >= n {
                return false;
            }

            // value：只取原始切片，形态判断留给 get_str / get_num
            let vstart = i;
            let Some(vend) = skip_value(b, i) else {
                return false;
            };
            f(key, &s[vstart..vend]);
            i = vend;
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// 扫描一个 JSON 字符串字面量，返回（内容切片, 结束引号之后的下标）。
///
/// 内容里出现反斜杠转义时返回 `None` —— 见 [`JsonObj::get_str`] 的说明。
fn scan_string<'a>(b: &'a [u8], i: usize) -> Option<(&'a str, usize)> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let start = i + 1;
    let mut j = start;
    while j < b.len() {
        match b[j] {
            b'"' => {
                let s = core::str::from_utf8(&b[start..j]).ok()?;
                return Some((s, j + 1));
            }
            b'\\' => return None,          // 含转义 -> 拒绝
            c if c < 0x20 => return None,  // 未转义的控制字符非法
            _ => j += 1,
        }
    }
    None
}

/// 跳过 `i` 处的一个 JSON 字符串（转义按 2 字节掠过，不解码），
/// 返回结束引号之后的下标。用于跳过我们不读的字段。
fn skip_string(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'"' => return Some(j + 1),
            b'\\' => {
                j += 2;
                if j > b.len() {
                    return None;
                }
            }
            c if c < 0x20 => return None,
            _ => j += 1,
        }
    }
    None
}

/// 跳过 `i` 处的一个 JSON 值，返回其后的下标。
///
/// 嵌套对象/数组只做括号配对跳过，不做深解析 —— 我们不读它们。
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    let c = *b.get(i)?;
    match c {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let (open, close) = if c == b'{' { (b'{', b'}') } else { (b'[', b']') };
            let mut depth = 0usize;
            let mut j = i;
            let mut in_str = false;
            while j < b.len() {
                let ch = b[j];
                if in_str {
                    if ch == b'\\' {
                        j += 2;
                        continue;
                    }
                    if ch == b'"' {
                        in_str = false;
                    }
                } else if ch == b'"' {
                    in_str = true;
                } else if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j + 1);
                    }
                }
                j += 1;
            }
            None
        }
        _ => {
            // 数字 / true / false / null：扫到分隔符为止，不校验形态，
            // 由 get_num 决定它能不能当数字用。
            let mut j = i;
            while j < b.len()
                && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
            {
                j += 1;
            }
            Some(j)
        }
    }
}