@echo off
rem Opens Seek: starts the indexing service (asks for admin once) and the Seek window.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\launch.ps1"
if errorlevel 1 pause
