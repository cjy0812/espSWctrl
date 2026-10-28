#!/usr/bin/env python3
"""跨平台门禁：路径中立 + 文本纪律。

纯标准库，Windows / Linux / macOS 通用 —— 本脚本自身就是
AGENTS.md「平台中立边界」L0 / L1 两档的可执行版本。

用法：
    python scripts/check-portability.py            # 检查仓库根（脚本的上两级）
    python scripts/check-portability.py --root DIR
    python scripts/check-portability.py --verbose   # 打印通过项与白名单命中

退出码：0 = 通过；1 = 有 ERROR；2 = 脚本自身出错。

检查项
    P1  绝对路径        代码 / 配置的**有效行**禁止写死盘符、UNC、/mnt/x。
    P2  .cmd / .bat     必须纯 ASCII + 全 CRLF。cmd 按 OEM 码页解析，
                        非 ASCII 注释会被当作命令执行；LF 会让 goto/标签解析出错。
    P3  .gitattributes  必须强制 *.cmd / *.bat 为 eol=crlf，
                        否则非 Windows 检出后拿到 LF。
    P4  rust-toolchain  必须声明 rustfmt + clippy 组件，否则 fmt/clippy 调不出来。

**为什么不扫注释、不扫 Markdown**

注释与文档里的路径不参与执行，写死与否不影响构建或运行；它们记录的是
「历史上为什么这么改」（本仓库两次事故：junction 搬迁、去链接化改造）。
把它们当错误会让门禁产生大量噪音，进而被人习惯性忽略 —— 那样门禁就死了。

这类内容的时效性由人工 review 与 `/doctor prompt-audit` 负责，不由本脚本。
唯一例外是「可被环境变量覆盖的兜底默认值」，它确实写在有效行里，
只能逐条登记到 ALLOWLIST 并写明理由。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

SKIP_DIRS = {
    ".git",
    "target",
    ".embuild",
    "managed_components",
    "__pycache__",
    ".venv",
    "node_modules",
    "dist",
}

CODE_SUFFIXES = {
    ".rs", ".toml", ".json", ".py", ".cmd", ".bat",
    ".ps1", ".sh", ".yml", ".yaml", ".cmake",
}
CODE_NAMES = {
    ".gitattributes", ".gitignore", ".editorconfig",
    "sdkconfig.defaults", "Cargo.lock", "dependencies.lock",
}
DOC_SUFFIXES = {".md"}

# 盘符 / UNC / WSL 挂载点。负向断言避免把 `Self::` 之类的双冒号误判。
ABS_PATTERNS = [
    ("盘符路径", re.compile(r"(?<![A-Za-z0-9_])[A-Za-z]:[\\/]")),
    ("UNC 路径", re.compile(r"\\\\[A-Za-z0-9_.\-]+\\")),
    ("WSL 挂载", re.compile(r"/mnt/[a-z]/")),
]

# 历史上真实踩过、且任何有效行里都不该再出现的路径。
# 注意：不收录 G:\tmp\p —— 它是合法的兜底默认值（见 ALLOWLIST）。
DEAD_PATH_PATTERNS = [
    # 缺一级 ESP32 的错误仓库根。本仓库两次 shell 脚本因此静默失效。
    ("错误仓库根", re.compile(r"[A-Za-z]:[\\/]GitHub[\\/]espSWctrl", re.IGNORECASE)),
    # 已删除的 junction 目标。
    ("已废弃 junction", re.compile(r"[A-Za-z]:[\\/]_esp32", re.IGNORECASE)),
]

# 白名单：静态检查无法判断「可被环境变量覆盖的兜底默认值」这类合法例外，
# 只能逐条登记并写明理由。新增条目必须附理由。
ALLOWLIST: list[tuple[re.Pattern[str], re.Pattern[str], str]] = [
    (
        re.compile(r"^(cargo-build|run-check)\.cmd$"),
        re.compile(r"CARGO_TARGET_DIR=[A-Za-z]:[\\/]"),
        "可被环境变量覆盖的兜底默认值（AGENTS.md 允许的唯一例外）",
    ),
]

# 注释前缀，按后缀区分。json 无注释语法，故不在表内。
COMMENT_PREFIXES: dict[str, tuple[str, ...]] = {
    ".rs": ("//", "/*", "*"),
    ".toml": ("#",),
    ".py": ("#",),
    ".sh": ("#",),
    ".ps1": ("#",),
    ".yml": ("#",),
    ".yaml": ("#",),
    ".cmake": ("#",),
    ".cmd": ("rem ", "rem\t", "::", "@rem "),
    ".bat": ("rem ", "rem\t", "::", "@rem "),
    ".gitignore": ("#",),
    ".gitattributes": ("#",),
    ".editorconfig": ("#",),
    "sdkconfig.defaults": ("#",),
}


class Report:
    def __init__(self, verbose: bool) -> None:
        self.errors: list[str] = []
        self.warns: list[str] = []
        self.notes: list[str] = []
        self.verbose = verbose
        self.checks = 0

    def error(self, where: str, tag: str, msg: str) -> None:
        self.errors.append(f"ERROR {where} [{tag}] {msg}")

    def warn(self, where: str, tag: str, msg: str) -> None:
        self.warns.append(f"WARN  {where} [{tag}] {msg}")

    def note(self, msg: str) -> None:
        if self.verbose:
            self.notes.append(f"ok    {msg}")


def iter_files(root: Path):
    """遍历仓库文件，跳过构建产物与版本库目录。"""
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        rel = path.relative_to(root)
        if any(part in SKIP_DIRS for part in rel.parts):
            continue
        yield rel, path


def classify(rel: Path) -> str:
    name = rel.name
    suffix = rel.suffix.lower()
    if name in CODE_NAMES or suffix in CODE_SUFFIXES:
        return "code"
    if suffix in DOC_SUFFIXES:
        return "doc"
    return "other"


def is_comment(line: str, rel: Path) -> bool:
    """该行是否为注释。注释不参与执行，不纳入路径检查。"""
    stripped = line.lstrip()
    if not stripped:
        return True
    key = rel.name if rel.name in COMMENT_PREFIXES else rel.suffix.lower()
    prefixes = COMMENT_PREFIXES.get(key)
    if prefixes is None:
        return False
    # cmd 的 rem 大小写不敏感
    lowered = stripped.lower()
    return any(lowered.startswith(p) for p in prefixes)


def read_text(path: Path) -> str:
    return path.read_bytes().decode("utf-8", errors="replace")


def allowed(rel: Path, line: str) -> str | None:
    for name_pat, line_pat, reason in ALLOWLIST:
        if name_pat.search(rel.name) and line_pat.search(line):
            return reason
    return None


def check_abs_paths(rel: Path, path: Path, kind: str, rep: Report) -> None:
    if kind != "code":
        return
    for lineno, line in enumerate(read_text(path).splitlines(), 1):
        if is_comment(line, rel):
            continue
        for label, pattern in ABS_PATTERNS:
            if not pattern.search(line):
                continue
            reason = allowed(rel, line)
            if reason:
                rep.note(f"{rel}:{lineno} 白名单（{label}）：{reason}")
                continue
            rep.error(f"{rel}:{lineno}", "P1", f"{label}：{line.strip()[:100]}")


def check_dead_paths(rel: Path, path: Path, kind: str, rep: Report) -> None:
    if kind != "code":
        return
    for lineno, line in enumerate(read_text(path).splitlines(), 1):
        if is_comment(line, rel):
            continue
        for label, pattern in DEAD_PATH_PATTERNS:
            if pattern.search(line) and not allowed(rel, line):
                rep.error(f"{rel}:{lineno}", "P5", f"{label}：{line.strip()[:100]}")


def check_cmd_encoding(rel: Path, path: Path, rep: Report) -> None:
    data = path.read_bytes()
    non_ascii = [(i, b) for i, b in enumerate(data) if b >= 0x80]
    if non_ascii:
        i, b = non_ascii[0]
        line = data[:i].count(b"\n") + 1
        rep.error(
            f"{rel}:{line}", "P2",
            f"含非 ASCII 字节 0x{b:02x}（共 {len(non_ascii)} 处）",
        )
    lf = data.count(b"\n")
    crlf = data.count(b"\r\n")
    if lf != crlf:
        rep.error(f"{rel}:1", "P2", f"{lf - crlf} 处裸 LF（cmd 需要全 CRLF）")
    if not non_ascii and lf == crlf:
        rep.note(f"{rel} 纯 ASCII + 全 CRLF")


def check_gitattributes(root: Path, rep: Report) -> None:
    path = root / ".gitattributes"
    if not path.is_file():
        rep.error(".gitattributes", "P3", "文件不存在")
        return
    covered = set()
    for line in read_text(path).splitlines():
        parts = line.strip().split()
        if len(parts) >= 2 and "eol=crlf" in parts[1:] and parts[0] in {"*.cmd", "*.bat"}:
            covered.add(parts[0])
    missing = {"*.cmd", "*.bat"} - covered
    if missing:
        rep.error(".gitattributes", "P3", f"缺少 {', '.join(sorted(missing))} 的 eol=crlf")
    else:
        rep.note(".gitattributes 已强制 *.cmd / *.bat 为 CRLF")


def check_toolchain(root: Path, rep: Report) -> None:
    path = root / "rust-toolchain.toml"
    if not path.is_file():
        rep.error("rust-toolchain.toml", "P4", "文件不存在")
        return
    match = re.search(r"components\s*=\s*\[([^\]]*)\]", read_text(path), re.S)
    if not match:
        rep.error("rust-toolchain.toml", "P4", "缺少 components 数组")
        return
    items = set(re.findall(r'"([^"]+)"', match.group(1)))
    missing = {"rustfmt", "clippy"} - items
    if missing:
        rep.error("rust-toolchain.toml", "P4", f"components 缺少 {', '.join(sorted(missing))}")
    else:
        rep.note("rust-toolchain.toml 已声明 rustfmt + clippy")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="跨平台门禁：路径中立 + 文本纪律")
    parser.add_argument(
        "--root",
        default=str(Path(__file__).resolve().parent.parent),
        help="仓库根目录（默认取脚本的上两级）",
    )
    parser.add_argument("--verbose", action="store_true", help="打印通过项与白名单命中")
    args = parser.parse_args(argv)

    root = Path(args.root).resolve()
    if not (root / "Cargo.toml").is_file():
        print(f"错误：{root} 不像是仓库根（缺 Cargo.toml）", file=sys.stderr)
        return 2

    rep = Report(args.verbose)
    self_path = Path(__file__).resolve()

    for rel, path in iter_files(root):
        if path.resolve() == self_path:
            continue  # 本脚本自身含检测模式字面量
        rep.checks += 1
        kind = classify(rel)
        check_abs_paths(rel, path, kind, rep)
        check_dead_paths(rel, path, kind, rep)
        if rel.suffix.lower() in {".cmd", ".bat"}:
            check_cmd_encoding(rel, path, rep)

    check_gitattributes(root, rep)
    check_toolchain(root, rep)

    for line in rep.notes + rep.warns + rep.errors:
        print(line)

    print()
    print(f"扫描 {rep.checks} 个文件：{len(rep.errors)} ERROR / {len(rep.warns)} WARN")
    if rep.errors:
        print("结论：不通过。P1 的合法例外必须登记到 ALLOWLIST 并写明理由。")
        return 1
    print("结论：通过。")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except Exception as exc:  # noqa: BLE001 - 顶层兜底，给出可读错误
        print(f"脚本自身出错：{exc}", file=sys.stderr)
        sys.exit(2)