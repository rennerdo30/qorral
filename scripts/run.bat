@echo off
rem Build and start Qorral from this checkout.
rem
rem   scripts\run.bat              debug build (fast to build)
rem   scripts\run.bat --release    optimized build (the real speed)
rem   scripts\run.bat --pull       update to the latest main first (fast-forward only)
rem   scripts\run.bat --trace      print startup and present timings when Qorral closes
rem
rem QORRAL_QEMU picks the QEMU build (default here: E:\AgentData\winq-build\bin);
rem QORRAL_HOME the library (default here: E:\AgentData\qorral, never the user's own).
setlocal EnableDelayedExpansion
cd /d "%~dp0.."
call "%~dp0_setup.bat" || exit /b 1
set "RELEASE=" & set "PULL=" & set "ARGS="
:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--release" (set "RELEASE=--release" & shift & goto parse)
if /i "%~1"=="--pull" (set "PULL=1" & shift & goto parse)
if /i "%~1"=="--trace" (set "QORRAL_TRACE=1" & shift & goto parse)
set "ARGS=!ARGS! %1"
shift
goto parse
:parsed
if defined PULL (git pull --ff-only origin main || (echo Pull failed. & exit /b 1))
if not defined QORRAL_QEMU if exist "E:\AgentData\winq-build\bin\qemu-system-x86_64.exe" set "QORRAL_QEMU=E:\AgentData\winq-build\bin"
if not defined QORRAL_HOME if exist "E:\AgentData" set "QORRAL_HOME=E:\AgentData\qorral"
echo Building Qorral %RELEASE%...
cargo build %RELEASE% -j %JOBS% --quiet -p qorral || (echo Build failed. & pause & exit /b 1)
cargo run %RELEASE% -j %JOBS% --quiet -p qorral --bin qorral-gui -- !ARGS!
exit /b %ERRORLEVEL%
