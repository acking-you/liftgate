@echo off
where pwsh >nul 2>nul
if errorlevel 1 goto :use_winps
pwsh -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-gateway.ps1" %*
exit /b %ERRORLEVEL%

:use_winps
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-gateway.ps1" %*
exit /b %ERRORLEVEL%
