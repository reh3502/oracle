@echo off
cd /d "%~dp0"
"%~dp0payload\python\python.exe" "%~dp0apply-windows-module-fix.py"
pause
