@echo off
call "G:\GitHub\espSWctrl\cargo-build.cmd" check > N:\tmp\g2check.log 2>&1
echo EXIT=%ERRORLEVEL% >> N:\tmp\g2check.log
