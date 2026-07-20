@echo off
setlocal

rem Launch the prebuilt Nocturne TUI (which spawns the backend) without building.
set "ROOT=%~dp0"
set "TUI=%ROOT%target\release\nocturne-tui.exe"

if not exist "%TUI%" set "TUI=%ROOT%target\debug\nocturne-tui.exe"

if not exist "%TUI%" (
    echo Could not find a prebuilt nocturne-tui.exe in target\release or target\debug.
    echo Build it first with: cargo build --release
    exit /b 1
)

"%TUI%" %*
