# ESP32-C3 远程电源键控制器

通过光耦模拟按下机箱电源键，让你能在任何地方把家里的 PC 开机/关机。

```
              ┌──────────┐   MQTT/TLS    ┌─────────────┐
  手机/PC ────▶│  EMQX    │◀─────────────▶│  ESP32-C3   │
              │ (broker) │               │             │
              └──────────┘               └──────┬──────┘
                                                │ 817D 光耦（电气隔离）
                                                ▼
                                          ┌──────────┐
                                          │  PC 主板  │
                                          │ PWR_SW   │
                                          └──────────┘
```

## 功能

- **远程按电源键** —— MQTT 下发 `press` / `press_long`，经 817D 光耦隔离触发主板 `PWR_SW`
- **开关机状态回读** —— 第二个光耦读主板电源 LED 电平，状态随 `state` topic 上报
- **在线状态** —— `status` topic + 遗嘱（LWT），掉线 broker 自动发 `{"online":false}`
- **DDNS** —— 公网 IP 变化时自动更新 DNSHE 记录
- **串口控制台** —— 不重新烧录就能改 Wi-Fi、脉冲宽度、MQTT 凭据
- **安全设计** —— 脉冲只在独立线程生成；宽度上下限编译期常量，永不进 NVS

## 硬件

- ESP32-C3（4MB flash，原生 USB-Serial-JTAG 作主控制台）
- 817D 光耦 ×2（一个驱动 `PWR_SW`，一个读电源 LED）

接线、限流电阻计算、脉冲参数：见 [`docs/hardware.md`](docs/hardware.md)。

> ⚠️ **ESP32 与 PC 之间唯一的跨边界连接是光耦内部的光，两边 GND 不得有任何电气连接。**

## 快速开始

```
1. 接线      见 docs/hardware.md §5.10，先接光耦再上电
2. 构建烧录  见下文
3. 配 Wi-Fi  串口敲：wifi <ssid> <pass>
4. 配 MQTT   mq host / mq user / mq pass / mq enable
5. 发指令    向 psw/pc01/cmd 发 {"cmd":"press","nonce":"...","ts":...}
```

完整使用说明、topic 结构、命令字段、排障：**[`docs/usage.md`](docs/usage.md)**。

上电后串口（115200）每 5 秒打印一行心跳，看到它就说明板子活着：

```
心跳：PC = 开机，pulse busy = false，Wi-Fi = 已连接
```

## 构建

**Windows 是当前唯一验证过的构建环境。** 入口是 `cargo-build.cmd`：

```bat
cargo-build.cmd check              :: 类型门禁（约 5 秒，增量）
cargo-build.cmd build --release    :: 完整 LTO 链接（首次约 1 小时，增量 7 分钟）
```

不要在 Git Bash 里直接跑 `cargo build` / `idf.py` —— 这个包装器处理了 `MSYSTEM` 污染、PATH 顺序、`CARGO_TARGET_DIR` 长度、libclang DLL 四件不可省的事。细节见 [`AGENTS.md`](AGENTS.md)。

首次构建需要：

1. 安装 ESP-IDF v6.1，并设置 User 级环境变量 `IDF_PATH` / `IDF_TOOLS_PATH` / `IDF_PYTHON_ENV_PATH`
2. `rustup toolchain install nightly-2026-09-20`（`rust-toolchain.toml` 已钉死）
3. 宿主要能链接 C 代码：有 MSVC 的机器直接可用；无 MSVC 的 Windows 需 rustup gnu 工具链 + binutils（见 `AGENTS.md`）

构建前后可跑跨平台门禁（路径中立、`.cmd` 编码、toolchain 组件）：

```powershell
python scripts\check-portability.py
```

## 烧录

用 IDF 自带的 esptool（本机未装 espflash）。产物在 `%CARGO_TARGET_DIR%\riscv32imc-esp-espidf\release\`。

```powershell
$py = "$env:IDF_PYTHON_ENV_PATH\Scripts\python.exe"
$t  = "$env:CARGO_TARGET_DIR\riscv32imc-esp-espidf\release"

# 1. ELF -> app 镜像
& $py -m esptool --chip esp32c3 elf2image --flash-mode dio --flash-freq 80m --flash-size 4MB `
    -o "G:\tmp\p\app.bin" "$t\powerswitch"

# 2. 写入三个镜像（偏移来自 CONFIG_BOOTLOADER_OFFSET_IN_FLASH / CONFIG_PARTITION_TABLE_OFFSET）
& $py -m esptool --chip esp32c3 -p COM3 -b 460800 write_flash `
    --flash-mode dio --flash-freq 80m --flash-size 4MB `
    0x0     "$t\build\bootloader.bin" `
    0x8000  "$t\build\partition-table.bin" `
    0x10000 "G:\tmp\p\app.bin"
```

**端口按实际改**（设备管理器里看 `USB 串行设备 (COMx)`，本机是 COM3）。

ESP32-C3 原生 USB-JTAG 通常能自动进下载模式，不用手按 BOOT。板子**没有自动复位电路**，烧录失败要拔插 USB 重试。

镜像大小上限 1500 KB（`partitions_singleapp_large.csv` 的 factory 分区）。当前约 1295 KB，余量 205 KB。超限时 `write_flash` 会写入失败。

## 项目状态

MVP 功能已完整实现并通过编译门禁，**MQTT 通路尚未在真机上连 broker 验证**。

已验证：烧录启动、串口控制台、Wi-Fi 连接、SNTP 对时、PC 状态回读。
未验证：MQTT TLS 握手、订阅、收指令按电源键。

进度与阶段划分见 [`docs/roadmap.md`](docs/roadmap.md)（唯一来源）。

## 文档

见 [`docs/README.md`](docs/README.md) 的文档地图。常用的：

| 文件 | 内容 |
|---|---|
| [`docs/usage.md`](docs/usage.md) | **使用手册**：控制台命令、MQTT topic 与 payload、排障 |
| [`docs/hardware.md`](docs/hardware.md) | 817D 电路、限流电阻、接线步骤、供电接地 |
| [`docs/architecture.md`](docs/architecture.md) | TLS 方案、MQTT 主题设计、安全模型 |
| [`docs/roadmap.md`](docs/roadmap.md) | 阶段划分与进度台账 |
| [`AGENTS.md`](AGENTS.md) | 构建踩坑、安全线、代码纪律（给 coding agent 看） |

## 构建平台

**仅支持 Windows**。唯一构建入口是 `cargo-build.cmd`（依赖 ESP-IDF 的 Windows 工具链、libclang DLL 与 gnu 宿主链接器）。

Linux / macOS 当前无构建入口，这是已知限制而非缺陷——补 `cargo-build.sh` 需在有对应环境时验证后才算完成。详见 [`AGENTS.md`](AGENTS.md) 的「平台中立边界」。