# 开发路线图与进度台账

**本文件是全项目唯一的阶段编号来源与进度台账。** 其他文档只引用，不复述阶段划分。

- `docs/architecture.md §10` 原来的「阶段一 MVP / 阶段二加固 / 阶段三长期」已映射到本表的 Phase 编号（见下方映射表），原文已改为指向本文件。
- `docs/mqtt-port.md §7` 的 MQTT 实施阶段 1–5 已映射到本表 Phase 5 / Phase 1 / Phase 6。
- `docs/mqtt-port.md` 本身是移植施工图，**MQTT 实现完成后删除**。

## 写入纪律

1. 完成一项就改对应行的勾选状态，并在行尾追加 `（YYYY-MM-DD 完成，证据 文件:行号）`。
2. 新增任务行**追加到对应阶段的末尾**，不要插在中间。
3. 行内引用的 `文件:行号` 是迁移时的快照，代码改动后应同步更新。
4. 本表条目以 2026-10-28 从 `AGENTS.md` 迁移时的内容为准；个别阶段的历史汇总计数可能与逐条勾选有出入，**以条目为准**。

图例：`[x]` 已完成且有代码 · `[~]` 部分完成 · `[ ]` 未开始 · `[-]` 明确不做/延后

## 阶段映射

| 本文 Phase | 原 `architecture.md §10` | 原 `mqtt-port.md §7` |
|---|---|---|
| Phase 0 脚手架与工具链 | — | — |
| Phase 1 安全执行器与状态回读 | §10.1（MVP #8–10、#14）、§10.3 第 1–2 条 | 阶段 4（`pc_state` 语义化） |
| Phase 2 配置/控制台/网络基础 | §10.1（MVP #1、#7、#14） | — |
| Phase 3 DDNS | — | — |
| Phase 4 指令校验 | §10.1（MVP #5、#6） | — |
| Phase 5 MQTT 运行时 | §10.1（MVP #2、#3、#13） | 阶段 1、2、3 |
| Phase 6 可靠性加固 | §10.1（MVP #11、#12）、§10.3 | 阶段 5（Task WDT） |
| Phase 7 安全与运维 | §10.3、§10.4 | — |
| Phase 8 长期 | §10.4 | — |

---

## 当前焦点（按依赖顺序）

1. **Phase 5：`src/mqtt.rs` 阶段 1→3** —— MVP 唯一还缺的功能块。前置条件已全部就绪：托管组件进构建、NVS 凭据可读写且已有控制台入口（`mq` / `devid`）、`CmdVerifier` 自洽、`time_synced` 已接线。
2. **Phase 6：Task WDT** —— 纯安全项，与 MQTT 无依赖，可并行。

> 2026-10-28 已完成：`time_synced` 接线（Phase 2 末项）、`run-check.cmd` 修复与 `cargo-build.cmd check` 基线（Phase 0 遗留）、`mq` / `devid` 控制台命令（Phase 2 末项）。

---

## Phase 0 —— 脚手架与工具链

- [x] workspace + release size profile（Cargo.toml:1、Cargo.toml:29-43）
- [x] `build.rs` 导出 IDF sysenv（build.rs:1-11）
- [x] target / build-std / `CARGO_WORKSPACE_DIR`（.cargo/config.toml:3、:15、:37）
- [x] `sdkconfig.defaults`：4MB flash + 大分区表 + USB-Serial-JTAG 主控制台（sdkconfig.defaults:16-40）
- [x] TLS 交叉签名根校验 + 证书有效期校验（sdkconfig.defaults 末段 `CONFIG_MBEDTLS_CERTIFICATE_BUNDLE_CROSS_SIGNED_VERIFY` / `CONFIG_MBEDTLS_HAVE_TIME_DATE`）
- [x] `espressif/mqtt` 托管组件接线（extra-components.toml:7、.cargo/config.toml:39-47）
- [x] `cargo-build.cmd` 去硬编码、改从环境变量推导（2026-10-28 完成，证据：`cargo-build.cmd` 全文；`check` 实测 exit 0）。踩过的坑与对应解：
  - 宿主链接器缺失（无 MSVC）→ 装 rustup gnu host 工具链，脚本内设 `RUSTUP_TOOLCHAIN`，不改签入的 `rust-toolchain.toml`
  - rustup gnu 工具链只有 `gcc/ld/dlltool`，缺 `as/ar` → scoop 装 `binutils`
  - IDF 工具链被重复下载到 `.embuild` → `ESP_IDF_TOOLS_INSTALL_DIR=fromenv`
  - component registry 走代理时 TLS 失败 → 精准 `NO_PROXY` 两个完整主机名，**保留证书校验**
- [x] 工具链固定 + `rustfmt` / `clippy` 组件声明（2026-10-28 完成，证据：rust-toolchain.toml `components = ["rust-src", "rustfmt", "clippy"]`）
- [~] 固件体积预算已记录（Cargo.toml:22-25 注释：1.128MB / 上限 1.464MB），但无固件 hash 台账
- [x] `run-check.cmd` 修复路径与日志目录（2026-10-28 完成，证据：`run-check.cmd` 改用 `%~dp0` 与 `%CARGO_TARGET_DIR%`）
- [x] 删除 C 版遗留构建路径（2026-10-28 完成，证据：`main/` 14 个文件、根 `CMakeLists.txt`、`components_esp32c3.lock` 已从 git 删除）。纯 Rust std 模式的官方模板不含这两者，并存会导致 `idf.py build` 编到 C 版。`main/secrets.h` 已备份至仓库外
- [x] 删除 `rust-probe/` 体积探针（2026-10-28 完成，证据：目录与 `Cargo.toml` 的 workspace exclude 一并移除；结论已固化在 Cargo.toml:22-25 注释）
- [x] 补齐工程化配置文件（2026-10-28 完成，证据：新增 `README.md`、`.editorconfig`、`.gitattributes`、`rustfmt.toml`、`clippy.toml`）。`.gitattributes` 解决 `cargo-build.cmd` 在索引里存 LF 的问题
- [x] 建立 lint 门禁（2026-10-28 完成，证据：`cargo fmt --check` exit 0、`cargo clippy -- -Dwarnings` exit 0）。`main.rs:35` 加 `#![deny(unsafe_op_in_unsafe_fn)]`；36 处 MQTT 预留死代码逐点标注 `#[allow(dead_code)] // Phase 5 接线后删除`（不全局 allow，保留死代码信号）；未开 `unreachable_pub`（bin crate 无意义）
- [x] `.vscode/settings.json` 去掉失效绝对路径（2026-10-28 完成，证据：删掉指向不存在的 `G:\Dev\ESP\v5.5.5\esp-idf` 的 `idf.currentSetup`）
- [x] 平台中立门禁 `scripts/check-portability.py`（2026-10-28 完成，证据：P1 绝对路径 / P2 `.cmd` 纯 ASCII+CRLF / P3 `.gitattributes` / P4 toolchain 组件四项检查；**Windows (Python 3.11.17) 与 WSL Ubuntu 26.04 (Python 3.14.4) 双侧实测 0 ERROR / 0 WARN**；负向测试四项均能正确报错，非哑巴门禁）。这把 AGENTS.md 的 L0/L1 从口头纪律升级为可执行检查

## Phase 1 —— 安全执行器与状态回读

- [x] 上电立刻把 GPIO5 置低（pulse.rs:70-76）
- [x] 脉冲独占线程（pulse.rs:82-88）
- [x] 线程内二次 `clamp(PULSE_MS_MIN, PULSE_MS_MAX)`（pulse.rs:102）
- [x] 脉冲后强制最小间隔（pulse.rs:120）
- [x] 安全边界保持 `const`（consts.rs:14-21）
- [x] 启动顺序：pulse 先于 pc_state 与所有网络（main.rs:486-494）
- [x] GPIO6 状态回读，只读、上拉（pc_state.rs:32-34、:41-44）
- [~] `pc_state` 语义化（去抖 + 脉冲后确认）—— 目前只有裸电平读 `is_on()`，`busy` 期间的上报抑制未做（规格见 mqtt-port.md 阶段 4）
- [ ] 幂等「开机/关机」语义（基于状态回读）

## Phase 2 —— 配置 / 控制台 / 网络基础

- [x] NVS 键与 C 版逐字节兼容（cfg.rs:36-42）
- [x] Rust 新增键 `ddns_*` / `mq_*` / `dev_id`（cfg.rs:48-65）
- [x] `PulseMs` 类型级安全约束（cfg.rs:160-168、:347-368）
- [x] 每次读直查 NVS，无 RAM 缓存（cfg.rs:347-363 读路径）
- [x] 控制台 12 条命令全部注册（console.rs:654-665）
- [x] `ddns` 七个子命令（console.rs:421-423、:437-545）
- [x] `press` 走与 MQTT 相同的脉冲队列（console.rs:577-579）
- [x] Wi-Fi 事件回调 + 退避重连（wifi.rs:359-370）
- [x] SNTP 主/备切换（wifi.rs:784-808）
- [x] SNTP 独立线程（main.rs:537-544）
- [x] SNTP 成功 → 时间同步标志的接线（2026-10-28 完成，证据：`main.rs:138` 签名加 `Arc<AtomicBool>`、`main.rs:169` 在 `log::info!` 之前 `store(true)`、`main.rs:521` 创建、`main.rs:552` 传参；`check` exit 0 且警告数与基线一致）
- [x] MQTT 配置的控制台命令（`mq` / `devid`）（2026-10-28 完成，证据：`console.rs:596-704` `cmd_mq`、`:706-720` `cmd_devid`、`:865-872` CmdDef 登记；`main.rs:335-359` 只读转发、`:438-457` 写入转发；`trait ConsoleBackend` 无「读密码明文」方法，`mq pass` 显示与回显均走 `mask_token`；`check`/`fmt --check`/`clippy -Dwarnings` 三道门禁 exit 0）

## Phase 3 —— DDNS

- [x] provider / 传输分离（ddns.rs:371 trait DdnsProvider、:1012 `execute`）
- [x] token 不进日志、不进 URL（ddns.rs:307 `mask_token`、:340 `assert_token_absent`、:351 `redacted_request_line`）
- [x] IPv6 双闸门（编译期 + 运行期）（ddns.rs:62、:690；sdkconfig.defaults IPv6 SLAAC 段）
- [x] 5xx / 429 退避与最小间隔（ddns.rs:43-52、:283-296）
- [x] 主循环 tick 接入（main.rs:188 `ddns_tick`、:579-581）
- [x] `ddns now` 异步置标志、不在 REPL 线程发 HTTPS（main.rs:92、:340-344；console.rs:430）
- [x] 20 个宿主单测（ddns.rs:1084 `mod tests`）

## Phase 4 —— 指令校验（CmdVerifier）

- [x] topic 逐字节比对（cmd_verify.rs:144 `topic_matches`、:175-178）
- [x] 未对时一律拒绝（cmd_verify.rs:185-187）
- [x] 手工 JSON 解析、fail-closed（cmd_verify.rs:199-204）
- [x] 只允许 `press` / `press_long`，无 on/off（cmd_verify.rs:206-212）
- [x] ts / ttl 时间窗，ttl 非法回退配置值（cmd_verify.rs:214-226）
- [x] nonce 环形去重（cmd_verify.rs:99、:152-155、:228 起）
- [x] `PRESS_LONG_MS = PULSE_MS_MAX`（cmd_verify.rs:39）
- [x] `verify()` 接入真实消息源（2026-10-28 完成，证据 mqtt.rs:383 `handle_command` 调 `verifier.verify()`；`CmdVerifier` 由 mqtt 线程独占，mqtt.rs:305 构造）

## Phase 5 —— MQTT 运行时（mqtt-port.md 阶段 1–3）

> **验收口径**：以下 `[x]` 均为「代码审查 + `cargo-build.cmd check` 通过」，
> **尚未在真机上连 broker 验证**。端到端验收需 `mq enable` + `mq host` 配好凭据后
> 上电观察串口日志。

- [x] client_id / 端口 / keepalive 等常量就位（consts.rs:48-71）
- [x] MQTT 凭据 NVS 读写 API —— 五个 getter/setter（cfg.rs:565-744），由 `mq` / `devid` 控制台命令写入（console.rs:609-720），由 `mqtt::spawn` 运行时读取（mqtt.rs:164-179）（2026-10-28 完成）
- [x] `src/mqtt.rs` 骨架与独立 `mqtt` 线程（2026-10-28 完成，证据 mqtt.rs:158 `spawn` 建 client、:290-294 `Builder::new().name("mqtt")` 起线程、:304 `run` 主循环）
- [x] TLS(8883) 连接 broker，内嵌/校验 CA（2026-10-28 完成，证据 mqtt.rs:174 `mqtts://{host}:{MQTT_PORT}`、:224 `crt_bundle_attach`、:221 `skip_cert_common_name_check = false`）
- [x] LWT + retained `psw/<id>/status`（2026-10-28 完成，证据 mqtt.rs:198-203 `LwtConfiguration{topic: status, payload: {"online":false}, retain: true}`；上线发布 mqtt.rs:427 `publish_status_online`）
- [x] 订阅 `psw/<id>/cmd`，回调只做拷贝 + `try_send`（2026-10-28 完成，证据 mqtt.rs:324 `subscribe`、:239-276 回调只 `to_owned()`/`to_vec()` + `try_send`，分片报文直接丢弃）
- [x] `ack` 回执发布（2026-10-28 完成，证据 mqtt.rs:440 `publish_ack`，QoS 1 / retain false。**nonce 回填未做，与 C 版一致发空串**，属协议变更见 mqtt-port.md §8.2）
- [x] 状态发布 `psw/<id>/state`（2026-10-28 完成，证据 mqtt.rs:403 `publish_state`，retain true；定时刷新 mqtt.rs:366-370 按 NVS 里的 `STATUS_PUBLISH_S` 每轮重读）
- [x] 连接前置条件「SNTP 已同步」的运行时强制（2026-10-28 完成，证据 mqtt.rs:311-319 把 `Arc<AtomicBool>` 同步进 `CmdVerifier`，未同步时 `verify()` 返回 `TimeNotSynced`）
- [ ] MAC 派生 device_id（当前固定 `pc01`，见「已知偏差」第 1 条）

## Phase 6 —— 可靠性加固

- [ ] Task WDT（`esp_task_wdt_reconfigure/add/reset`）—— src/ 中 `wdt` 仅命中 pulse.rs:98 的注释；sdkconfig.defaults 无 `TASK_WDT` 配置。规格见 mqtt-port.md 阶段 5，验收：在 pulse 线程插 35s sleep 应 panic 重启
- [~] 指数退避 —— Wi-Fi 侧已完成（wifi.rs:359-370）；MQTT 侧常量存在但无消费者（consts.rs:52-54）
- [-] 掉线 fail-off —— 不适用：执行器是瞬时按钮，掉线保持现状即安全

## Phase 7 —— 安全与运维

- [ ] OTA 升级 + 签名 + anti-rollback（src/ 中 `ota` 无匹配）
- [ ] Secure Boot / Flash Encryption（sdkconfig.defaults 中无相关项）
- [ ] BLE 配网（带 PoP + 超时关闭），把 Wi-Fi 密码移出固件
- [ ] 生产日志降级
- [ ] 固件 hash 台账
- [ ] 访客网络 / 关闭 UPnP（路由器侧配置，非代码）
- [-] 容器化构建 —— 已明确放弃

## Phase 8 —— 长期

- [ ] 多设备统一管理
- [ ] 审计日志 + 异常告警（如「连续 3 次按电源键但状态未变化」）
- [-] 继电器 / 市电负载控制 —— 当前方案不涉及市电

---

## 已知偏差（文档与实际实现）

1. **device_id 策略已改**：`architecture.md` 曾建议 MAC 派生（`psw-3c84f7`），实际 `consts.rs` 决策改为固定 `pc01` + 后缀 `-esp32`，多设备需求出现再改。
2. **C 版的 MQTT 退避常量在 Rust 版尚无消费者**：`consts.rs:52-54` 定义 `RECONNECT_BASE_MS/MAX_MS`，`wifi.rs:77` 注释明确这是 MQTT 的退避，Wi-Fi 用自己的 `RECONNECT_IMMEDIATE` 逻辑。且 `RECONNECT_MAX_MS` 在 C 版也从未被使用。
3. **`cargo-build.cmd` 已于 2026-10-28 修复**：路径全部改为从 `IDF_PATH` / `IDF_TOOLS_PATH` / `IDF_PYTHON_ENV_PATH` 推导。
4. **`CmdVerifier` 尚未在运行时实例化**：`cmd_verify.rs` 全模块已实现且自洽，但 `main.rs` 只有 `mod cmd_verify;`，没有任何 `CmdVerifier::new`。