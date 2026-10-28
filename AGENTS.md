# AGENTS.md

本文件说明**如何在这个仓库工作**：构建入口、本机踩坑、安全线、代码纪律。所有 coding agent 共用这一份。

- **进度与阶段划分**：见 [`docs/roadmap.md`](docs/roadmap.md)（唯一来源，本文件不维护台账）
- **文档地图**：见 [`docs/README.md`](docs/README.md)

---

## 这是什么

ESP32-C3 的「电源键控制器」：通过 GPIO5 驱动光耦模拟按下机箱电源键，通过 GPIO6 读回 PC 开关机状态；配网后经 MQTT 收指令、经 DDNS 上报公网地址。C 版重写为 Rust（`esp-idf-svc` 0.53 + 标准 std + `build-std`）。

**这是安全相关设备**：GPIO5 卡高会触发主板强制断电。改代码前先读「安全线」。

---

## 构建 / 烧录 / 测试

### 唯一的构建入口是 `cargo-build.cmd`

```bat
cargo-build.cmd check              :: 类型门禁
cargo-build.cmd build --release    :: 完整 LTO 链接（约 50 分钟）
```

不要在 Git Bash 里直接跑 `cargo build` / `idf.py`。这个包装器做四件不可省的事：

1. `set "MSYSTEM="` —— Git Bash 注入的 `MSYSTEM=MINGW64` 会让 `idf_tools.py` 和 `embuild` 拒跑或算错配置。
2. 拼 `PATH`，把 `esp-clang` 和 `libclang` 目录排在前面。
3. `CARGO_TARGET_DIR` 指向短路径 —— `esp-idf-sys` 会拒绝过长的 `OUT_DIR`。
4. 把 libclang 依赖的 MinGW 运行库拷到 build-script 的 `out\` 目录旁，因为 `esp-idf-sys` 会重写 `PATH` 并让 GCC 的 `libstdc++-6.dll` 抢先命中，导致 bindgen 报 Win32 error 127。

**该文件必须纯 ASCII + CRLF**。cmd 按 OEM 码页解析，非 ASCII 注释会被当作命令执行。

首次失败会自动重试一次 —— 首轮 `out\` 还不存在，DLL 没地方拷。

### 路径：一律从环境变量读，禁止写死绝对路径

本机做过「去链接化改造」，写死路径已两次导致构建脚本静默失效。需要的三个变量都是 **User 级**：

```powershell
foreach($n in 'IDF_PATH','IDF_TOOLS_PATH','IDF_PYTHON_ENV_PATH'){ "$n = [$([Environment]::GetEnvironmentVariable($n,'User'))]" }
```

`ESP_CLANG` / `LIBCLANG_PATH` / `RISCV_BIN` / `CMAKE_BIN` **不单独设**，由 `cargo-build.cmd` 从 `IDF_TOOLS_PATH` 推导（`for /d` 取版本号最大的目录）。不要在脚本或文档里再写死它们。

`CARGO_TARGET_DIR` 同样从环境变量读。

**动手前先跑一次 `cargo-build.cmd check` 验证**，别假设路径是对的。

### 平台中立边界（新增脚本前必读）

**目标**：核心代码与构建逻辑保持 OS 中立。分三档，别混为一谈：

| 档                | 内容                                     | 状态                       |
| ----------------- | ---------------------------------------- | -------------------------- |
| **L0 路径中立**   | 禁止写死绝对路径，一律从环境变量推导     | ✅ 已达成，**有可执行门禁** |
| **L1 脚本薄封装** | 平台专用脚本只做环境准备，不承载构建逻辑 | ✅ 已达成，**有可执行门禁** |
| **L2 跨 OS 构建** | 任何 OS 都能 build                       | ❌ **不支持**，见下         |

**L0 / L1 由 `scripts/check-portability.py` 强制**（纯标准库 Python，Windows 与 WSL 双侧实测通过）：

```powershell
python scripts\check-portability.py           # 仓库根，退出码 0/1
python scripts\check-portability.py --verbose # 打印白名单命中
```

它检查四件事：P1 有效行里写死的盘符/UNC/`/mnt/x` 路径（注释与 Markdown 不算——那里记的是历史教训）、P2 `.cmd` 必须纯 ASCII + 全 CRLF、P3 `.gitattributes` 必须强制 `.cmd` 为 CRLF、P4 `rust-toolchain.toml` 必须声明 `rustfmt` + `clippy`。**改完构建脚本或新增平台文件后跑一遍。**

**当前支持的构建平台：仅 Windows。** 入口 `cargo-build.cmd`，日志包装 `run-check.cmd`。Linux / macOS **没有可用构建入口**——这是已知限制，不是待修的 bug。

新增或修改平台脚本时：

1. **绝对禁止写死路径**（唯一例外：可被环境变量覆盖的兜底默认值，且必须注释说明）。
2. 平台脚本必须是**薄适配器**：只做清 `MSYSTEM`、拼 `PATH`、设 `RUSTUP_TOOLCHAIN`、拷 DLL 这类环境准备。构建逻辑若需复用，抽成跨平台脚本（Python 优先）。
3. 补 `cargo-build.sh` 属 Phase 8 长期项。**没有 Linux 环境验证之前不得声称「跨平台」**——写一个测不了的脚本等于把不确定性留在仓库里。
4. 新增平台支持时，同步更新本节表格与 `docs/README.md`。

> 路径与编码类规则已从「口头纪律」升级为**可执行门禁**（见上）。仍未强制的是「脚本是否承载了不该承载的构建逻辑」——这需要人判断，脚本查不出来。CI 落地时直接复用 `check-portability.py` 即可，不必另写。

### 宿主侧工具链

这台机器没有 MSVC（无 Visual Studio、无 vswhere），宿主程序的链接器是另外补的：

- **Rust host**：rustup 装了 `nightly-2026-09-20-x86_64-pc-windows-gnu`，`cargo-build.cmd` 在进程内设 `RUSTUP_TOOLCHAIN` 覆盖 `rust-toolchain.toml`（签入的文件保持原样，有 MSVC 的机器仍可用）
- **binutils**：rustup 的 gnu 工具链是精简版，`self-contained` 里只有 `gcc/ld/dlltool`，缺 `as/ar` → scoop 装 `binutils`

`cargo check` 也会用到它们，因为 `esp-idf-sys` 的 build script 是**宿主程序**。

### 网络：两处必须的绕行

| 问题                                                                                    | 解法                                                                                         |
| --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| cargo 经代理下载 crate，schannel 吊销检查离线失败、多路复用被掐                         | `CARGO_HTTP_CHECK_REVOKE=false` / `MULTIPLEXING=false` / `TIMEOUT=180` / `LOW_SPEED_LIMIT=1` |
| component manager 经代理访问两个 espressif 域名 TLS 失败（直连正常、证书是真 DigiCert） | 精准 `NO_PROXY` 两个完整主机名，**保留证书校验**                                             |

`NO_PROXY` 必须写完整主机名：`espressif.com` / `.espressif.com` 都覆盖不到带连字符的 `components-file.espressif.com`（实测超时）。

`ESP_IDF_TOOLS_INSTALL_DIR=fromenv` 让 `esp-idf-sys` 复用已激活的 IDF 环境，不再往项目内 `.embuild` 重下整套工具链。

### 测试

只有 `src/ddns.rs` 的 `#[cfg(test)] mod tests`，20 个纯函数测试（构造请求 / 解析响应 / 掩码），可在宿主跑。**crate 默认 target 是 riscv32，`cargo test` 没有可用 runner**——要在宿主跑需显式指定 host target。其余模块无测试，靠硬件手测。

`run-check.cmd` 是 `check` 的带日志包装器，日志落在 `%CARGO_TARGET_DIR%`。

### 烧录

`.cargo/config.toml` 的 `runner = espflash ...` 是注释状态，本机未装 espflash。用 IDF 自带 esptool。板子**没有自动复位电路**，烧录失败要手动拔 USB。

---

## 架构速览

详细设计见 [`docs/architecture.md`](docs/architecture.md)，硬件见 [`docs/hardware.md`](docs/hardware.md)。

### 线程模型

| 线程    | 职责                                                       |
| ------- | ---------------------------------------------------------- |
| `main`  | 5 秒心跳、DDNS tick、发起所有 HTTPS 请求                   |
| `pulse` | 独占线程，唯一允许拉 GPIO5 的地方                          |
| `sntp`  | 一次性对时（`sync()` 最长阻塞 60 秒，不能放进 5 秒主循环） |

共享状态是 `Box::leak` 成 `&'static` 的 `Backend`：`Arc<Cfg>`、`PulseHandle`（可克隆的 `SyncSender`）、`Mutex<WifiManager>`、`NetUpWatch`（免锁 `AtomicBool`）、`Mutex<TimeSync>`。

**两把锁刻意分开**：对时期间只锁 `time_sync`，Wi-Fi 锁保持可抢，避免 60 秒对时把串口卡死。

### 启动顺序（顺序本身是设计）

`link_patches` → logger → `Peripherals::take` → **`pulse::init(gpio5)`** → `pc_state::init(gpio6)` → 事件循环 / NVS → `Cfg::new` → `WifiManager::new`（只建驱动，不连）→ 取 `net_up_watch` → `console::init` → 若有 SSID 则重连 → 起 SNTP 线程 → 主循环。

`pulse::init` 必须在所有网络初始化**之前**：它内部立刻 `set_low()`，安全态优先。

### 模块职责

- `consts.rs` —— 所有编译期常量的单一来源：引脚、脉冲边界、NVS 键默认值、SNTP 服务器、MQTT 端口。改参数先看这里。
- `pulse.rs` —— 安全线实现。`init()` 立刻 `PinDriver::output` + `set_low()`，再起独占线程经 `mpsc::sync_channel(4)` 收脉冲宽度；线程内二次 `clamp(PULSE_MS_MIN, PULSE_MS_MAX)`，脉冲后强制 `PULSE_GAP_MIN_MS` 间隔。
- `pc_state.rs` —— 只读，GPIO6 输入 + 上拉，`is_low()` 即开机。
- `cfg.rs` —— NVS 配置。`Cfg{ nvs: Mutex<EspNvs> }`，**每次读都直查 NVS，不做 RAM 缓存**。`PulseMs` 用类型级约束保证宽度进不了越界值。
- `console.rs` —— 通过 `trait ConsoleBackend` 与宿主解耦，自身不 import 任何 GPIO 类型。因为 `esp-idf-svc` 0.53 没包装 `esp_console`，直接走 `sys` raw bindings。
- `wifi.rs` —— Wi-Fi 驱动 + 事件回调 + SNTP。`WifiError` 全 fail-closed。
- `ddns.rs` —— provider / 传输分离，token 不进日志也不进 URL；IPv6 双闸门（编译期常量 + 运行期开关）。
- `cmd_verify.rs` —— JSON + ts/ttl 时间窗 + nonce 环去重 + topic 逐字节比对。`PRESS_LONG_MS = PULSE_MS_MAX`，不照抄 C 版的 5000。**当前无运行时调用者**，见 roadmap Phase 4/5。
- `mqtt.rs` —— **尚不存在**，是 MVP 唯一还缺的功能块。施工图见 [`docs/mqtt-port.md`](docs/mqtt-port.md)。

### NVS 持久化

namespace `psw_cfg`。键 `ssid/pass/tx_dbm/pulse_ms/cmd_ttl/status_s/keepalive` 与 C 版**逐字节兼容，禁止重命名**；`ddns_*`、`mq_*`、`dev_id` 是 Rust 版新增（老设备读到 `None` 走默认）。NVS 键名上限 15 字符。

**永不写入 NVS**：`PULSE_MS_MIN/MAX`、`PULSE_GAP_MIN_MS`、`MQTT_PORT`(8883)、MQTT scheme。这些是安全/降级防线，必须保持 `const`。

### 控制台命令

`cfg` `wifi` `txpower` `pulse` `ttl` `status` `keepalive` `ddns` `scan` `press` `reset` `reboot`

`ddns` 子命令：`show|host|token|enable|disable|interval|now`。各命令自带保存，没有单独的 `save`。

### 网络

`EspWifi`(BlockingWifi) + 事件订阅 + 退避重连（快速 5 次 500 ms，之后 5 秒）。SNTP 用 `ntp.aliyun.com` 主 / `cn.pool.ntp.org` 备——lwIP 只有 1 个槽位，是主备而非同时。

**SNTP 必须成功**，否则所有带 `ts` 的指令被拒。

---

## 安全线（改代码时一条都不能破）

1. **上电第一件事**是把 GPIO5 拉低——顺序本身是设计的一部分，别调换。
2. **脉冲只在独立线程里生成**，绝不在 MQTT / 控制台回调里直接拉 GPIO。
3. `PULSE_MS_MIN` / `PULSE_MS_MAX` 是安全边界，**必须保持 `const`，永不写入 NVS**。脉冲 ≥4000ms 会触发主板强制断电。
4. 板子**没有自动复位电路**，烧录失败要手动拔 USB。
5. 禁止滥用`unsafe`,如为了躲避rust检查器,除非有正当需求!


---

## 构建纪律

- 用 `cargo-build.cmd`；**不要在 Git Bash 里直接跑 `idf.py` 或 `cargo build`**。
- 改过 `sdkconfig.defaults` 之后必须清掉 `esp-idf-sys` 的 build 产物再编，否则新配置不生效：
  ```bash
  rm -rf "$CARGO_TARGET_DIR/riscv32imc-esp-espidf/release/build/esp-idf-sys"
  ```
- 同理，只改 `extra-components.toml` 的内容不会触发 build script 重跑，只有改 `.cargo/config.toml` 里 `ESP_IDF_SYS_EXTRA_COMPONENTS_FILE` 的值才会（`track_env_var`）。
- 大改动用 `cargo-build.cmd check` 先过类型门禁，别直接上 50 分钟的 LTO 链接。
- `.cargo/config.toml` 里的 `CARGO_WORKSPACE_DIR = { value = "", relative = true }` 不是可选项——去掉它 `embuild` 会算错 workspace 根，静默无视仓库里的 `sdkconfig` / `sdkconfig.defaults`，导致分区表回落成 1MB、主控制台降级为 UART0（串口敲不进命令）。
- 本机网络传大文件会断流，走 `dl.espressif.cn` 镜像，下完**必须校验 SHA256**。
- 故意不开 `panic_immediate_abort`：能再省 10-20%，但会抹掉 panic 信息。这是安全相关设备，串口上那行 panic 比几百 KB flash 值钱。

---

## 文档修改纪律

- **进度只写在 `docs/roadmap.md`**。完成一项就改勾选状态，行尾追加 `（YYYY-MM-DD 完成，证据 文件:行号）`。不要在本文件或 CLAUDE.md 里维护台账。
- **阶段编号只认 `docs/roadmap.md`**。其他文档提到阶段时只引用，不复述。
- 新增文档前先看 `docs/README.md` 的职责表，确认没有更合适的归属。

---

## 磁盘满 / 路径不可写 —— 先想 junction，不要先分析

`G:` 长期只剩个位数 GB，而 IDF 工具链、`dist` 缓存、`.embuild` 都会持续增长。

```bash
cp -r "$SRC/." "$DST/"          # 1. 复制（L: 有 81 GB 空闲，优先选它）
du -s "$SRC" "$DST"             # 2. 校验大小
rm -rf "$SRC"                   # 3. 删原目录
MSYS_NO_PATHCONV=1 cmd /c mklink /J 'G:\path\to\dir' 'Q:\path\to\dir'
```

**两个已踩过的坑：**

- `cmd //c` 在 Git Bash 里会被 MSYS 转成 `/c`；加 `MSYS2_ARG_CONV_EXCL='*'` 又会把 `//c` 一起保护住，cmd 收到字面量 `//c`，**开一个交互式 shell 然后挂住**。正确写法是 `MSYS_NO_PATHCONV=1 cmd /c`。
- `MSYS_NO_PATHCONV=1` 只作用在那一条命令上，不要 export 到全局。

判断 junction 是否真的生效：搬完之后跑一次会写那个目录的操作，看目标盘在涨、源盘不跌。只看 `ls -la` 出现 `->` 不够。

---

## idf_tools.py 在 Git Bash 里跑不了

`idf_tools.py` 的 `__main__` 第一件事就是 `if 'MSYSTEM' in os.environ: fatal('MSys/Mingw is not supported.')`。Git Bash 每个新 shell 都重新注入 `MSYSTEM=MINGW64`，**`unset` 和 `env -u` 都删不掉**。

唯一可靠办法是在 Python 进程内部删掉再 `runpy`：

```python
import os, sys, runpy
for k in ('MSYSTEM', 'MSYS2_ENV_CONV_EXCL', 'MINGW_PREFIX'):
    os.environ.pop(k, None)

IDF = os.environ['IDF_PATH']
TOOLS_DIR = os.path.join(IDF, 'tools')
sys.path.insert(0, TOOLS_DIR)   # 必须，否则 import python_version_checker 失败

sys.argv = ['idf_tools.py', '--idf-path', IDF, '--non-interactive', 'install', 'esp-clang']
runpy.run_path(os.path.join(TOOLS_DIR, 'idf_tools.py'), run_name='__main__')
```

或者直接用 `cargo-build.cmd`（它已经 `set "MSYSTEM="`）。
