@echo off
rem Render the real Qorral UI offscreen to a PNG and open it. No window opens.
rem
rem   scripts\snapshot.bat                       the example machines (--demo)
rem   scripts\snapshot.bat --empty               first run
rem   scripts\snapshot.bat --demo --remote --size 1280x800 --scale 1.5 --hover 400,300
rem
rem Options are passed to qorral-gui --snapshot. QORRAL_NO_OPEN=1 skips opening it (agents).
setlocal EnableDelayedExpansion
cd /d "%~dp0.."
call "%~dp0_setup.bat" || exit /b 1
set "ARGS=%*"
if "%~1"=="" set "ARGS=--demo"
if not exist artifacts mkdir artifacts
set "OUT=%CD%\artifacts\snapshot.png"
cargo build --release -j %JOBS% --quiet -p qorral || (echo Build failed. & exit /b 1)
cargo run --release -j %JOBS% --quiet -p qorral --bin qorral-gui -- --snapshot "%OUT%" %ARGS% || (echo Snapshot failed. & exit /b 1)
echo Saved %OUT%
if not defined QORRAL_NO_OPEN start "" "%OUT%"
exit /b 0
