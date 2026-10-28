//! MQTT 客户端 —— 对应 C 版 `main/main.c` 的 MQTT 部分
//!
//! 规格见 `docs/mqtt-port.md`。三个不可协商的约束：
//!
//! 1. **回调只做「拷贝 + try_send」**。`EspMqttClient::new_cb` 的回调跑在
//!    IDF 的 MQTT 任务里，且库内部对事件通道是**阻塞发送**（`share()`）。
//!    回调里做 publish / 等锁 / sleep / 读 NVS 都会把 MQTT 任务钉死，
//!    keepalive 发不出去、LWT 可能被误触发。
//! 2. **绝不在回调里拉 GPIO**。脉冲只在 `pulse` 线程里生成（安全线 2），
//!    这里最多把请求投进 `PulseHandle` 的队列。
//! 3. **端口与 scheme 固定 `mqtts://` + 8883**，不从 NVS 读，避免被静默
//!    降级成明文（见 `consts.rs` 的注释）。
//!
//! 线程模型：一条独立的 "mqtt" 线程持有 client，所有 `publish` / `subscribe`
//! 都只在那条线程里发生。外部（控制台 / 主循环）只能通过 [`MqttHandle`]
//! 的 `try_send` 提需求，**永远拿不到 client** —— 这是结构性保证，不是自觉。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use esp_idf_svc::mqtt::client::{
    Details, EspMqttClient, EspMqttEvent, EventPayload, LwtConfiguration, MqttClientConfiguration,
    QoS,
};
use esp_idf_svc::sys::{EspError, ESP_ERR_INVALID_ARG, ESP_FAIL};
use log::{error, info, warn};

use crate::cfg::Cfg;
use crate::cmd_verify::{CmdError, CmdVerifier};
use crate::consts::{
    CMD_TTL_DEFAULT, DEFAULT_DEV_ID, MQTT_CLIENT_SUFFIX, MQTT_KEEPALIVE_S, MQTT_PORT,
    PULSE_MS_DEFAULT, RECONNECT_BASE_MS, STATUS_PUBLISH_S,
};
use crate::pc_state::PcState;
use crate::pulse::{PulseError, PulseHandle};

// ---------------------------------------------------------------------------
//  常量（时间周期类的继续用 consts.rs，这里只放本模块私有的）
// ---------------------------------------------------------------------------

/// 回调 -> 线程的事件队列容量。满了丢弃并记日志（丢弃优于阻塞 MQTT 任务）。
const EVENT_QUEUE: usize = 8;
/// "mqtt" 线程栈。
const TASK_STACK: usize = 8192;
/// 线程优先级。
const TASK_PRIO: u8 = 5;
/// MQTT 回调所在任务的栈（`MqttClientConfiguration::task_stack`）。
/// 回调里只做小对象 + `Vec` 堆分配，4 KB 足够；不设会吃 IDF 默认值。
const CB_TASK_STACK: usize = 4096;
/// 接收缓冲。命令报文 < 200 B，4096 保证单包。
const BUFFER_SIZE: usize = 4096;
/// 发送缓冲。
const OUT_BUFFER_SIZE: usize = 2048;
/// 线程主循环的单次等待上限。有事件会立即醒，这只是一个心跳粒度。
const POLL_MS: u64 = 100;

// ---------------------------------------------------------------------------
//  跨线程类型
// ---------------------------------------------------------------------------

/// 从回调拷出来的**已拥有**事件。
///
/// 用 `String` / `Vec<u8>` 而不是 `&[u8]`：IDF 给的指针只在回调期间有效，
/// 带出去就是悬垂引用。
enum MqttEvent {
    Connected { session_present: bool },
    Disconnected,
    Command { topic: String, data: Vec<u8> },
    Error(String),
}

/// 外部线程能提的需求。**不包括 publish 本身** —— 那只能由 mqtt 线程做。
enum MqttRequest {
    PublishState,
}

/// 外部句柄。克隆安全，克隆不会多开线程。
#[derive(Clone)]
pub struct MqttHandle {
    tx: SyncSender<MqttRequest>,
    online: Arc<AtomicBool>,
}

impl MqttHandle {
    /// 当前是否连着 broker。
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// 请求立刻发布一次 PC 状态（`psw/<id>/state`）。非阻塞。
    ///
    /// 返回是否成功投递 —— 队列满时返回 `false`，调用方（控制台）据此
    /// 告诉用户「没发出去」，而不是静默失败。
    pub fn request_state(&self) -> bool {
        match self.tx.try_send(MqttRequest::PublishState) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                warn!("MQTT 请求队列已满，丢弃一次状态发布请求");
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                warn!("MQTT 线程已退出，状态发布请求无处投递");
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
//  topic
// ---------------------------------------------------------------------------

/// 四个 topic 在启动时拼好，之后只读。
struct Topics {
    cmd: String,
    state: String,
    status: String,
    ack: String,
}

impl Topics {
    fn new(device_id: &str) -> Self {
        Self {
            cmd: format!("psw/{device_id}/cmd"),
            state: format!("psw/{device_id}/state"),
            status: format!("psw/{device_id}/status"),
            ack: format!("psw/{device_id}/ack"),
        }
    }
}

// ---------------------------------------------------------------------------
//  线程上下文
// ---------------------------------------------------------------------------

struct RunCtx {
    client: EspMqttClient<'static>,
    rx: Receiver<MqttEvent>,
    rx_req: Receiver<MqttRequest>,
    cfg: Arc<Cfg>,
    pulse: PulseHandle,
    pc_state: Arc<Mutex<PcState<'static>>>,
    time_synced: Arc<AtomicBool>,
    topics: Topics,
    device_id: String,
}

// ---------------------------------------------------------------------------
//  对外入口
// ---------------------------------------------------------------------------

/// 建 client、起 "mqtt" 线程，返回外部句柄。
///
/// **失败只返回 `Err`，不 abort** —— 与 C 版 `main.c:606` 的语义一致：
/// MQTT 挂了不该拖垮脉冲、控制台和 DDNS。
pub fn spawn(
    cfg: Arc<Cfg>,
    pulse: PulseHandle,
    pc_state: Arc<Mutex<PcState<'static>>>,
    time_synced: Arc<AtomicBool>,
) -> Result<MqttHandle, EspError> {
    let host = cfg.mqtt_host().unwrap_or_default();
    if host.is_empty() {
        return Err(EspError::from_infallible::<ESP_ERR_INVALID_ARG>());
    }

    let device_id = cfg.dev_id().unwrap_or_else(|_| DEFAULT_DEV_ID.to_string());
    let topics = Topics::new(&device_id);

    // client_id 必须在 broker 上唯一，否则两台设备互相顶掉会话。
    let client_id = format!("{device_id}{MQTT_CLIENT_SUFFIX}");
    let url = format!("mqtts://{host}:{MQTT_PORT}");

    // 凭据：空串按「未设置」处理（EMQX 允许匿名时不传）。
    let username = cfg.mqtt_user().unwrap_or_default();
    let password = cfg.mqtt_pass().unwrap_or_default();
    let keepalive_s = cfg.keepalive_s().unwrap_or(MQTT_KEEPALIVE_S as u16).max(1) as u64;

    // LWT 固定 `{"online":false}`（17 字节），retained —— 设备掉线时
    // broker 替我们发，上位机据此判断离线。
    let lwt_payload: &[u8] = b"{\"online\":false}";

    // **不要**用 `..Default::default()`：默认值里 `keep_alive_interval: Some(0s)`
    // 和 `crt_bundle_attach: None` 分别等于「关掉 keepalive 语义」和
    // 「关掉证书校验」，两个都是安全事故。逐字段写。
    let conf = MqttClientConfiguration {
        client_id: Some(&client_id),
        // `None` = IDF 默认协议版本（MQTT 3.1.1）。刻意不用 V5：
        // 上位机与 C 版都按 3.1.1 实现，改版本属于协议变更。
        protocol_version: None,
        connection_refresh_interval: Duration::from_secs(0),
        keep_alive_interval: Some(Duration::from_secs(keepalive_s)),
        // Some 才会打开自动重连；None 会让 IDF 直接 `disable_auto_reconnect`。
        reconnect_timeout: Some(Duration::from_millis(RECONNECT_BASE_MS as u64)),
        network_timeout: Duration::from_secs(0),
        lwt: Some(LwtConfiguration {
            topic: &topics.status,
            payload: lwt_payload,
            qos: QoS::AtLeastOnce,
            retain: true,
        }),
        disable_clean_session: false,
        task_prio: TASK_PRIO,
        task_stack: CB_TASK_STACK,
        buffer_size: BUFFER_SIZE,
        out_buffer_size: OUT_BUFFER_SIZE,
        outbox_limit: None,
        username: if username.is_empty() {
            None
        } else {
            Some(&username)
        },
        password: if password.is_empty() {
            None
        } else {
            Some(&password)
        },
        use_global_ca_store: false,
        skip_cert_common_name_check: false,
        // 必开：sdkconfig.defaults 里同时开了交叉签名根证书校验。
        // 为省体积关掉它是禁止的（mqtt-port.md §8.5）。
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        server_certificate: None,
        client_certificate: None,
        private_key: None,
        private_key_password: None,
    };

    // channel 必须在 client 之前建好：回调要捕获 tx。
    let (tx, rx) = mpsc::sync_channel::<MqttEvent>(EVENT_QUEUE);
    let (tx_req, rx_req) = mpsc::sync_channel::<MqttRequest>(4);
    let online = Arc::new(AtomicBool::new(false));

    let tx_cb = tx;
    let online_cb = Arc::clone(&online);

    let client = EspMqttClient::new_cb(&url, &conf, move |event: EspMqttEvent<'_>| {
        // ---- 回调红线：只做拷贝 + try_send ----
        let ev = match event.payload() {
            EventPayload::Connected(session_present) => {
                online_cb.store(true, Ordering::Relaxed);
                MqttEvent::Connected { session_present }
            }
            EventPayload::Disconnected => {
                online_cb.store(false, Ordering::Relaxed);
                MqttEvent::Disconnected
            }
            // `id` 是 broker 的 message id，我们不用它（ack 走自定义 payload）。
            EventPayload::Received {
                topic,
                data,
                details,
                ..
            } => {
                if details != Details::Complete {
                    // 命令报文远小于 buffer_size，正常不会分片。
                    // 真出现了宁可丢，也不在回调里拼包（要缓存状态）。
                    warn!("MQTT 收到分片报文（details={details:?}），已丢弃");
                    return;
                }
                MqttEvent::Command {
                    topic: topic.unwrap_or_default().to_owned(),
                    data: data.to_vec(),
                }
            }
            EventPayload::Error(e) => MqttEvent::Error(format!("{e:?}")),
            // BeforeConnect / Subscribed / Published / Deleted 等不关心。
            _ => return,
        };

        if let Err(TrySendError::Full(_)) = tx_cb.try_send(ev) {
            warn!("MQTT 事件队列已满，丢弃一个事件");
        }
    })?;

    let ctx = RunCtx {
        client,
        rx,
        rx_req,
        cfg,
        pulse,
        pc_state,
        time_synced,
        topics,
        device_id,
    };

    std::thread::Builder::new()
        .name("mqtt".into())
        .stack_size(TASK_STACK)
        .spawn(move || run(ctx))
        .map_err(|_| EspError::from_infallible::<ESP_FAIL>())?;

    info!("MQTT 线程已启动，目标 {url}");
    Ok(MqttHandle { tx: tx_req, online })
}

// ---------------------------------------------------------------------------
//  线程主体
// ---------------------------------------------------------------------------

fn run(mut ctx: RunCtx) -> ! {
    let mut verifier = CmdVerifier::new(&ctx.device_id);
    let mut last_synced = false;
    let mut subscribed = false;
    let mut next_status = Instant::now();

    loop {
        // SNTP 标志同步进 verifier。只在变化时写，避免每轮都碰。
        let synced = ctx.time_synced.load(Ordering::SeqCst);
        if synced != last_synced {
            verifier.set_time_synced(synced);
            last_synced = synced;
            if synced {
                info!("MQTT 线程已看到时间同步标志，指令校验开始放行");
            }
        }

        match ctx.rx.recv_timeout(Duration::from_millis(POLL_MS)) {
            Ok(MqttEvent::Connected { session_present }) => {
                info!("MQTT 已连接（session_present={session_present}）");
                match ctx.client.subscribe(&ctx.topics.cmd, QoS::AtLeastOnce) {
                    Ok(_) => {
                        subscribed = true;
                        info!("已订阅 {}", ctx.topics.cmd);
                    }
                    Err(e) => {
                        subscribed = false;
                        error!("订阅 {} 失败: {e}", ctx.topics.cmd);
                    }
                }
                if subscribed {
                    publish_status_online(&mut ctx);
                    publish_state(&mut ctx);
                    next_status = Instant::now() + status_period(&ctx);
                }
            }
            Ok(MqttEvent::Disconnected) => {
                warn!("MQTT 已断开（自动重连中）");
                subscribed = false;
            }
            Ok(MqttEvent::Command { topic, data }) => {
                handle_command(&mut ctx, &mut verifier, &topic, &data);
            }
            Ok(MqttEvent::Error(msg)) => error!("MQTT 传输错误: {msg}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // 事件通道断开只可能是 client 被 drop，正常不该发生。
                error!("MQTT 事件通道已断开，线程空转");
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
        }

        // 外部请求（控制台 / 主循环）
        while let Ok(req) = ctx.rx_req.try_recv() {
            match req {
                MqttRequest::PublishState => publish_state(&mut ctx),
            }
        }

        // 定时刷新 retained 状态。周期运行时可变，每轮重读 NVS。
        if subscribed && Instant::now() >= next_status {
            publish_state(&mut ctx);
            publish_status_online(&mut ctx);
            next_status = Instant::now() + status_period(&ctx);
        }
    }
}

/// 处理一条下行命令：校验 -> 投脉冲队列 -> 回 ack。
fn handle_command(ctx: &mut RunCtx, verifier: &mut CmdVerifier, topic: &str, data: &[u8]) {
    let pulse_ms = ctx
        .cfg
        .pulse_ms()
        .map(|p| p.get())
        .unwrap_or(PULSE_MS_DEFAULT);
    let ttl = ctx.cfg.cmd_ttl().unwrap_or(CMD_TTL_DEFAULT as u16);

    let result = match verifier.verify(topic, data, unix_now() as i64, pulse_ms, ttl) {
        Ok(width_ms) => match ctx.pulse.request(width_ms) {
            Ok(()) => "ok",
            // 队列满 = 上一次脉冲还没做完，对上位机就是「忙」。
            Err(PulseError::QueueFull) => "busy",
            Err(PulseError::TaskGone) => {
                error!("脉冲线程已消失，命令无法执行");
                "error"
            }
        },
        Err(e) => err_str(e),
    };

    publish_ack(ctx, result);
}

// ---------------------------------------------------------------------------
//  发布
// ---------------------------------------------------------------------------

fn publish_state(ctx: &mut RunCtx) {
    let on = match ctx.pc_state.lock() {
        Ok(g) => g.is_on(),
        Err(e) => {
            error!("PC 状态互斥锁中毒: {e}");
            return;
        }
    };
    let payload: &[u8] = if on {
        br#"{"pc":"on"}"#
    } else {
        br#"{"pc":"off"}"#
    };
    // retained：上位机订阅后立刻能拿到当前状态，不必等下一次发布。
    if let Err(e) = ctx
        .client
        .publish(&ctx.topics.state, QoS::AtLeastOnce, true, payload)
    {
        warn!("发布 state 失败: {e}");
    } else {
        info!("已上报 PC 状态: {}", if on { "on" } else { "off" });
    }
}

fn publish_status_online(ctx: &mut RunCtx) {
    let payload: &[u8] = br#"{"online":true}"#;
    if let Err(e) = ctx
        .client
        .publish(&ctx.topics.status, QoS::AtLeastOnce, true, payload)
    {
        warn!("发布 status 失败: {e}");
    }
}

/// ack 的 `nonce` **恒为空串**，与 C 版 `publish_ack()` 的行为一致
/// （`main.c:481-487` 传的是 `NULL`）。回填真实 nonce 属于协议变更，
/// 需上位机确认后再做，见 `docs/mqtt-port.md §8.2`。
fn publish_ack(ctx: &mut RunCtx, result: &str) {
    let payload = format!(r#"{{"nonce":"","result":"{result}"}}"#);
    if let Err(e) = ctx
        .client
        .publish(&ctx.topics.ack, QoS::AtLeastOnce, false, payload.as_bytes())
    {
        warn!("发布 ack 失败: {e}");
    } else {
        info!("已回执: {result}");
    }
}

// ---------------------------------------------------------------------------
//  小工具
// ---------------------------------------------------------------------------

fn status_period(ctx: &RunCtx) -> Duration {
    let s = ctx
        .cfg
        .status_period_s()
        .unwrap_or(STATUS_PUBLISH_S as u16)
        .max(1);
    Duration::from_secs(s as u64)
}

/// 当前 Unix 秒。时钟没设好时返回 0 —— `verify()` 在未同步时会先
/// 返回 `TimeNotSynced`，所以 0 不会造成误判。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn err_str(e: CmdError) -> &'static str {
    match e {
        CmdError::BadJson => "bad-json",
        CmdError::BadSchema => "bad-schema",
        CmdError::UnknownCmd => "unknown-cmd",
        CmdError::Expired => "expired",
        CmdError::Replay => "replay",
        CmdError::TimeNotSynced => "time-not-synced",
    }
}
