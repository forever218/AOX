@echo off
rem AOX 远程主机桥 —— 双击即可常驻。
rem 前提：已经跑过 `aox remote pair`（配对一次就够，凭证存在 .aox\remote.json）。
cd /d "%~dp0.."

if not exist "target\debug\aox.exe" (
  echo 找不到 target\debug\aox.exe
  echo 先在项目根目录执行： cargo build --bin aox
  pause
  exit /b 1
)

title AOX Remote
target\debug\aox.exe remote serve
rem serve 异常退出时别把窗口闪没，留着看报错
pause
