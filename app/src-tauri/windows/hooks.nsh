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

; Optional last step of Seek's own uninstaller: delete Seek's index and settings.
;
; Guards, each one keeps this step from running or deleting when it shouldn't:
;  - ${Silent} (/S): a silent uninstall has nobody to ask, so it keeps the data. The question's
;    /SD IDNO answers No by itself, so a prompt that does appear in a silent run can't delete.
;  - $PassiveMode (/P): the progress-only mode is also meant to run without questions.
;  - $UpdateMode = 1 (/UPDATE): an upgrade runs this uninstaller before the new files go in. The user
;    is keeping Seek, so the index (which costs a full rescan to rebuild) and settings must stay.
;  - The question defaults to No (MB_DEFBUTTON2): Enter, Escape or closing the box deletes nothing.
;  - Only two folders are ever removed: Seek's folder in ProgramData (the index and device list)
;    and the uninstalling user's own %APPDATA%\Seek (settings.json). Nothing else. The webview data
;    in %LOCALAPPDATA%\com.calinsandrei.seek is out of scope: Tauri's own "Delete app data" checkbox
;    handles it.
;  - NSIS has no $COMMONAPPDATA, and $APPDATA means ProgramData while SetShellVarContext is "all"
;    (a per-machine uninstaller usually runs that way) but the user's Roaming folder while it is
;    "current". So ProgramData is read under "all", the user's folder under "current", and the context
;    it had before is put back (Tauri's template switches to "current" itself in one branch).
;  - Neither folder is touched if $APPDATA comes back empty, so RmDir can't be handed a bare "\Seek".
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} ${Silent}
  ${OrIf} $PassiveMode = 1
  ${OrIf} $UpdateMode = 1
    ; keep everything
  ${Else}
    MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also delete Seek's index and settings? This is permanent." /SD IDNO IDNO seek_keep_data
      Push $0
      Push $1
      Push $2
      StrCpy $0 "$APPDATA" ; what $APPDATA means now, so the context can be put back
      SetShellVarContext current
      StrCpy $1 "$APPDATA" ; this user's Roaming folder
      SetShellVarContext all
      StrCpy $2 "$APPDATA" ; ProgramData
      ${If} $2 != ""
        RmDir /r "$2\Seek"
      ${EndIf}
      ${If} $1 != ""
        RmDir /r "$1\Seek"
      ${EndIf}
      ${If} $0 == $1
        SetShellVarContext current
      ${EndIf}
      Pop $2
      Pop $1
      Pop $0
    seek_keep_data:
  ${EndIf}
!macroend
