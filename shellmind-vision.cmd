@echo off
setlocal
set "ROOT_DIR=%~dp0"
if "%ROOT_DIR:~-1%"=="\" set "ROOT_DIR=%ROOT_DIR:~0,-1%"
set "NODE_EXE=node"
if exist "%ROOT_DIR%\runtime\node\node.exe" set "NODE_EXE=%ROOT_DIR%\runtime\node\node.exe"

if "%~1"=="stdio" (
  "%NODE_EXE%" "%ROOT_DIR%\mcp-stdio.mjs"
  exit /b %ERRORLEVEL%
)
if "%~1"=="serve" (
  "%NODE_EXE%" "%ROOT_DIR%\server.mjs"
  exit /b %ERRORLEVEL%
)
echo Usage: shellmind-vision.cmd ^<stdio^|serve^>
exit /b 1
