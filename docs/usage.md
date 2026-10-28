# 使用手册

面向**使用者**：怎么配、怎么用、怎么排障。设计与论证见 [`architecture.md`](architecture.md)，硬件见 [`hardware.md`](hardware.md)。

---

## 0. 首次上手（5 步）

```text
1. 接线      见 hardware.md §5.10，先接光耦再上电
2. 烧录      README.md「构建与烧录」
3. 配 Wi-Fi  串口敲：wifi <ssid> <pass>
4. 配 MQTT   mq host / mq user / mq pass / mq enable
5. 发指令    向 psw/pc01/cmd 发 {"cmd":"press","nonce":"...","ts":...}
```

上电后串口（115200）会打印心跳，每 5 秒一行：

```
心跳：PC = 开机，pulse busy = false，Wi-Fi = 已连接
```

看到这行就说明板子活着。敲 `help` 列出全部命令。

---

## 1. 串口控制台

USB-Serial-JTAG 是**主控制台**（`sdkconfig.defaults` 里 `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y`），插上就是 COM 口，115200。

### 命令一览

| 命令 | 说明 |
|---|---|
| `cfg` | 打印当前全部配置 |
| `wifi <ssid> [pass]` | 改 Wi-Fi 并存 NVS，立即重连 |
| `scan` | 立刻扫描一次 Wi-Fi |
| `txpower <dBm>` | 改发射功率 |
| `pulse <ms>` | 改默认脉冲宽度（`PULSE_MS_MIN..PULSE_MS_MAX`，即 100..3000） |
| `ttl <秒>` | 改指令时间窗（`ts` 与当前时间的最大允许偏差） |
| `status <秒>` | 改状态上报周期 |
| `keepalive <秒>` | 改 MQTT keepalive（重连后生效） |
| `ddns show\|host\|token\|enable\|disable\|interval\|now` | DDNS 子命令 |
| `mq show\|host\|user\|pass\|enable\|disable\|refresh` | MQTT 子命令 |
| `devid [新ID]` | 设备 ID，决定 topic 前缀 |
| `press [ms]` | 手动触发一次电源键脉冲（本地测试用） |
| `reset` | 清空 NVS 恢复默认配置 |
| `reboot` | 软重启 |

**各命令自带保存**，没有单独的 `save`。

### 配 MQTT 凭据

```text
mq host xxxx.emqx.cloud      # 只写主机名，不要带 mqtts:// 和端口
mq user YOUR_USERNAME
mq pass YOUR_PASSWORD
mq enable
```

`mq show` 回显（密码永远不显示）：

```
mq.enabled = 是
mq.host    = xxxx.emqx.cloud
mq.port    = 8883 (固定 mqtts，不可配)
mq.user    = YOUR_USERNAME
mq.pass    = your_pas***      # 只显前 8 字符，永不回显明文
dev.id     = pc01
mq.link    = 已连接
```

`mq.link` 三种状态，对应三种排障方向：

| 值 | 含义 | 该查什么 |
|---|---|---|
| `已连接` | 连着 broker | —— |
| `已启动，断开中（自动重连）` | 客户端起来了，但没连上 | 主机名、凭据、防火墙、broker 是否在线 |
| `客户端未启动（未启用或初始化失败）` | `mq.enabled = 否` 或建 client 就失败了 | 先 `mq show` 看 enabled 和 host |

`mq refresh` 让设备立刻重发一次 retained 的 `state`，不用等下一个上报周期。

### 端口与 scheme 不可配

端口硬编码 `8883`、scheme 硬编码 `mqtts://`，**不进 NVS**（`src/consts.rs:71`）。这是防止有人把端口改成 1883 静默降级成明文。要改只能改代码重编。

---

## 2. MQTT 接口

### Topic 结构

`<id>` = `devid`，默认 `pc01`。

| Topic | 方向 | QoS | retain | payload |
|---|---|---|---|---|
| `psw/<id>/cmd` | 下行 | 1 | 0 | 见下 |
| `psw/<id>/state` | 上行 | 1 | **1** | `{"pc":"on"}` / `{"pc":"off"}` |
| `psw/<id>/status` | 上行 | 1 | **1** | `{"online":true}`；遗嘱 `{"online":false}` |
| `psw/<id>/ack` | 上行 | 1 | 0 | `{"nonce":"","result":"ok"}` |

`state` 与 `status` 都是 retained —— 上位机一订阅就拿到当前值，不必等下一次发布。

### 下发命令

```json
{
  "cmd": "press",
  "nonce": "5f3a9c21",
  "ts": 1793186081,
  "ttl": 10
}
```

| 字段 | 必需 | 说明 |
|---|---|---|
| `cmd` | ✅ | 只能是 `press` 或 `press_long`。**没有 on/off** —— 电源键是"按一下翻转" |
| `nonce` | ✅ | 任意字符串，用于去重。**必须每次不同** |
| `ts` | ✅ | Unix 秒。与设备当前时间的偏差不能超过 `ttl` |
| `ttl` | ❌ | 时间窗秒数。缺省或非法（`<=0` / `>300`）时用 NVS 里的配置值 |

`press` 用 NVS 里的 `pulse_ms`（默认 `PULSE_MS_DEFAULT`），`press_long` 固定 `PULSE_MS_MAX`（3000ms，接近主板强制断电阈值，慎用）。

### ack 结果码

| result | 含义 | 该怎么办 |
|---|---|---|
| `ok` | 脉冲已投递到脉冲线程 | —— |
| `busy` | 上一次脉冲还没做完 | 稍后重试 |
| `bad-json` | 载荷不是合法 JSON | 检查引号/转义 |
| `bad-schema` | 缺字段或类型不对 | `cmd`/`nonce` 必须是字符串，`ts` 必须是数字 |
| `unknown-cmd` | `cmd` 不是 press / press_long | 检查拼写 |
| `expired` | `|now - ts| > ttl` | 校对上位机时钟，或调大 `ttl` |
| `replay` | 这个 nonce 已经用过 | **换个新 nonce**，不要重发同一条 |
| `time-not-synced` | 设备还没对时成功 | 检查 Wi-Fi 和 NTP 连通性，见 §4 |

> **nonce 去重是必需的**：QoS 1 是"至少一次"，broker 抖动会把同一条消息投两遍。没有去重，你的网络抖一下电脑就被多按一下。
>
> 注意 ack 里的 `nonce` 目前**恒为空串**（与 C 版行为一致）。上位机不能靠 ack 的 nonce 匹配请求，要用自己的重试逻辑。

### 遗嘱（LWT）

设备掉线（断电、断网、崩溃）时 broker 会自动往 `psw/<id>/status` 发 `{"online":false}`。上位机订阅这个 topic 就能知道设备死活。

---

## 3. 典型操作

### 远程开机/关机

```bash
# 开机（PC 当前是关的）
mosquitto_pub -h xxxx.emqx.cloud -p 8883 --cafile ca.pem \
  -u user -P pass -t psw/pc01/cmd -q 1 \
  -m '{"cmd":"press","nonce":"'"$(date +%s%N)"'","ts":'"$(date +%s)"'}'

# 等 20 秒，查状态
mosquitto_sub -h xxxx.emqx.cloud -p 8883 --cafile ca.pem \
  -u user -P pass -t psw/pc01/state -C 1
# → {"pc":"on"}
```

**`nonce` 用时间戳或 UUID，每次必须不同**，否则第二条会被判 `replay`。

### 强制关机

长按 4 秒以上会触发主板强制断电。**先确认 PC 已经卡死再用**：

```json
{"cmd":"press_long","nonce":"...","ts":...,"ttl":10}
```

### 监控设备在线状态

```bash
mosquitto_sub -h xxxx.emqx.cloud -p 8883 --cafile ca.pem \
  -u user -P pass -t 'psw/pc01/status' -v
# psw/pc01/status {"online":true}
# （拔电后）
# psw/pc01/status {"online":false}
```

---

## 4. 排障

### 所有命令都回 `time-not-synced`

**最常见的问题。** `ts` 校验需要设备有正确的系统时间，而时间只能靠 SNTP 拿。

看串口日志有没有这一行：

```
SNTP 同步成功，时间 1793186081
```

没有的话依次查：

1. Wi-Fi 连上了吗（心跳行里 `Wi-Fi = 已连接`）
2. DNS 能解析 `ntp.aliyun.com` 吗
3. 路由器有没有拦 UDP 123

> 设备每秒重试一次 SNTP，最长阻塞 60 秒。这段时间主循环照常跑，控制台可用。

### `mq.link = 已启动，断开中（自动重连）`

客户端起来了但连不上 broker。按顺序查：

1. `mq show` 确认 host 没有带 `mqtts://` 前缀（设 host 时会拒绝，但值得确认）
2. 凭据对不对（用 `mosquitto_pub` 在电脑上试同一个账号）
3. broker 的 8883 端口通不通
4. 串口日志里有没有 TLS 握手失败（`mbedtls` 报错）

### TLS 握手失败 / `-0x3000`

证书链问题。`sdkconfig.defaults` 里已经开了 `CONFIG_MBEDTLS_CERTIFICATE_BUNDLE_CROSS_SIGNED_VERIFY` 处理交叉签名，如果还失败：

- 确认 broker 用的是公共 CA 签发的证书（自签证书不在 bundle 里，连不上）
- 确认设备时间正确（证书有效期校验需要，见上一条）

### 命令发出去了，没反应，也没 ack

1. 订阅 `psw/pc01/ack` 看有没有回执
2. 检查 `devid` —— topic 前缀错了设备就收不到
3. 检查 broker 的 ACL 是否允许这个用户订阅 `psw/pc01/cmd`

### 按了但 PC 没反应

串口敲 `press` 本地试一次：

- 有"嗒"的一声（继电器/光耦动作）但不亮 → 接线问题，见 `hardware.md`
- 完全没动静 → 串口日志看 `pulse busy`，或查 GPIO5 是否被拉低过

### 板子彻底没输出

- 确认 COM 口对不对（设备管理器看 `USB 串行设备`）
- 波特率 115200
- 烧录失败过的话板子可能卡在下载模式，拔插 USB

---

## 5. 配置项与默认值

| 项 | NVS 键 | 默认值 | 范围 |
|---|---|---|---|
| Wi-Fi SSID | `ssid` | 空 | —— |
| Wi-Fi 密码 | `pass` | 空 | —— |
| 发射功率 | `tx_dbm` | —— | —— |
| 脉冲宽度 | `pulse_ms` | 300 | 100..3000（上下限是**编译期常量，永不进 NVS**） |
| 指令时间窗 | `cmd_ttl` | 30 | 5..300 秒 |
| 状态上报周期 | `status_s` | 30 | 5..3600 秒 |
| MQTT keepalive | `keepalive` | 60 | 秒 |
| MQTT 主机 | `mq_host` | 空 | 不带 scheme |
| MQTT 用户 | `mq_user` | 空 | —— |
| MQTT 密码 | `mq_pass` | 空 | 只进 NVS，日志走 `mask_token` |
| MQTT 开关 | `mq_en` | 关 | —— |
| 设备 ID | `dev_id` | `pc01` | ASCII 字母数字 `_` `-` |

**永不写入 NVS**：`PULSE_MS_MIN` / `PULSE_MS_MAX` / `PULSE_GAP_MIN_MS` / `MQTT_PORT`。这些是安全与降级防线，必须是编译期常量。

`reset` 命令清空整个 namespace，所有项回默认值。

---

## 6. 已知限制

| 限制 | 说明 |
|---|---|
| 单设备 | `devid` 默认 `pc01`。多台设备必须手动改成不同 ID，否则 client_id 撞车会互踢 |
| 无 OTA | 升级要插 USB 重烧 |
| 无 BLE 配网 | Wi-Fi 密码目前只能串口配 |
| ack nonce 恒为空 | 协议设计问题，见 §2 |
| 仅 Windows 构建 | 见 `AGENTS.md` 的「平台中立边界」 |