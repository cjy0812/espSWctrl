# MQTT 模块移植规格

目标：把 `main/` 下的 C 版 MQTT 实现（基于 ESP-IDF 的 `esp_mqtt_client`）移植为 `src/mqtt.rs`，在 esp-idf-svc 0.53 + ESP-IDF v6.1 + ESP32-C3 (`riscv32imc-esp-espidf`) 上运行。

范围：连接/重连、TLS、LWT、订阅下行命令、发布状态与 ACK、SNTP 对时结果与命令校验的接线。

不在范围：命令校验逻辑本身（`src/cmd_verify.rs` 已存在）、脉冲生成（`src/pulse.rs` 已存在）、DDNS、控制台。

---

## 1. C 版行为清单（逐项，带 main.c 行号）

路径前缀均为 `G:\GitHub\espSWctrl\main\main.c`。

| # | 行为 | 行号 | 关键细节 |
|---|---|---|---|
| 1.1 | `app_main` 启动顺序 | 551-632 | `pulse_init()` → `pc_state_init()`（仅 `ENABLE_STATUS_READBACK`）→ NVS 初始化（`NO_FREE_PAGES`/`NEW_VERSION` 时 erase 后重试）→ `cfg_init()` → `console_start()` → 读 MAC 算 device_id → 拼 4 个 topic → `cmd_verify_init()` → 建事件组 → 装任务看门狗 → `wifi_start()` → 等 `BIT_WIFI_UP` → 起 sntp/status 两个任务 → `mqtt_start()` → 主循环 |
| 1.2 | device_id 生成 | 567-573 | `snprintf(device_id, "psw-%02x%02x%02x", mac[3], mac[4], mac[5])`，即 MAC 末 3 字节 |
| 1.3 | topic 拼接 | 575-578 | `psw/%s/cmd`、`psw/%s/state`、`psw/%s/status`、`psw/%s/ack` 四个静态缓冲 |
| 1.4 | 命令校验器初始化 | 580 | `cmd_verify_init(s_device_id)`，内部固定期望 topic 并清零 nonce 环、`time_synced=false`（`cmd_verify.c:49-57`） |
| 1.5 | 任务看门狗 | 585-590 | `{timeout_ms=30000, idle_core_mask=0, trigger_panic=true}`，`esp_task_wdt_add(NULL)` 把 app_main 自己挂上 |
| 1.6 | 等 Wi-Fi 的写法 | 592-600 | 每 1 s 重置一次 WDT 的轮询循环；注释明确说明「`portMAX_DELAY` 会触发 30 s 看门狗」 |
| 1.7 | 起 SNTP 任务 | 602-603 | `xTaskCreate(time_sync_task, "sntp", 4096, NULL, 4, NULL)` |
| 1.8 | 起状态发布任务 | 604-605 | `xTaskCreate(status_task, "status", 3072, NULL, 3, NULL)` |
| 1.9 | 起 MQTT | 607 | `mqtt_start()`；失败只 `ESP_LOGE`，不 abort（606 行注释：不能让 MQTT 拖垮整机） |
| 1.10 | 主循环 | 609-631 | 每 5 s 重置 WDT 后 `vTaskDelay`，只打心跳日志 |
| 1.11 | SNTP 对时任务 | 400-421 | `ESP_NETIF_SNTP_DEFAULT_CONFIG("pool.ntp.org")` → `esp_netif_sntp_init` → `esp_netif_sntp_sync_wait(30000ms)`；成功则 `cmd_set_time_synced(true)` + 置 `BIT_TIME_SYNC`（413-414）；失败只 `ESP_LOGE`「SNTP 同步失败 —— 指令将一律被拒绝」；最后 `esp_netif_sntp_deinit()` + `vTaskDelete(NULL)` |
| 1.12 | MQTT client 配置 | 504-532 | uri `mqtts://%s:%d`（`MQTT_HOST`, `MQTT_PORT`）；`credentials.username/password`；`broker.verification.crt_bundle_attach = esp_crt_bundle_attach`（517 行注释「校验服务器证书 —— 必须，不能跳过」）；`session.keepalive = cfg_keepalive_s()`；`session.last_will = {s_topic_status, "{\"online\":false}", 17, qos=1, retain=1}`；`network.reconnect_timeout_ms = RECONNECT_BASE_MS`；`network.disable_auto_reconnect = false` |
| 1.13 | 注册与启动 | 524-531 | `esp_mqtt_client_init` → `esp_mqtt_client_register_event(..., ESP_EVENT_ANY_ID, mqtt_event_handler, NULL)` → `esp_mqtt_client_start` |
| 1.14 | CONNECTED 分支 | 453-461 | `s_mqtt_up = true`；`subscribe(s_topic_cmd, qos=1)`；向 status topic 发 `{"online":true}`（QoS1, retain1）；再调 `publish_state()` |
| 1.15 | DISCONNECTED 分支 | 463-466 | `s_mqtt_up = false`，仅日志 |
| 1.16 | DATA 分支 | 468-489 | 顶部注释「这里绝对不拉 GPIO。只做校验 + 入队，立即返回。」；`now = cmd_time_synced() ? time(NULL) : 0`；`cmd_verify(...)`；`CMD_OK` 时 `pulse_request(width)` 成功回 `accepted`、失败回 `busy`；其它结果回 `cmd_result_str(r)` |
| 1.17 | ERROR 分支 | 491-497 | 仅当 `error_type == MQTT_ERROR_TYPE_TCP_TRANSPORT` 才打 `esp_tls_last_esp_err` 与 `esp_transport_sock_errno` |
| 1.18 | `publish_state()` | 426-436 | `!s_mqtt_up` 直接返回；`#if ENABLE_STATUS_READBACK` 内才发；payload `{"pc":"on"}` / `{"pc":"off"}` 到 `buf[64]`；`publish(..., len=0, qos=1, retain=1)`（len=0 表示 strlen） |
| 1.19 | `publish_ack()` | 438-445 | `!s_mqtt_up` 直接返回；`buf[128]`；`{"nonce":"%s","result":"%s"}`，nonce 传 NULL 时写空串；`publish(..., qos=1, retain=0)` |
| 1.20 | `status_task` | 537-546 | 无限循环：`vTaskDelay(cfg_status_period_s()*1000)` → `publish_state()` → 打 PC 状态日志 |
| 1.21 | 命令校验语义 | `cmd_verify.c:86-168` | 逐字节 topic 比较 → `time_synced` 检查 → cJSON 解析 → 必需字段 `cmd`(str) / `nonce`(str) / `ts`(num) → `press` 用 `cfg_pulse_ms()`、`press_long` 硬编码 5000（会被 `pulse.c` 夹到 3000） → ttl 缺省/非法回退 `cfg_cmd_ttl()`，范围 `(0,300]` → `skew > ttl` 判过期 → FNV-1a nonce 环（32 槽）去重 |
| 1.22 | ACK 结果字符串 | `cmd_verify.c:62-74` | `ok` / `bad-json` / `bad-schema` / `unknown-cmd` / `expired` / `replay` / `time-not-synced` |
| 1.23 | Wi-Fi 重连退避 | 181-199 | `s_reconnect_tries++`；前 `RECONNECT_IMMEDIATE` 次用 `RECONNECT_FAST_MS`，之后 `RECONNECT_SLOW_MS`；延时后**重新检查 `s_scan_mode`** 再 `esp_wifi_connect()` |
| 1.24 | 拿到 IP | 200-212 | `s_reconnect_tries = 0`，置 `BIT_WIFI_UP` |
| 1.25 | 常量来源 | `main/app_config.h:17-41` | `PULSE_MS_MIN 100` / `PULSE_MS_MAX 3000` / `PULSE_MS_DEFAULT 300` / `CMD_TTL_DEFAULT 30` / `MQTT_KEEPALIVE_S 60` / `STATUS_PUBLISH_S 30` / `RECONNECT_BASE_MS 1000` / `RECONNECT_MAX_MS 60000` / `ENABLE_STATUS_READBACK 1` |
| 1.26 | `RECONNECT_MAX_MS` 的实际使用情况 | `main.c:504-532` | **C 版并未使用**。MQTT 只把 `RECONNECT_BASE_MS` 交给 IDF 作为初始重连间隔，之后由 IDF 自己退避；Wi-Fi 侧用的是另一组 `RECONNECT_IMMEDIATE/FAST_MS/SLOW_MS`。移植时不要假装它生效了 |

---

## 2. Topic 与 payload 格式

device_id 形如 `psw-a1b2c3`，下表用 `<id>` 简写。

| topic | 方向 | QoS | retain | payload |
|---|---|---|---|---|
| `psw/<id>/cmd` | 下行（设备订阅） | 1 | 0 | `{"cmd":"press"\|"press_long","nonce":"<str>","ts":<unix 秒>,"ttl":<秒，可选>}` |
| `psw/<id>/state` | 上行（设备发布） | 1 | **1** | `{"pc":"on"}` 或 `{"pc":"off"}` |
| `psw/<id>/status` | 上行（设备发布 + LWT） | 1 | **1** | `{"online":true}`；LWT 固定 `{"online":false}`（17 字节） |
| `psw/<id>/ack` | 上行（设备发布） | 1 | 0 | `{"nonce":"<str>","result":"<结果>"}` |

字段说明：

- `cmd`：只有 `press` / `press_long` 两个合法值。**绝不提供 on/off** —— 电源键是「按一下翻转」，不是开关（`cmd_verify.c:127-128`）。
- `nonce`：任意非空字符串，设备侧做 FNV-1a 32 位哈希后在环形缓冲里去重（`cmd_verify.c:21-47`）。Rust 版 `NONCE_HISTORY = 16`（`src/consts.rs:29`），**比 C 版的 32 槽少一半**，属已知差异。
- `ts`：Unix 秒。设备时间未同步时一律拒绝，不会用 `ts` 做任何推断。
- `ttl`：可选，缺省或非法（`<=0` 或 `>300`）时回退到 NVS 里的 `cmd_ttl`。
- `result` 取值见 §1.22。

**已知差异（必须记录）**：C 版 `publish_ack()` 调用点（`main.c:481-487`）把 nonce 参数传 `NULL`，因此 C 版实际发出的 ack 里 `nonce` 恒为空串。Rust 版应在 `verify()` 成功后把真实 nonce 回填到 ack —— 但这是与上位机的协议变更，需确认对端是否依赖空串（见 §8）。

---

## 3. esp-idf-svc 0.53 API 确认结果

源码路径：`G:\Dev\cargo\registry\src\index.crates.io-1949cf8c6b5b557f\esp-idf-svc-0.53.0\src\mqtt\client.rs`（`CARGO_HOME=G:\Dev\cargo`）。

标注口径：
- **源码确认** = 直接读了上述 crate 源码，行号可查。
- **文档确认** = 只从 rustdoc 注释 / docs.rs 看到，未逐行核对。
- **未确认** = 没有任何依据，禁止在实现中依赖。

### 3.1 源码确认

| 项 | 内容 |
|---|---|
| 客户端类型 | `EspMqttClient<'static>`；`pub fn new(url: &str, conf: &MqttClientConfiguration) -> Result<(Self, EspMqttConnection), EspError>`（client.rs:412） |
| 回调式构造 | `pub fn new_cb<F>(url, conf, callback: F) -> Result<Self, EspError> where F: for<'b> FnMut(EspMqttEvent<'b>) + Send + 'static` |
| 非 static 回调 | `unsafe fn new_nonstatic_cb`，允许借用捕获（需自行保证生命周期） |
| 配置结构 | `MqttClientConfiguration<'a>`（client.rs:82），字段含 `client_id`、`keep_alive_interval: Option<Duration>`、`reconnect_timeout: Option<Duration>`、`network_timeout`、`lwt: Option<LwtConfiguration>`、`task_prio`、`task_stack`、`buffer_size`、`out_buffer_size`、`outbox_limit`、`username`、`password`、`crt_bundle_attach: Option<unsafe extern "C" fn(*mut c_void) -> esp_err_t>`、`skip_cert_common_name_check`、`server_certificate`、`client_certificate`、`private_key` |
| LWT 字段名 | `LwtConfiguration { topic: &'a str, payload: &'a [u8], qos: QoS, retain: bool }` —— 字段是 **`payload`**，不是 `lwt_message` |
| 配置映射 | `TryFrom<&MqttClientConfiguration> for esp_mqtt_client_config_t`（client.rs:262+）：`keep_alive_interval: None` → `disable_keepalive = true`；`reconnect_timeout: Some(d)` → `network.reconnect_timeout_ms = d` 且 `disable_auto_reconnect = false`；`None` → `disable_auto_reconnect = true`；`lwt` → `session.last_will{topic, msg, msg_len, qos, retain}` |
| 默认值陷阱 | `Default`（client.rs:125-168）里 `keep_alive_interval: Some(0s)`、`reconnect_timeout: Some(0s)`、`crt_bundle_attach: None`。**必须逐字段覆盖**，不能 `..Default::default()` 了事 —— 默认值等于关掉 keepalive 语义 + 关闭证书校验 |
| 订阅 | `pub fn subscribe(&mut self, topic: &str, qos: QoS) -> Result<MessageId, EspError>`（client.rs:594） |
| 发布 | `pub fn publish(&mut self, topic: &str, qos: QoS, retain: bool, payload: &[u8]) -> Result<MessageId, EspError>`（client.rs:602） |
| 发送能力 | `unsafe impl Send for EspMqttClient<'_>`（client.rs:788）—— 可以放进 `Mutex` 供多线程共享 |
| 事件轮询 | `EspMqttConnection::next(&mut self) -> Result<&EspMqttEvent<'_>, EspError>`（client.rs:797）。**无新事件时返回 `Err(ESP_ERR_INVALID_STATE)`，不是阻塞 recv** |
| 事件枚举 | `EspMqttEvent::payload() -> EventPayload`，变体：`Error` / `BeforeConnect` / `Connected(session_present)` / `Disconnected` / `Subscribed` / `Unsubscribed` / `Published` / `Received { id, topic: Option<&str>, data: &[u8], details }` / `Deleted` |
| 事件安全 | `unsafe impl Send`/`Sync for EspMqttEvent`；**未知 event id 会 panic** |
| 异步变体 | `EspAsyncMqttClient`（client.rs:844+）：`new` / `new_with_caps` / `wrap` / `wrap_with_caps`，内部起 "MQTT Sending task"（4096 栈），提供 async `subscribe` / `publish` |

### 3.2 关键结论：回调阻塞在 `share()` 上（既定事实，直接采用）

`EspMqttClient::new()` 内部的回调闭包最后一句是 `sender.channel().share(event);`。`share()` 是**有界通道的阻塞发送**。含义：

1. 回调运行在 **IDF 的 MQTT 任务上下文**里，不是我们的线程。
2. 回调一旦阻塞，整个 MQTT 任务停摆 —— 收不到包、发不出 keepalive、LWT 可能被误触发。
3. 因此回调里**绝对不能**做任何可能长时间阻塞的事：不能 `publish()`（内部要拿 `MQTT_API_LOCK`，与 MQTT 任务自身互锁）、不能等锁、不能 `sleep`、**绝不能拉 GPIO**（脉冲 3000ms 会把 MQTT 任务钉死 3 秒，远超 keepalive 余量）。

这条直接决定了 §4 的线程模型：**回调只做「拷贝 + 入队」，其余一切在别的线程做。**

### 3.3 文档确认

| 项 | 内容 |
|---|---|
| `QoS` 枚举 | `QoS::AtMostOnce` / `AtLeastOnce` / `ExactlyOnce`，来自 `embedded_svc::mqtt::client`，语义与 MQTT 规范一致 |
| `MqttProtocolVersion` | `V3_1` / `V3_1_1` / `V5(cfg)`；不指定时按 IDF 默认（3.1.1） |
| `crt_bundle_attach` | 传 `Some(esp_idf_svc::sys::esp_crt_bundle_attach)` 可启用 ESP x509 证书包校验，对应 C 版的 `broker.verification.crt_bundle_attach` |

### 3.4 未确认

| 项 | 为什么没确认 | 影响 |
|---|---|---|
| 回调线程的实际栈大小与 `task_stack` 字段的对应关系 | 源码里 `task_stack` 直传 `config.task_stack`，但 IDF 对 0 值有默认值，具体数值未查 IDF 源码 | 若在回调里做稍重的栈分配可能溢出；§4 已按「回调只入队」规避 |
| `Received` 分片（`Details::InitialChunk`/`SubsequentChunk`）在 `buffer_size` 足够时是否仍会出现 | 未做硬件验证 | 命令报文很短（<200B），设 `buffer_size` 4096 后预期单包；但实现里**必须**处理 `Complete` 以外的分片或显式丢弃并记日志 |
| `EspMqttConnection::next()` 返回 `Err(ESP_ERR_INVALID_STATE)` 时是否可能有其他 `EspError` 也代表「无事件」 | 未穷举 | 轮询实现里应按错误码精确匹配，其余错误记日志 |
| `reconnect_timeout` 的退避是否真的有上限（是否读 `RECONNECT_MAX_MS`） | 该值由 IDF 内部 `esp_mqtt_client` 实现，未读 IDF 源码 | 见 §4 Q5：**不能依赖 IDF 的退避来省 Serverless 额度** |

---

## 4. 线程模型与回调职责边界

### Q1 线程模型结论：**独立 "mqtt" 线程 + 事件通道，不用主循环 tick**

理由：

1. `EspMqttConnection::next()` 是非阻塞轮询（无事件返回 `Err`），主循环 5 s 一次的话，命令到达后最多延迟 5 s 才被处理 —— 对「按电源键」这种交互不可接受。
2. 用 `new_cb` + `std::sync::mpsc`（或 `SyncSender`，与 `pulse.rs` 同款）：回调里只做 `try_send`，**非阻塞**，满足 §3.2 的约束。
3. 独立线程可以 `recv_timeout`，既不会忙等烧 CPU，也不会像主循环那样受 5 s 周期拖累。
4. 与仓库既有风格一致：`pulse.rs` 是 `SyncSender<u16>` + 专用线程，`time_sync_loop` 是专用线程。mqtt 沿用同一套。

**不选主循环 tick 的具体代价**：`ddns_tick` 那种 tick 模式适合「分钟级」任务；命令通道是「秒级」甚至「亚秒级」的，塞进 5 s 主循环等于自造 5 s 延迟。

线程形态（描述，不写代码）：

- 名字 `"mqtt"`，栈 `MQTT_TASK_STACK = 8192`（与 `TIME_SYNC_STACK` 同级；里面有 JSON 解析和 `publish` 调用链）。
- 循环体：`recv_timeout(1s)` 取事件 → 匹配 → 处理；超时分支顺便检查「该不该发周期状态」（见 Q4）。
- 线程**只**持有 `EspMqttClient`（`Mutex` 包一层，因为 `publish`/`subscribe` 要 `&mut self`）和 `CmdVerifier`（`Mutex` 或线程独占）。

### Q2 回调职责边界

**允许**（全部是 O(1)、无锁、无 IO）：

- 把 `EventPayload` 归类成自己的小枚举：`Connected` / `Disconnected` / `Command { topic: heapless 或 Vec<u8>, data: Vec<u8> }` / `TransportError { tls_err, sock_errno }` / `Other`。
- `try_send` 到 mpsc；队列满则**丢弃并计数**（命令丢弃优于阻塞 MQTT 任务）。
- 打 `log::info!` / `log::warn!`（`EspLogger` 内部是 `esp_log_write`，不阻塞；但仍应克制，避免高频重连时刷屏）。

**禁止**（红线）：

- 任何 `publish` / `subscribe` —— 会拿 `MQTT_API_LOCK`，与回调所在任务自锁。
- 任何 `std::thread::sleep` / 阻塞 `send` / `Mutex::lock` 长时间等待。
- **任何 GPIO 操作**。这是项目安全线第 2 条：脉冲只在 `pulse` 线程里生成。回调里最多 `pulse.request(width)`，而它内部是 `try_send`，非阻塞 —— 但即便如此，**校验也不该在回调里做**（JSON 解析 + 时间窗计算需要 `CmdVerifier` 的可变借用，放回调里要么加锁要么破坏结构）。
- 任何 NVS 读写（`Cfg` 的 `mqtt_host()` 等会走 NVS，慢）。

**回调栈大小**：`MqttClientConfiguration::task_stack` 显式设 **4096**。回调里只做栈上小对象 + `Vec` 堆分配，4 KB 足够；不设会吃 IDF 默认值（未确认，§3.4），风险不可控。

### Q3 `time_synced` 如何到达 `CmdVerifier`

**结论：用 `Arc<AtomicBool>` 共享标志，不用 setter 跨线程调用。**

现状盘点：

- `CmdVerifier::set_time_synced(&mut self, synced: bool)`（`src/cmd_verify.rs:129`）需要 `&mut self`。
- `CmdVerifier::verify(...)` 也要 `&mut self`（nonce 环要写）。
- 因此 `CmdVerifier` 本身必然被 `Mutex` 包住，或由 "mqtt" 线程独占。

两条路：

1. **setter 直调**：SNTP 线程拿到 `CmdVerifier` 的 `Mutex` 后 `set_time_synced(true)`。可行，但把 SNTP 线程和 mqtt 线程耦合到同一把锁上 —— SNTP 对时最长 60 s 阻塞（`TimeSync::sync()`），虽然锁只在调用 setter 的一瞬间持有，但**锁竞争的存在本身就是「MQTT 连上了但命令没反应」的一个潜在来源**。
2. **AtomicBool 共享**（推荐）：`Arc<AtomicBool>` 由 `main.rs` 创建，`time_sync_loop` 成功时 `store(true, SeqCst)`，"mqtt" 线程每次 `verify()` 前 `load(SeqCst)` 并同步进 `CmdVerifier`（`if flag != last_seen { verifier.set_time_synced(flag); last_seen = flag; }`）。

**必须点名的失败模式**：「MQTT 连上了但命令没反应」的根因是 `CmdVerifier.time_synced` 永远是 `false`，`verify()` 在 `src/cmd_verify.rs:185-187` 直接返回 `CmdError::TimeNotSynced`，设备会把 ack `time-not-synced` 发出去（如果 ack 路径也通了的话）。**排查顺序**：先看 ack 是不是 `time-not-synced`；再看 SNTP 线程是否真的走到了 `Ok(())` 分支；最后看 AtomicBool 有没有被 `store(true)`。

精确接线点见 §6。

---

## 5. 文件清单

本节只列**需要改动或新建的文件**，以及每个文件里要有/要有哪几块。不改的文件（`pulse.rs`、`pc_state.rs`、`ddns.rs`、`console.rs`）不在此列。

### 5.1 新建 `src/mqtt.rs`

模块职责：**持有 `EspMqttClient`、跑事件循环、执行命令校验、发出 ack / state / status。** 结构建议按下面五块组织，顺序即实现顺序：

| 块 | 内容 | 备注 |
|---|---|---|
| (a) 常量 | `MQTT_EVENT_QUEUE = 8`（事件通道容量）、`MQTT_TASK_STACK = 8192`（"mqtt" 线程栈）、`MQTT_TASK_PRIO = 5`、`MQTT_TASK_STACK_CB = 4096`（`MqttClientConfiguration::task_stack`）、`MQTT_BUFFER_SIZE = 4096`、`MQTT_OUT_BUFFER_SIZE = 2048`、`ACK_BUF = 160`、`STATE_BUF = 32` | 时间周期类（`STATUS_PUBLISH_S` 等）继续用 `consts.rs` 的，不在这里重复定义 |
| (b) 事件枚举 | `enum MqttEvent { Connected { session_present: bool }, Disconnected, Command { topic: Vec<u8>, data: Vec<u8> }, TransportError { tls_err: i32, sock_errno: i32 }, Dropped }` | 对应 §4 Q2 的分类。`Dropped` 用于队列满时计数日志，不是真的入队 |
| (c) 线程函数 | `pub fn run(ctx: MqttCtx) -> !` | 这是 "mqtt" 线程的入口。内部：建 client（回调只做 §4 Q2 的「拷贝 + try_send」）→ `loop` 里 `try_send`/`recv` 事件 → `publish_state` / `publish_status` 定时 → `next()` 轮询出事件 → 逐条处理 → `handle_command()` 校验 + 入脉冲队列 + 发 ack |
| (d) 发布/订阅 API | `fn publish_state(&mut self, on: bool)`、`fn publish_status_online(&mut self, online: bool)`、`fn publish_ack(&mut self, nonce: &str, result: &str)`、`fn subscribe_cmd(&mut self)` | 全部只在 "mqtt" 线程内调用 —— 即 §3.2 那条红线的**结构性**保证：这些函数是 `&mut self` 方法，而 `client` 字段不外借 |
| (e) 对外暴露面 | `pub struct MqttHandle { tx: SyncSender<MqttRequest> }` + `pub enum MqttRequest { PublishState, PublishStatusOnline(bool), Stop }`；`pub fn init(...) -> Result<MqttHandle, EspError>`；`MqttHandle::request_state(&self)` 用 `try_send` | 非阻塞。控制台 / 主循环只可能经这条通道提需求，**永远拿不到 client** |

`MqttCtx`（线程启动参数，所有权一次交出去）：

```rust
pub struct MqttCtx {
    pub client: EspMqttClient<'static>,   // 已 subscribe 前，先建后传入；或在线程内建
    pub rx: std::sync::mpsc::Receiver<MqttEvent>,
    pub rx_req: std::sync::mpsc::Receiver<MqttRequest>,
    pub cfg: Arc<Cfg>,
    pub pulse: PulseHandle,
    pub pc_state: PcStateHandle,
    pub time_synced: Arc<AtomicBool>,
    pub device_id: String,
    pub topics: Topics,                   // cmd/state/status/ack 四个 String
}
```

> 顺序注意：`EspMqttClient::new()` 的回调闭包里要 `try_send` 到 `tx`，所以 **channel 必须在 client 之前建好**，`tx` 移进闭包、`rx` 留给线程。回调签名是 `FnMut(EspMqttEvent<'_>) + Send + 'static`，闭包里只能捕获 `SyncSender<MqttEvent>`、`Arc<Cfg>` 这类 `Send` 值，不能捕获 `&mut Cfg`。

### 5.2 `src/main.rs`

| 位置 | 改动 |
|---|---|
| `mod` 列表（28-35 行） | 加 `mod mqtt;` |
| `use` 区（37-49 行） | 加 `use std::sync::atomic::AtomicBool`（已有）、`use std::sync::mpsc::sync_channel` |
| `struct Backend`（60-81 行） | 加字段 `pub mqtt: Option<MqttHandle>`。用 `Option` 是因为 MQTT 初始化可能失败（C 版 `main.c:606-607` 的语义：失败只 `ESP_LOGE`，不 abort） |
| `app_main` 里 `time_sync` 字段构造之后（≈521 行） | 加 `let time_synced = Arc::new(AtomicBool::new(false));`，塞进 `Backend` 和下面的 SNTP 线程 |
| `time_sync_loop(backend)` 的 spawn（540-544 行） | 把 `time_synced` 的 `Arc::clone` 也传进去：`.spawn(move || time_sync_loop(backend, time_synced))`，函数签名同步改 |
| SNTP 线程 spawn 之后、主循环之前 | 建 channel → `MqttHandle` 可能失败 → 成功则 `std::thread::Builder::new().name("mqtt".into()).stack_size(MQTT_TASK_STACK).spawn(move || mqtt::run(ctx))`。**与 C 版一致：失败只记日志，不影响脉冲/控制台/DDNS** |
| 主循环（565-589 行） | 心跳日志可加 `MQTT = 已连/未连`（由 `MqttHandle` 内部的 `Arc<AtomicBool>` 或状态标志读出）。这一项可选 |

`Backend` 是 `&'static`（`Box::leak`），所以 `MqttHandle` 一旦放进去就活到进程结束，控制台命令可以直接 `backend.mqtt.as_ref()`。

### 5.3 `src/wifi.rs` 的 `time_sync_loop`

> 注：`time_sync_loop` 实际在 **`src/main.rs:138-178`**（`src/wifi.rs` 里是 `TimeSync` 单例本身，`sync()` 在这里被调用）。下文按真实位置写。

精确插入点 —— `src/main.rs:164-168`：

```rust
        match time_sync.sync() {
            Ok(()) => {
                log::info!("SNTP 对时完成，当前时间戳 {}", unix_now());
                return;                       //  <- 现在直接 return
            }
```

改成：

```rust
        match time_sync.sync() {
            Ok(()) => {
                // 必须在 return 之前 store：CmdVerifier 靠这个标志放行命令。
                time_synced.store(true, Ordering::SeqCst);
                log::info!("SNTP 对时完成，当前时间戳 {}", unix_now());
                return;
            }
```

**`store` 放在 `log::info!` 之前**：日志可能被丢弃（`log` 级别过滤），标志不能。`return` 之后这个线程就结束了，所以这是唯一机会。

### 5.4 `src/consts.rs`

现有常量对照（`src/consts.rs` 实读）：

| 常量 | 值 | 行号 | 是否够用 |
|---|---|---|---|
| `MQTT_KEEPALIVE_S` | 60 | 48 | 够。映射到 `keep_alive_interval: Some(Duration::from_secs(60))` |
| `STATUS_PUBLISH_S` | 30 | 50 | 够。**但目前无任何消费端**（`rg STATUS_PUBLISH_S` 只命中定义），见 §7 阶段 3 |
| `RECONNECT_BASE_MS` | 1000 | 52 | 够，映射 `reconnect_timeout` |
| `RECONNECT_MAX_MS` | 60_000 | 54 | **不要用** —— 见 §8 第 3 条，C 版并未把它交给 IDF |
| `DEFAULT_DEV_ID` | `"pc01"` | 63 | 够，作为无 MAC 时的兜底（本机正常走 MAC 派生） |
| `MQTT_CLIENT_SUFFIX` | `"-esp32"` | 67 | 够，client_id = `device_id + suffix` |
| `MQTT_PORT` | 8883 | 71 | 够 |

**真正需要新增的**（建议放 `src/mqtt.rs` 内部常量，不必进 `consts.rs`，因为它们不跨模块）：

- `MQTT_BUFFER_SIZE: usize = 4096` —— 收包缓冲。命令报文 <200B，留足余量防 §3.4 的分片问题。
- `MQTT_OUT_BUFFER_SIZE: usize = 2048` —— 发包缓冲。
- `MQTT_EVENT_QUEUE: usize = 8` —— 回调 `try_send` 的通道容量。满即丢，见 §4 Q2。
- `ACK_BUF: usize = 160` —— 对应 C 版 `publish_ack` 的 `buf[128]`，nonce 最长按 64 计，留余量。

`device_id` 派生（对应 `main.c:567-573`）建议放 `main.rs` 或 `mqtt.rs` 的 `fn device_id_from_mac(mac: &[u8]) -> String`，格式 `format!("psw-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5])`。

### 5.5 `Cargo.toml` —— **结论：不需要新增依赖**

证据链（三层，逐一核实）：

1. **`esp-idf-svc` 0.53.0 确实 re-export 了 MQTT 客户端整套 API。**
   - `G:\tmp\svc-unpack\esp-idf-svc-0.53.0\src\lib.rs:88-93`：
     ```rust
     #[cfg(all(
         feature = "alloc",
         any(esp_idf_comp_mqtt_enabled, esp_idf_comp_espressif__mqtt_enabled),
         esp_idf_comp_esp_event_enabled
     ))]
     pub mod mqtt;
     ```
   - `…\src\mqtt.rs:5`：`pub mod client;`
   - `…\src\mqtt\client.rs:21-23`：
     ```rust
     pub use embedded_svc::mqtt::client::{
         Details, Event, EventPayload, InitialChunkData, MessageId, QoS, SubsequentChunkData,
     };
     ```
     以及同文件 `pub struct MqttClientConfiguration`（:82）、`pub struct EspMqttClient`（:398）、`pub struct EspMqttConnection`（:790）、`pub struct EspMqttEvent`（:1091）、`pub struct LwtConfiguration`（:74）。
   - 因此 `use esp_idf_svc::mqtt::client::{...}` 即可，**无需在 `Cargo.toml` 加 `embedded-svc`**。
2. **启用 `pub mod mqtt` 的 cfg 门是 `esp_idf_comp_espressif__mqtt_enabled`**，这个 cfg 由 `esp-idf-sys` 构建脚本按 `extra_components.toml` 生成。
3. 本项目已有 `extra-components.toml:6-7`：
   ```toml
   [[extra_components]]
   remote_component = { name = "espressif/mqtt", version = "^1.1.0" }
   ```
   且 `.cargo/config.toml:39-46` 已设置让 `esp-idf-sys` 生成带 `espressif/mqtt` 的 `idf_component.yml`（并注明升级 mqtt 版本时要清 `$CARGO_TARGET_DIR/riscv32imc-esp-espidf/release/build/esp-idf-sys`）。

**唯一要确认的是 `alloc` feature 是开的**（`lib.rs:88` 的 `feature = "alloc"` 门）。本项目用的是 `esp-idf-svc = "0.53"`（`Cargo.toml:18`），其默认 feature 包含 `alloc` —— `…\Cargo.toml:43-49` 的 `[features]` 里 `alloc` 是默认集的一部分。所以不需要额外声明。若实现时 `cargo check` 报 `esp_idf_svc::mqtt` 不存在，第一件事是查两处：(1) `CARGO_TARGET_DIR` 下 `esp-idf-sys` 产物是否残留旧的 `idf_component.yml`；(2) `cargo tree -e features -i esp-idf-svc` 看 `alloc` 是否被关掉。**不是去加依赖。**

---

## 6. NTP 结果 → `CmdVerifier` 的精确接线

这一节写的是**最容易静默失效**的一段：SNTP 明明成功了，MQTT 也连上了，设备却对每一条命令都回 `time-not-synced`。原因是 `CmdVerifier` 里那个 `bool` 从来没人置位 —— 它不会报错，只会一直拒绝。

### 6.1 现状（实读源码）

| 位置 | 内容 | 影响 |
|---|---|---|
| `src/cmd_verify.rs:129` | `pub fn set_time_synced(&mut self, synced: bool)` | 需要 **`&mut self`**，所以调用方必须持有 `CmdVerifier` 的独占访问权 |
| `src/cmd_verify.rs:122` | 构造时 `time_synced: false` | 默认值是「未同步」，即**默认全拒** |
| `src/cmd_verify.rs:185-187` | `if !self.time_synced { return Err(CmdError::TimeNotSynced); }` | 未同步时 `verify()` 在第 3 步就返回，后面 JSON 解析、ts 校验、nonce 去重**全都不执行** |
| `src/main.rs:164-168` | `match time_sync.sync() { Ok(()) => { log::info!(...); return; } ... }` | 成功分支目前只打日志就 `return`，**没有任何状态外传** |
| `src/main.rs:138` | `fn time_sync_loop(backend: &'static Backend)` | 函数签名里没有时间同步标志，需要加参数 |

### 6.2 接线方案（按此改，不要自创）

**第 1 步 —— `main.rs` 创建标志并分发所有权**

在 `Backend` 构造之前（即 `src/main.rs:515` 的 `Box::leak` 之前）加：

```rust
// SNTP 成功 -> true，只写一次；mqtt 线程轮询读。
let time_synced: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
```

`Arc` 复制两份：一份进 `MqttCtx`（mqtt 线程独占），一份进 SNTP 线程。**不要**塞进 `Backend` 再从 `Backend` 里读 —— `time_sync_loop` 拿到的是 `&'static Backend`，从那里读标志等于让 SNTP 线程依赖自己的输出，绕了一圈还多了个不必要的共享点。

**第 2 步 —— 改 `time_sync_loop` 签名**

```rust
fn time_sync_loop(backend: &'static Backend, time_synced: Arc<AtomicBool>) {
```

spawn 处（`src/main.rs:540-544`）：

```rust
std::thread::Builder::new()
    .name("sntp".into())
    .stack_size(TIME_SYNC_STACK)
    .spawn(move || time_sync_loop(backend, time_synced))
    .expect("SNTP 线程创建失败");
```

注意这里 `time_synced` 已经被 `move` 进闭包；要同时给 mqtt 线程，得在此之前 `let time_synced_mqtt = Arc::clone(&time_synced);`，两份各走各的。

**第 3 步 —— 成功分支 `store`（精确到行）**

`src/main.rs:164-168`，`Ok(())` 分支内、`log::info!` 之前插入一行：

```rust
match time_sync.sync() {
    Ok(()) => {
        time_synced.store(true, Ordering::SeqCst);   // <- 新增，必须在这
        log::info!("SNTP 对时完成，当前时间戳 {}", unix_now());
        return;                                       // 线程到此结束
    }
    Err(e) => { ... }
}
```

三条纪律：

1. **`store` 必须在 `return` 之前**。这个线程 `return` 后直接结束，之后再没有任何机会写这个标志。
2. **`store` 放在 `log::info!` 之前**。日志可能被 `log` 级别过滤丢掉，标志不能依赖日志是否输出。
3. **只置 `true`，永远不置回 `false`**。SNTP 成功后系统时钟只会单调前进（RTC 保持），置回 false 会让设备突然开始拒命令。

**第 4 步 —— mqtt 线程内同步进 `CmdVerifier`**

`CmdVerifier` 由 mqtt 线程独占（§4 Q3 结论），所以**不需要锁**。在每次 `verify()` 之前做一次「本地缓存 vs 共享标志」的比较：

```rust
// 线程局部变量，跨事件循环轮次保持
let mut last_time_synced = false;

// ... 收到 Command 事件后：
let now_synced = time_synced.load(Ordering::SeqCst);
if now_synced != last_time_synced {
    verifier.set_time_synced(now_synced);
    last_time_synced = now_synced;
    log::info!("CmdVerifier 时间同步标志 -> {now_synced}");
}

let now_ts = if now_synced { unix_now() as i64 } else { 0 };
let res = verifier.verify(&topic, &data, now_ts, cfg_pulse_ms, cfg_cmd_ttl);
```

要点：

- **`now_ts` 必须和 `set_time_synced` 用同一个 `now_synced` 值**。不要一个读 AtomicBool、一个调 `CmdVerifier::time_synced()` —— 中间没有竞态（同一线程），但代码上留出「两个来源」的口子，日后改一处忘另一处就是 bug。
- `last_time_synced` 是**缓存**不是状态源；状态源只有 `Arc<AtomicBool>`。
- 如果实现时 mqtt 线程内部有多个入口都会调 `verify()`（例如把 `handle_command` 拆开），把这段同步逻辑收进一个 `fn sync_time_flag(&mut self)` 里，避免漏调。

### 6.3 排查顺序（「命令没反应」时按此走）

按顺序，不要跳步：

1. **看 ack 内容**。上位机收到的 ack 如果 `result == "time-not-synced"`，问题就在本节；如果 ack 压根没到，先查 §3.2 那条红线是不是被破坏了（回调里 publish 导致自锁），或者 MQTT 根本没连上。
2. **看 SNTP 线程是否真的走到了 `Ok(())`**。串口日志里应出现 `SNTP 对时完成，当前时间戳 <非 0 数字>`。没有这行 → `TimeSync::sync()` 一直在失败（NTP 不可达 / Wi-Fi 未连），看紧邻的 `SNTP 对时失败: ...` warn。
3. **看 AtomicBool 是否被 store**。如果第 2 步的日志出现了，但 ack 仍是 `time-not-synced`，那就是第 3/4 步接错了：先确认 `store` 真的写在 `Ok(())` 分支里（不是写在 `Err` 分支），再确认 mqtt 线程的 `last_time_synced` 比较逻辑被执行到（加一条 `log::info!` 验证）。
4. **最后才怀疑时钟**。`unix_now()` 返回 0 说明系统时间没设 —— 但那样 SNTP 也不会返回 `Ok(())`，与本节无关。

---

## 7. 实施顺序

### 7.0 总体约束

- **每个阶段只动自己列出的文件**。`src/mqtt.rs` 是唯一新建文件，其余全是接线。`sdkconfig.defaults` 只在阶段 5 碰。
- **每阶段结束跑 `cargo check`**（约 10 分钟，`G:\Dev\ESP\idf.cmd` 里跑），不要直接上 50 分钟 LTO 链接。见 `CLAUDE.md` 第三节。
- 阶段 1 与阶段 4 **互不依赖**，可以并行；阶段 2 必须等阶段 1 的对外 API 定稿（哪怕函数体全是 `todo!()`）。

### 阶段 1 —— `src/mqtt.rs` 骨架（独占文件：`src/mqtt.rs`）

**先定 API 面，后写内部。** 先把下面这些名字和签名写死，让阶段 2 能对着编译，再填空：

| 项 | 形态 | 说明 |
|---|---|---|
| 模块常量 | `MQTT_BUFFER_SIZE` / `MQTT_OUT_BUFFER_SIZE` / `MQTT_EVENT_QUEUE` / `ACK_BUF` / `MQTT_TASK_STACK` | 见 §5.4，**不进 `consts.rs`** |
| `enum MqttEvent` | `Connected` / `Disconnected` / `Command { topic: String, data: Vec<u8> }` / `Error(u32)` | 跨线程的**已拷贝**事件；`String`/`Vec` 是为了不把 `&[u8]` 的 IDF 生命周期带出回调 |
| `pub struct MqttHandle` | `tx: SyncSender<MqttEvent>`（或发布用 `Arc<Mutex<EspMqttClient>>`）+ `online: Arc<AtomicBool>` | 对外只暴露 §5.2 用到的面：`is_online()`、`publish_state(bool)`、`publish_ack(&str, &str)`、`publish_online(bool)` |
| `pub fn init(ctx: MqttCtx) -> Result<MqttHandle, EspError>` | 建 channel → 建 client（回调只 `try_send`）→ 返回 handle | 回调里**只做拷贝 + try_send**，见 §3.2 / §4 Q2 |
| `pub fn run(ctx: MqttCtx)` | 线程主体：`loop { client.connection().next() ... }` + 处理 `rx` | 所有 `verify()` / 脉冲请求 / publish 都在这里做 |

`MqttCtx` 建议字段：`rx: Receiver<MqttEvent>`、`client: Arc<Mutex<EspMqttClient<'static>>>`、`verifier: CmdVerifier`、`pulse: &'static PulseHandle`（或 `Arc`）、`cfg: &'static Cfg`、`time_synced: Arc<AtomicBool>`、`pc_state: Option<PcState<'static>>`、`online: Arc<AtomicBool>`。

内部线程循环的要点：

- `next()` 无事件返回 `Err(ESP_ERR_INVALID_STATE)`，**按错误码精确匹配**再 `continue`，其余错误打日志（§3.4 第 3 项）。
- 主循环里用 `rx.recv_timeout(...)` 或「非阻塞 drain + 短 sleep」驱动，**不要**在持有 `client` 锁时 `sleep`。
- `Received` 只处理 `Details::Complete`；其余分片记日志并丢弃（§3.4 第 2 项）。

**验收**：`cargo check` 过；`mqtt.rs` 里不出现 `pub fn` 之外的 `pub` 字段（对外面收敛）。

### 阶段 2 —— 接线（独占文件：`src/main.rs`、`src/wifi.rs`）

依赖阶段 1 的 API 面。按 §5.2 / §5.3 / §6.2 逐条改：

1. `main.rs` 加 `mod mqtt;`、`Backend.mqtt: Option<MqttHandle>`（失败不 abort，对应 `main.c:527` 之后只 `ESP_LOGE` 的语义）。
2. `main.rs` 创建 `Arc<AtomicBool>`，一份进 SNTP 线程、一份进 `MqttCtx`（§6.2 第 1 步）。
3. `time_sync_loop` 签名加参数，`Ok(())` 分支 `store(true, SeqCst)` **在 `log::info!` 之前**（§6.2 第 3 步）。
4. spawn "mqtt" 线程，`stack_size(MQTT_TASK_STACK)`，失败只记日志。
5. **不要**把 `time_synced` 塞进 `Backend` 再读回来（§6.2 已说明理由）。

**验收**：串口能看到 `MQTT 已连接`；手工发一条 `cmd` 能得到 ack；`ack` 里不再是 `time-not-synced`。若失败按 §6.3 顺序排查。

### 阶段 3 —— 状态发布（独占文件：`src/mqtt.rs` 的 `run` 内，必要时 `main.rs`）

`STATUS_PUBLISH_S` 目前**没有任何消费端**（§5.4 已核）。阶段 2 只做了连接时的 `online:true` + 一次 `publish_state()`，对应 `main.c:458-460`。本阶段把 `status_task`（`main.c:537-546`）的定时语义搬进 mqtt 线程：

- 用 `Instant` 记下次发布时刻，在主循环里做 **非阻塞** 判断；**不要**为它再开一条线程，更不要在回调里做。
- 周期取 `cfg.status_period_s()`（运行时可变），每轮重新读，别缓存成常量。
- PC 状态取 `PcState::is_on()`（`src/pc_state.rs:32`，纯寄存器读、不会失败），发 retained `{"pc":"on"|"off"}` 到 `psw/<id>/state`（§2）。

### 阶段 4 —— `pc_state` 语义化（独占文件：`src/pc_state.rs`，可选 `src/consts.rs`）

**不依赖 MQTT，可与阶段 1–3 并行。** 当前 `is_on()` 是裸电平读，语义只有「此刻光耦是否导通」。要补的是：

- **幂等**：`pulse` 发出后到主板真正断电/上电之间有秒级延迟，同一时刻连续读会抖动。上报层应做去抖（连续 N 次一致才改状态），而不是把抖动直接推给上位机。
- **脉冲后确认**：`PulseHandle.busy` 为 true 期间（`src/pulse.rs:104-117`）不要上报「状态已变」，等 `busy` 落下再按去抖窗口确认。

**注意**：本阶段不要改 `PULSE_MS_MIN`/`PULSE_MS_MAX` 的 `const` 性质（安全线 3），也不要让 `pc_state` 反向影响脉冲决策 —— 它仍应保持「纯只读」。

### 阶段 5 —— Task WDT（独占文件：`src/main.rs`、`sdkconfig.defaults`）

当前 `rg wdt src/` 只命中两处注释（`src/pulse.rs:98`、`main.rs` 无实际调用），**MVP #11 未做**。C 版在 `main.c:597-603` 配 `{timeout_ms=30000, idle_core_mask=0, trigger_panic=true}` 并 `esp_task_wdt_add(NULL)` 监控 `app_main`；`main.c:615/629` 每轮 `reset`。

Rust 侧：

- 用 `esp_idf_svc::sys::esp_task_wdt_reconfigure` + `esp_task_wdt_add` + `esp_task_wdt_reset`，与 `main.c` 一一对应。
- **要监控的是会卡死的那几个线程**：`pulse` 线程（GPIO 卡高的直接受害者）、`mqtt` 线程、主循环。`esp_task_wdt_add` 传 `pthread` 句柄需按 IDF 6.1 的 API 确认（**未确认**：Rust 线程到 `TaskHandle_t` 的获取方式），若不支持则退化为只监控 `app_main`，与 C 版一致。
- **`sdkconfig.defaults` 改完后必须清 esp-idf-sys 产物**，否则新配置不生效：
  ```bash
  rm -rf "$CARGO_TARGET_DIR/riscv32imc-esp-espidf/release/build/esp-idf-sys"
  ```
  见 `CLAUDE.md` 第三节。`CONFIG_ESP_TASK_WDT=y` 若已由 IDF 默认开启，则只需运行时 `reconfigure`，不一定改文件 —— 改前先 `rg TASK_WDT sdkconfig`。

**验收**：人为在 `pulse` 线程里插 `sleep(35s)`，设备应 panic 重启而不是让 GPIO 卡高。

---

## 8. 风险与未确认

### 8.1 §3.4 的四项未确认（保留原文，逐项给规避）

| # | 未确认项 | 规避措施 |
|---|---|---|
| 1 | 回调线程实际栈大小与 `task_stack` 的对应关系 | 回调里**只做 `String`/`Vec` 拷贝 + `try_send`**（§4 Q2）。拷贝量上限由 `buffer_size` 约束（4096），命令报文 <200B，栈峰值可控。不设 `task_stack` 时用 IDF 默认值，**不主动调小** |
| 2 | `Received` 分片在 `buffer_size` 足够时是否仍出现 | `MQTT_BUFFER_SIZE = 4096`，并对 `Details::InitialChunk`/`SubsequentChunk` 显式 `log::warn!` + 丢弃，**不静默拼包**。若真出现分片，宁可丢一条命令也不做有状态重组 |
| 3 | `next()` 返回 `Err` 时是否还有别的错误码代表「无事件」 | 只把 `ESP_ERR_INVALID_STATE` 当「无事件」；其余 `EspError` 打 `log::error!` 并计数，连续 N 次则视为连接异常、交由 IDF 重连 |
| 4 | `reconnect_timeout` 的退避是否真有上限（是否读 `RECONNECT_MAX_MS`） | **不依赖 IDF 的退避**。`RECONNECT_MAX_MS` 在 C 版根本没被使用（见 8.3），Rust 版也不要假装它生效；若需要控额度，在 `Disconnected` 事件里自己计数并决定是否 `stop`/`restart` |

### 8.2 ack 的 `nonce` 回填属于**协议变更**

C 版 `publish_ack()`（`main.c:481-487`）三个调用点**全部传 `NULL`**，`publish_ack` 内部（`main.c:442-443`）把 `NULL` 渲染成 `""`：

```c
snprintf(buf, sizeof(buf), "{\"nonce\":\"%s\",\"result\":\"%s\"}",
         nonce ? nonce : "", result);
```

即：**现网 ack 里的 `nonce` 恒为空串**。Rust 版 `CmdVerifier` 已经解析出 nonce，回填真实值是自然做法，但这是**上位机可见的协议变化**：

- 若上位机按「`nonce == ""` 表示设备回的是非命令类 ack」做过判断，回填会让它行为改变。
- 若上位机靠 `nonce` 关联请求，那 C 版本来就关联不上，回填反而是修 bug。

**动作**：实现前先与上位机确认；未确认前，默认行为应与 C 版一致（发空串），把回填做成可开关，不要默认开启。

### 8.3 `RECONNECT_MAX_MS` 在 C 版并未真正使用

`main.c:504-532` 的 `mqtt_start()` 只把 `RECONNECT_BASE_MS` 交给了 IDF：

```c
.network.reconnect_timeout_ms = RECONNECT_BASE_MS,
.network.disable_auto_reconnect = false,
```

`RECONNECT_MAX_MS`（`src/consts.rs:54`，60_000）在 C 版**没有任何引用**。IDF 的重连退避策略在 `esp_mqtt_client` 内部实现，C 版没有干预。新实现同样只映射 `RECONNECT_BASE_MS` → `reconnect_timeout: Some(Duration::from_millis(1000))`；**不要在文档或代码注释里声称有 60s 上限**。若确实需要封顶（例如省 Serverless 额度），那是一个新功能，需要自己写重连状态机，不在本次移植范围内。

### 8.4 `NONCE_HISTORY` 已知差异

- Rust：`NONCE_HISTORY = 16`（`src/consts.rs:29`），`CmdVerifier` 用 `[u32; 16]` 环形数组（`src/cmd_verify.rs:99,154`）。
- C：32 槽。

影响：同一设备短时间内收到 >16 条带不同 nonce 的命令时，最早的那条**重放窗口变短**（这是更严格，不是更宽松，安全方向正确），但若上位机有合法的「延迟重发同一条命令」流程且超过 16 条间隔，会被判为重放或反之不判。**本次不改**，在 §2 的 payload 说明旁标注该差异即可；若要改回 32，只改 `consts.rs:29`，`cmd_verify.rs` 的数组长度自动跟随。

### 8.5 二进制体积

`Cargo.toml:29-36` 已是 `opt-level="z"` + `lto="fat"` + `codegen-units=1` + `strip`。注释里记录 Phase 0 实测：WiFi+SNTP+HTTPS+MQTT/TLS = **1.128 MB**，分区上限 **1.464 MB**，余量 **354 KB**。

MQTT 移植本身不增加新的 C 组件（`espressif/mqtt` 已在 `extra-components.toml`），但会带来：`serde_json` 的实际使用路径、`MqttEvent` 的 `String`/`Vec` 分配、以及 §7 阶段 3/4 的状态逻辑。**实现后必须核对**：

```bash
G:\Dev\ESP\idf.cmd idf.py size
```

若逼近 1.464 MB，优先削减的是 `buffer_size`/`out_buffer_size`（4096/2048 → 2048/1024）和日志字符串，**不是**关掉 `crt_bundle_attach`（TLS 校验是硬要求，见 `main.c:513-514` 的注释）。