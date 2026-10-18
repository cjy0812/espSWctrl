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
REM ===========================================================================
set "MSYSTEM="
set "MINGW_PREFIX="
set "IDF_PATH=G:\Dev\ESP\v6.1\esp-idf"
set "IDF_TOOLS_PATH=G:\Dev\ESP\Espressif"
set "IDF_PYTHON_ENV_PATH=G:\Dev\ESP\Espressif\python_env\idf6.1_py3.14_env"
set "PYTHONUTF8=1"
set "ESP_IDF_VERSION=6.1.0"

REM esp-clang is needed by bindgen (libclang). It is NOT on idf.cmd's PATH
REM because the C build does not use bindgen.
REM Note: esp-clang 21.1.3 ships ONLY libclang_rt.* runtimes -- no libclang.dll,
REM so bindgen cannot use it. The dll lives in the separate esp-clang-libs package.
set "ESP_CLANG=G:\Dev\ESP\Espressif\tools\esp-clang\esp-21.1.3_20260408\esp-clang\bin"
set "LIBCLANG_PATH=G:\Dev\ESP\Espressif\tools\esp-clang-libs\esp-19.1.2_20250312\esp-clang\bin"

echo [cargo-build] LIBCLANG_PATH=%LIBCLANG_PATH%
echo [cargo-build] ESP_CLANG=%ESP_CLANG%
REM CRITICAL: %LIBCLANG_PATH% MUST be on PATH, not just in LIBCLANG_PATH.
REM   bindgen loads libclang.dll with LoadLibraryExW(..., 0), whose dependency
REM   resolution only searches already-loaded modules + PATH. libclang.dll
REM   depends on libLLVM-19.dll / libclang-cpp.dll, which live in the SAME dir.
set "PATH=G:\Dev\cargo\bin;%ESP_CLANG%;%LIBCLANG_PATH%;%IDF_PYTHON_ENV_PATH%\Scripts;G:\Dev\ESP\Espressif\tools\riscv32-esp-elf\esp-15.2.0_20251204\riscv32-esp-elf\bin;G:\Dev\ESP\Espressif\tools\cmake\3.30.2\bin;G:\Dev\ESP\Espressif\tools\ninja\1.12.1;%PATH%"

REM ---------------------------------------------------------------------------
REM The target dir must live outside the project root, and the path must be short.
REM   esp-idf-sys's build script rejects output dirs over a length threshold:
REM   "Error: Too long output directory ... Shorten your project path"
REM   NOTE: when the target dir moves, .cargo/config.toml MUST keep
REM   CARGO_WORKSPACE_DIR, otherwise embuild pops 6 levels from OUT_DIR and
REM   lands on N:\p instead of the crate root.
REM ---------------------------------------------------------------------------
set "CARGO_TARGET_DIR=N:\p"

REM ---------------------------------------------------------------------------
REM libclang dependency hell on Windows -- why the .dll copies below exist.
REM
REM   libclang.dll (MinGW-built, from esp-clang-libs/esp-19.1.2) needs the
REM   MinGW runtime: libstdc++-6.dll / libgcc_s_seh-1.dll / libwinpthread-1.dll.
REM   Windows resolves those dependencies via the standard search order, which
REM   for a LoadLibraryExW(..., 0) call is: exe dir -> system32 -> PATH.
REM
REM   esp-idf-sys rewrites the whole PATH with env::set_var("PATH", idf.exported_path)
REM   and idf.exported_path puts the IDF toolchain dirs FIRST. That means
REM   riscv32-esp-elf\bin (which also ships libstdc++-6.dll, built against a
REM   different GCC) wins over esp-clang-libs\bin -> libclang.dll loads the
REM   WRONG libstdc++ and fails with Win32 error 127, which bindgen reports as
REM   "the libclang shared library ... could not be opened: LoadLibraryExW failed".
REM
REM   Fix: put the esp-clang-libs MinGW runtime next to the build-script exe.
REM   The exe's own directory is searched BEFORE PATH, so the right DLLs win no
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
        copy /y "%CLANG_MINGW_DLLS%\libstdc++-6.dll" "%%D\out\" >nul 2>&1
        copy /y "%CLANG_MINGW_DLLS%\libgcc_s_seh-1.dll" "%%D\out\" >nul 2>&1
        copy /y "%CLANG_MINGW_DLLS%\libwinpthread-1.dll" "%%D\out\" >nul 2>&1
        echo [cargo-build] seeded libclang MinGW runtime into %%D\out
    )
)
exit /b 0
