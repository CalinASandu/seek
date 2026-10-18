@echo off
rem Closes the Seek window and stops the console-mode service.
powershell -NoProfile -ExecutionPolicy Bypass -Command "& '%~dp0scripts\stop.ps1'; Start-Sleep -Seconds 2"
