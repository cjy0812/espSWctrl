# espSWctrl 项目规则

## 一、磁盘满 / 路径不可写 —— 先想 junction，不要先分析

`G:` 这块盘长期只剩个位数 GB，而 ESP-IDF 工具链、`dist` 下载缓存、`.embuild` 都是会持续增长的大目录。**遇到 `No space left on device` 或路径写不进去，第一反应是搬走 + 目录链接，不是先去做空间归因。**

标准做法（2026-10-17 实测有效）：

```bash
# 1. 复制到别的盘（本机 Q: 有 100G+ 空闲）
cp -r "$SRC/." "$DST/"

# 2. 校验大小（目标不应明显小于源）
du -s "$SRC" "$DST"

# 3. 删原目录
rm -rf "$SRC"

# 4. 建 junction —— 必须用 MSYS_NO_PATHCONV=1
MSYS_NO_PATHCONV=1 cmd /c mklink /J 'G:\path\to\dir' 'Q:\path\to\dir'
```

**两个已踩过的坑：**

- `cmd //c` 在 Git Bash 里会被 MSYS 转成 `/c`；而如果加 `MSYS2_ARG_CONV_EXCL='*'`，会把 `//c` 一起保护住，cmd 收到字面量 `//c`，**开一个交互式 shell 然后挂住**。正确写法是 `MSYS_NO_PATHCONV=1 cmd /c`。
- `MSYS_NO_PATHCONV=1` 只作用在 `cmd` 这一条命令上，不要 export 到全局，否则后续其它命令的路径参数会变形。

已建立的 junction（本机现状）：

| 链接点 | 目标 |
|---|---|
| `G:\GitHub\espSWctrl\.embuild` | `Q:\embuild-swctrl` |
| `G:\Dev\ESP\Espressif\dist` | `Q:\espressif-dist` |
| `G:\Dev\ESP\Espressif\tools\esp-clang` | `Q:\esp-clang` |

判断 junction 是否真的生效：搬完之后跑一次会写那个目录的操作，看 `df -h` 是**目标盘在涨、源盘不跌**。只看 `ls -la` 出现 `->` 不够 —— 2026-10-17 那次就是链接没建成，`dist` 已被删掉而链接点不存在。

## 二、idf_tools.py 在 Git Bash 里跑不了

`idf_tools.py` 的 `__main__` 第一件事是：

```python
if 'MSYSTEM' in os.environ:
    fatal('MSys/Mingw is not supported.')
```

Git Bash 每个新 shell 都会把 `MSYSTEM=MINGW64` 重新注入，**`unset` 和 `env -u` 都删不掉**（MSYS 的 python 启动器会重加；实测 `env -u` 无效，`MSYSTEM=` 空串也仍让 `'MSYSTEM' in os.environ` 为真）。

唯一可靠的办法是在 **Python 进程内部**删掉再 `runpy`：

```python
import os, sys, runpy
for k in ('MSYSTEM', 'MSYS2_ENV_CONV_EXCL', 'MINGW_PREFIX'):
    os.environ.pop(k, None)

IDF = r'G:\Dev\ESP\v6.1\esp-idf'
TOOLS_DIR = os.path.join(IDF, 'tools')
sys.path.insert(0, TOOLS_DIR)   # 必须，否则 import python_version_checker 失败

sys.argv = ['idf_tools.py', '--idf-path', IDF, '--non-interactive', 'install', 'esp-clang']
runpy.run_path(os.path.join(TOOLS_DIR, 'idf_tools.py'), run_name='__main__')
```

或者用 `G:\Dev\ESP\idf.cmd`（它会 `set "MSYSTEM="`）。

**esp-clang 版本必须匹配**：IDF v6.1 要 `esp-21.1.3_20260408`；只有 `esp-19.1.2` 会导致 `esp-idf-sys` 的 build script **每次重跑都触发重装并写满磁盘**。这是「构建莫名失败 + 磁盘莫名满」的根因，排查时优先核对：

```bash
ls /g/Dev/ESP/Espressif/tools/esp-clang/          # 实际
jq -r '.tools[]|select(.name=="esp-clang")|.versions[0].win64.url' \
   /g/Dev/ESP/v6.1/esp-idf/tools/tools.json       # 要求
```

## 三、构建纪律

- 用 `G:\Dev\ESP\idf.cmd`；**不要在 Git Bash 里直接跑 `idf.py`**。
- 改过 `sdkconfig.defaults` 之后必须清掉 `esp-idf-sys` 的 build 产物再编，否则新配置不生效：
  ```bash
  rm -rf "$CARGO_TARGET_DIR/riscv32imc-esp-espidf/release/build/esp-idf-sys"
  ```
- 大改动用 `cargo check` 先过类型门禁（~10 分钟），别直接上 50 分钟的 LTO 链接。
- 本机网络传大文件会断流，走 `dl.espressif.cn` 镜像，下完**必须校验 SHA256**。
- `.cargo/config.toml` 里的 `CARGO_WORKSPACE_DIR = { value = "", relative = true }` 不是可选项 —— 去掉它 `embuild` 会算错 workspace 根，静默无视仓库里的 `sdkconfig` / `sdkconfig.defaults`。

## 四、安全线（改代码时一条都不能破）

1. **上电第一件事**是把 GPIO5 拉低 —— 顺序本身是设计的一部分，别调换。
2. **脉冲只在独立线程里生成**，绝不在 MQTT / 控制台回调里直接拉 GPIO。
3. `PULSE_MS_MIN` / `PULSE_MS_MAX` 是安全边界，**必须保持 `const`，永不写入 NVS**。脉冲 ≥4000ms 会触发主板强制断电。
4. 板子**没有自动复位电路**，烧录失败要手动拔 USB。

## 五、协作与调度

**主 agent 只做调度、指挥、向用户回报，并监控子 agent 是否异常；具体执行下派给子 agent。**

- 主 agent 不亲自下基层做长串的探索 / 构建 / 试错；把这类工作派给子 agent，自己保留全局视野和上下文余量。
- 主 agent 有**监控义务**：子 agent 卡住、跑偏、反复失败时，要主动发现并纠正或重新指派，不能放任它空转。
- 回报给用户时给结论和判断，不要把子 agent 的原始文件转储、日志倾倒到用户面前。

### 派活时的硬性要求 —— 防止 agent 卡死

**工具调用必须是一个完整的、独立的、结构正确的块。** 已出现过两次事故：工具块末尾混进了 XML 残渣（形如 `||DSML|| parameter>`），调用被截断，子 agent 收到的是半截消息，**然后卡死在那里等一个永远不会到的完整指令**。

- 一次只发**一个**工具调用块，不嵌套、不拼接、不在同一块里塞多个调用。
- 参数必须是干净的 JSON，**块内不允许出现任何游离的标签残渣或未闭合的 XML**。
- 发给子 agent 的 prompt 必须是**自包含的完整指令**：任务目标、输入路径、验收标准、失败时怎么办，一次写全。不要发「先这样，等下我再补」的碎片消息。
- 派活后要检查子 agent **是否真的动起来了**（有没有产出文件、有没有返回结果）。长时间无产出就是卡死信号，应立即干预而不是继续等。