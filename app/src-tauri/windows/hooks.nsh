; Seek installer hooks: register the indexing service with the app and remove it on uninstall.
; The installer is per-machine (Program Files), so the service binary that runs as SYSTEM is never
; in a folder a normal user can write to.

!macro NSIS_HOOK_PREINSTALL
  ; Upgrade/repair: stop the running service so seek-service.exe can be replaced.
  nsExec::Exec 'powershell.exe -NoProfile -NonInteractive -Command "Stop-Service -Name SeekService -ErrorAction SilentlyContinue"'
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Registers (or updates) SeekService and starts it.
  nsExec::ExecToLog '"$INSTDIR\seek-service.exe" install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Stop and delete the service before its files are removed. The index in %ProgramData%\Seek is kept
  ; so a reinstall does not have to rescan every drive.
  nsExec::ExecToLog '"$INSTDIR\seek-service.exe" uninstall'
!macroend
