@echo off
rem Type-check wrapper. Writes the full cargo output plus the exit code to a log.
rem Paths are derived at runtime: no hard-coded absolute paths (see AGENTS.md).
setlocal
set "SCRIPT_DIR=%~dp0"
if not defined CARGO_TARGET_DIR set "CARGO_TARGET_DIR=G:\tmp\p"
if not exist "%CARGO_TARGET_DIR%" mkdir "%CARGO_TARGET_DIR%"
set "LOG=%CARGO_TARGET_DIR%\check.log"
call "%SCRIPT_DIR%cargo-build.cmd" check > "%LOG%" 2>&1
set "RC=%ERRORLEVEL%"
rem Redirect-first form: avoids the "echo EXIT=0>>" digit-eaten-by-redirect trap.
>> "%LOG%" echo EXIT=%RC%
echo [run-check] log: %LOG%
echo [run-check] EXIT=%RC%
exit /b %RC%
