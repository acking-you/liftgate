@echo off
setlocal DisableDelayedExpansion
if exist "%ProgramFiles%\PowerShell\7\pwsh.exe" goto :use_progfiles_pwsh
"%SystemRoot%\System32\where.exe" pwsh.exe >nul 2>nul
if errorlevel 1 goto :use_winps

pwsh.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0start-agy.ps1" %*
exit /b %ERRORLEVEL%

:use_progfiles_pwsh
"%ProgramFiles%\PowerShell\7\pwsh.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0start-agy.ps1" %*
exit /b %ERRORLEVEL%

:use_winps
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0start-agy.ps1" %*
exit /b %ERRORLEVEL%
