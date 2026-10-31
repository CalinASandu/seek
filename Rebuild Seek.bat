@echo off
rem Rebuilds Seek from the current source and runs it: service, CLI and window (asks for admin once).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\rebuild.ps1"
if errorlevel 1 pause
