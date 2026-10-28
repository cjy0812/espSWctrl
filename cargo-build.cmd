@echo off
REM ===========================================================================
REM ESP-IDF v6.1 + Rust build entry
REM   usage: cargo-build.cmd check
REM          cargo-build.cmd build --release
REM
REM Why this file exists:
REM   Git Bash always exports MSYSTEM=MINGW64, and idf_tools.py / embuild both
REM   misbehave when they see it. `set "MSYSTEM="` only works from cmd.exe, so
REM   the build has to be launched through a .cmd, exactly like idf.cmd.
REM
REM CRITICAL: this file MUST be ASCII-only with CRLF line endings.
REM   cmd.exe parses .cmd files using the OEM code page. A UTF-8 file with
REM   LF-only endings gets mis-tokenized: comment lines execute as commands,
REM   "REM <chinese>" splits at bytes that look like spaces, and the whole
REM   environment block is silently lost. That produced a build that failed
REM   in ways that looked like CMake/cache bugs. Do not add non-ASCII here.
REM
REM CRITICAL: NO HARDCODED PATHS.
REM   This machine's ESP-IDF install was moved (de-junctioned) on 2026-10-27;
REM   every absolute path previously baked in here went stale and the build
REM   died at step one. All tool locations are now derived from the user-level
REM   environment variables IDF_PATH / IDF_TOOLS_PATH / IDF_PYTHON_ENV_PATH.
REM   If a tool directory cannot be found, this script FAILS LOUDLY instead of
REM   silently building against a path that no longer exists.
REM ===========================================================================
setlocal
set "MSYSTEM="
set "MINGW_PREFIX="

REM ---------------------------------------------------------------------------
REM 1. Required environment variables.
REM    Set these once in the user environment. Do not add absolute-path
REM    fallbacks here -- that is exactly what broke on 2026-10-27.
REM ---------------------------------------------------------------------------
if not defined IDF_PATH goto :missing_env
if not defined IDF_TOOLS_PATH goto :missing_env
if not defined IDF_PYTHON_ENV_PATH goto :missing_env
if not exist "%IDF_PATH%\tools\idf_tools.py" goto :bad_idf
if not exist "%IDF_PYTHON_ENV_PATH%\Scripts\python.exe" goto :bad_python

set "IDF_CCACHE_ENABLE=0"
set "PYTHONUTF8=1"

REM ---------------------------------------------------------------------------
REM Host toolchain selection.
REM   rust-toolchain.toml pins the channel as `nightly-2026-09-20`, which rustup
REM   resolves to the MSVC host. This machine has NO MSVC linker (no Visual
REM   Studio, no vswhere, the Windows SDK ships only arm64 libs), so every HOST
REM   artifact fails to link -- including the esp-idf-sys build script, which is
REM   a host program even though the crate target is riscv32. The gnu host
REM   toolchain ships its own mingw linker and compiles the same targets.
REM
REM   RUSTUP_TOOLCHAIN overrides rust-toolchain.toml for this process only, so
REM   the checked-in toolchain file stays valid on machines that do have MSVC.
REM   The value is a rustup identifier, not a path -- nothing to go stale here.
REM ---------------------------------------------------------------------------
if not defined RUSTUP_TOOLCHAIN set "RUSTUP_TOOLCHAIN=nightly-2026-09-20-x86_64-pc-windows-gnu"

REM ---------------------------------------------------------------------------
REM Cargo network settings.
REM   This machine reaches crates.io through a local proxy. Two cargo defaults
REM   break that path:
REM     * schannel certificate-revocation checks fail behind the proxy
REM       (CRYPT_E_REVOCATION_OFFLINE) and abort the download.
REM     * the default low-speed watchdog gives up after 30s under 10 B/s, which
REM       fires on a cold mirror index even though the transfer would finish.
REM   All three are non-path settings, so they stay literal. Proxy itself is
REM   taken from the inherited http_proxy / https_proxy variables.
REM ---------------------------------------------------------------------------
if not defined CARGO_HTTP_CHECK_REVOKE set "CARGO_HTTP_CHECK_REVOKE=false"
if not defined CARGO_HTTP_TIMEOUT set "CARGO_HTTP_TIMEOUT=180"
if not defined CARGO_HTTP_LOW_SPEED_LIMIT set "CARGO_HTTP_LOW_SPEED_LIMIT=1"
REM Proxy under multiplexing is the classic "connects but transfers 0 bytes"
REM symptom; single-request mode is slower but survives the local proxy.
if not defined CARGO_HTTP_MULTIPLEXING set "CARGO_HTTP_MULTIPLEXING=false"

REM ---------------------------------------------------------------------------
REM pip settings, inherited by idf_tools.py -> pip/uv.
REM   embuild unconditionally runs `idf_tools.py install-python-env`, which
REM   pip-installs setuptools from files.pythonhosted.org. That host dies with
REM   SSL UNEXPECTED_EOF behind the proxy, while the tuna PyPI mirror answers
REM   200 both direct and through the proxy. Point pip at the mirror so the
REM   bootstrap never has to touch pythonhosted.org.
REM ---------------------------------------------------------------------------
if not defined PIP_INDEX_URL set "PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple"
if not defined PIP_TRUSTED_HOST set "PIP_TRUSTED_HOST=pypi.tuna.tsinghua.edu.cn"
set "PIP_DISABLE_PIP_VERSION_CHECK=1"

REM Version string consumed by esp-idf-sys. Not a path, so it stays literal.
if not defined ESP_IDF_VERSION set "ESP_IDF_VERSION=6.1.0"

REM ---------------------------------------------------------------------------
REM Reuse the already-installed IDF toolchain; never download it again.
REM
REM   Default (`workspace`) makes esp-idf-sys/embuild treat the project-local
REM   .embuild\espressif as the tool root and re-run `idf_tools.py
REM   install-python-env` + re-download the whole toolset (clang tar.xz from
REM   GitHub, which SSL-fails behind the proxy). embuild-0.33.5/src/espidf.rs:690
REM   does this unconditionally -- there is no "already installed" short circuit
REM   for the workspace dir.
REM
REM   `fromenv` (esp-idf-sys BUILD-OPTIONS.md:203, build/native/cargo_driver.rs:191)
REM   instead uses the activated environment: it only needs $IDF_PATH plus
REM   `idf.py` and a dependency-complete `python` on PATH. Both are satisfied by
REM   the dirs prepended below, so nothing is fetched.
REM ---------------------------------------------------------------------------
if not defined ESP_IDF_TOOLS_INSTALL_DIR set "ESP_IDF_TOOLS_INSTALL_DIR=fromenv"

REM ---------------------------------------------------------------------------
REM Component manager: bypass the proxy for the two Espressif registry hosts.
REM
REM   IDF's component manager (idf_component_tools) fetches manifests and
REM   component archives from components.espressif.com and
REM   components-file.espressif.com. Both requests go through
REM   `requests`, which honours the http_proxy/https_proxy/all_proxy variables
REM   set at user level on this machine (http://127.0.0.1:7890, Bettbox).
REM
REM   Measured 2026-10-28 with the real certificate chain inspected:
REM     * through the proxy      -> SSLError for BOTH hosts
REM     * proxy bypassed (direct)-> HTTP 200 for BOTH hosts, verify=True,
REM                                 issuer = DigiCert Global G2 TLS RSA
REM                                 SHA256 2020 CA1 (the genuine chain)
REM   So the hosts are reachable and their certificates are valid; only the
REM   proxy's TLS handling of these two names is broken. Disabling certificate
REM   verification would therefore be the wrong fix -- and it would not even
REM   work: `download_file` (idf_component_tools/sources/web_service.py:70-73)
REM   builds its session via `create_session()` (registry/base_client.py:29),
REM   which never sets `session.verify`, so IDF_COMPONENT_VERIFY_SSL only
REM   affected the API probe, not the archive download.
REM
REM   NO_PROXY takes exact hostnames. Measured alternatives that do NOT work:
REM     NO_PROXY=espressif.com / .espressif.com
REM       -> components.espressif.com OK, but components-file.espressif.com
REM          times out (suffix matching does not cover the hyphenated host).
REM ---------------------------------------------------------------------------
if not defined NO_PROXY set "NO_PROXY=components.espressif.com,components-file.espressif.com"
if not defined no_proxy set "no_proxy=%NO_PROXY%"

REM ---------------------------------------------------------------------------
REM 2. Derive tool directories from IDF_TOOLS_PATH.
REM    `for /d` iterates matching version dirs in name order and the last one
REM    wins, which is the highest for the esp-YYYYMMDD / X.Y.Z naming IDF uses.
REM ---------------------------------------------------------------------------
set "ESP_CLANG="
for /d %%D in ("%IDF_TOOLS_PATH%\tools\esp-clang\esp-*") do set "ESP_CLANG=%%~fD\esp-clang\bin"

REM esp-clang 21.1.3 ships ONLY libclang_rt.* runtimes -- no libclang.dll,
REM so bindgen cannot use it. The dll lives in the separate esp-clang-libs
REM package, one level deeper than the package root (note the esp-clang dir).
set "LIBCLANG_PATH="
for /d %%D in ("%IDF_TOOLS_PATH%\tools\esp-clang-libs\esp-*") do set "LIBCLANG_PATH=%%~fD\esp-clang\bin"

set "RISCV_BIN="
for /d %%D in ("%IDF_TOOLS_PATH%\tools\riscv32-esp-elf\esp-*") do set "RISCV_BIN=%%~fD\riscv32-esp-elf\bin"

set "CMAKE_BIN="
for /d %%D in ("%IDF_TOOLS_PATH%\tools\cmake\*") do set "CMAKE_BIN=%%~fD\bin"

set "NINJA_BIN="
for /d %%D in ("%IDF_TOOLS_PATH%\tools\ninja\*") do set "NINJA_BIN=%%~fD"

if not exist "%ESP_CLANG%\clang.exe" goto :bad_clang
if not exist "%LIBCLANG_PATH%\libclang.dll" goto :bad_libclang
if not exist "%RISCV_BIN%\riscv32-esp-elf-gcc.exe" goto :bad_riscv
if not exist "%CMAKE_BIN%\cmake.exe" goto :bad_cmake
if not exist "%NINJA_BIN%\ninja.exe" goto :bad_ninja

echo [cargo-build] IDF_PATH      = %IDF_PATH%
echo [cargo-build] ESP_CLANG     = %ESP_CLANG%
echo [cargo-build] LIBCLANG_PATH = %LIBCLANG_PATH%

REM CRITICAL: %LIBCLANG_PATH% MUST be on PATH, not just in LIBCLANG_PATH.
REM   bindgen loads libclang.dll with LoadLibraryExW(..., 0), whose dependency
REM   resolution only searches already-loaded modules + PATH. libclang.dll
REM   depends on libLLVM-*.dll / libc++.dll, which live in the SAME dir.
REM   cargo itself is expected to be on PATH already; it is not added here.
REM   %IDF_PATH%\tools is required by ESP_IDF_TOOLS_INSTALL_DIR=fromenv:
REM   embuild looks up `idf.py` on PATH and canonicalizes it against
REM   %IDF_PATH%\tools\idf.py (embuild-0.33.5/src/espidf.rs:427-457).
REM   %IDF_PYTHON_ENV_PATH%\Scripts supplies the matching python.exe.
set "PATH=%ESP_CLANG%;%LIBCLANG_PATH%;%IDF_PYTHON_ENV_PATH%\Scripts;%IDF_PATH%\tools;%RISCV_BIN%;%CMAKE_BIN%;%NINJA_BIN%;%PATH%"

REM ---------------------------------------------------------------------------
REM The target dir must live outside the project root, and the path must be
REM short. esp-idf-sys's build script rejects output dirs over a length
REM threshold: "Error: Too long output directory ... Shorten your project path"
REM   NOTE: when the target dir moves, .cargo/config.toml MUST keep
REM   CARGO_WORKSPACE_DIR, otherwise embuild pops 6 levels from OUT_DIR and
REM   lands on the wrong directory instead of the crate root.
REM Override by exporting CARGO_TARGET_DIR before calling this script.
REM ---------------------------------------------------------------------------
if not defined CARGO_TARGET_DIR set "CARGO_TARGET_DIR=G:\tmp\p"
if not exist "%CARGO_TARGET_DIR%" mkdir "%CARGO_TARGET_DIR%"

REM ---------------------------------------------------------------------------
REM libclang dependency hell on Windows -- why the .dll copies below exist.
REM
REM   esp-clang 21.1.3's libclang.dll is built against libc++ (NOT libstdc++).
REM   Its actual imports are libc++.dll / libLLVM-*.dll / libunwind.dll, plus
REM   the mingw thread shim libwinpthread-1.dll. Older esp-clang (19.x) used
REM   libstdc++-6.dll / libgcc_s_seh-1.dll; those names are STALE and copying
REM   them here fails silently -- do not reintroduce them.
REM
REM   Windows resolves those dependencies via the standard search order, which
REM   for a LoadLibraryExW(..., 0) call is: exe dir -> system32 -> PATH.
REM
REM   esp-idf-sys rewrites the whole PATH with env::set_var("PATH", ...) and
REM   puts the IDF toolchain dirs FIRST. riscv32-esp-elf\bin also ships a
REM   libwinpthread-1.dll built against a different runtime, so it would win
REM   and libclang.dll would fail with Win32 error 127, which bindgen reports
REM   as "the libclang shared library ... could not be opened: LoadLibraryExW
REM   failed".
REM
REM   Fix: put the esp-clang-libs runtime next to the build-script exe. The
REM   exe's own directory is searched BEFORE PATH, so the right DLLs win no
REM   matter how esp-idf-sys rewrites PATH.
REM ---------------------------------------------------------------------------
set "CLANG_MINGW_DLLS=%LIBCLANG_PATH%"

call :seed_clang_dlls
cargo %*
set "RC=%ERRORLEVEL%"

REM NOTE: this MUST be a goto, not an `if (...)` block. Inside a parenthesized
REM block cmd.exe expands %ERRORLEVEL% when the block is PARSED, not when each
REM line runs, so `set "RC=%ERRORLEVEL%"` inside the block would capture the
REM pre-cargo value (0) and the gate would report EXIT=0 on a real failure.
if not "%RC%"=="0" goto retry_once
exit /b %RC%

:retry_once
REM A fresh target dir has no build-script out dir yet, so the seed above had
REM nothing to copy into. Seed again and retry once.
echo [cargo-build] first pass failed rc=%RC%, seeding libclang DLLs and retrying once
call :seed_clang_dlls
cargo %*
exit /b %ERRORLEVEL%

:seed_clang_dlls
for /d %%D in ("%CARGO_TARGET_DIR%\debug\build\esp-idf-sys\*" "%CARGO_TARGET_DIR%\release\build\esp-idf-sys\*") do (
    if exist "%%D\out\build_script_build.exe" (
        copy /y "%CLANG_MINGW_DLLS%\libc++.dll" "%%D\out\" >nul
        if errorlevel 1 echo [cargo-build] WARN: libc++.dll not found in %CLANG_MINGW_DLLS%
        copy /y "%CLANG_MINGW_DLLS%\libLLVM-*.dll" "%%D\out\" >nul
        if errorlevel 1 echo [cargo-build] WARN: libLLVM-*.dll not found in %CLANG_MINGW_DLLS%
        copy /y "%CLANG_MINGW_DLLS%\libunwind.dll" "%%D\out\" >nul
        if errorlevel 1 echo [cargo-build] WARN: libunwind.dll not found in %CLANG_MINGW_DLLS%
        copy /y "%CLANG_MINGW_DLLS%\libwinpthread-1.dll" "%%D\out\" >nul
        if errorlevel 1 echo [cargo-build] WARN: libwinpthread-1.dll not found in %CLANG_MINGW_DLLS%
        echo [cargo-build] seeded libclang runtime into %%D\out
    )
)
exit /b 0

REM ---------------------------------------------------------------------------
REM Loud failure paths. Never fall back to a guessed absolute path.
REM ---------------------------------------------------------------------------
:missing_env
echo [cargo-build] ERROR: IDF_PATH / IDF_TOOLS_PATH / IDF_PYTHON_ENV_PATH is not set.
echo [cargo-build]        Set them in the user environment, then reopen the shell.
exit /b 1

:bad_idf
echo [cargo-build] ERROR: %IDF_PATH%\tools\idf_tools.py not found.
exit /b 1

:bad_python
echo [cargo-build] ERROR: %IDF_PYTHON_ENV_PATH%\Scripts\python.exe not found.
exit /b 1

:bad_clang
echo [cargo-build] ERROR: esp-clang not found under %IDF_TOOLS_PATH%\tools\esp-clang\esp-*
exit /b 1

:bad_libclang
echo [cargo-build] ERROR: libclang.dll not found under %IDF_TOOLS_PATH%\tools\esp-clang-libs\esp-*
echo [cargo-build]        Run idf_tools.py install esp-clang to fetch the libs package.
exit /b 1

:bad_riscv
echo [cargo-build] ERROR: riscv32-esp-elf-gcc.exe not found under %IDF_TOOLS_PATH%\tools\riscv32-esp-elf\esp-*
exit /b 1

:bad_cmake
echo [cargo-build] ERROR: cmake.exe not found under %IDF_TOOLS_PATH%\tools\cmake\*
exit /b 1

:bad_ninja
echo [cargo-build] ERROR: ninja.exe not found under %IDF_TOOLS_PATH%\tools\ninja\*
exit /b 1