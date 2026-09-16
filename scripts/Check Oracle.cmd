@echo off
cd /d "%~dp0"
echo Leave the bot running while this check runs.
"%~dp0payload\python\python.exe" "%~dp0windows-diagnostics.py"
if errorlevel 1 goto failed
start "" notepad.exe "%~dp0Oracle diagnostics.json"
exit /b
:failed
echo Put both helper files beside Start Oracle.exe in the extracted bot folder.
pause
