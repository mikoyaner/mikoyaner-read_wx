@echo off
rem ============================================================
rem WeChat chat exporter panel launcher (double-click).
rem Keep this file ASCII-only and CRLF: cmd.exe parses batch
rem lines with the console codepage, so UTF-8 Chinese here
rem gets mojibake-ed into phantom commands before chcp runs.
rem ============================================================
chcp 65001 >nul
cd /d "%~dp0"

where python >nul 2>nul
if errorlevel 1 (
  echo [X] python not found. Install Python 3.10+ and enable "Add python.exe to PATH".
  pause
  exit /b 1
)

python -m wxmsg.cli %*
if errorlevel 1 pause
