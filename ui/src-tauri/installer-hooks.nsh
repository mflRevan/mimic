; Mimic installer hooks (Tauri NSIS, per-user install, no admin rights).
;
; Files outside $INSTDIR that Mimic owns:
;   %APPDATA%\Mimic       settings (config.toml) and the dictionary (rules.json)
;   %LOCALAPPDATA%\Mimic  models, engine runtimes, logs, update downloads, backups
;   HKCU\...\Run "Mimic"  start at sign-in (written by the app itself)
;   user PATH             $INSTDIR, so `mimic` works in a terminal
;
; Modes: a normal install, an in-app update (`/P /UPDATE`: passive, keeps shortcuts),
; and the uninstaller, which Tauri also runs with /UPDATE when replacing a version.

Var MimicWasRunning

; Stop Mimic so its files can be replaced. Ask nicely first (the daemon finishes
; writing settings and stops its engine servers), then make sure nothing is left.
!macro MIMIC_STOP
  StrCpy $MimicWasRunning 0
  nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq mimic-daemon.exe" /NH | find /I "mimic-daemon.exe"'
  Pop $0
  ${If} $0 = 0
    StrCpy $MimicWasRunning 1
    ${If} ${FileExists} "$INSTDIR\mimic.exe"
      ; Bounded: a daemon that doesn't answer is killed below instead of hanging setup.
      nsExec::Exec /TIMEOUT=10000 '"$INSTDIR\mimic.exe" stop'
      Pop $0
    ${EndIf}
  ${EndIf}
  nsExec::Exec 'taskkill /F /IM mimic-daemon.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM mimic-settings.exe'
  Pop $0
  ; A `mimic logs -f` left open in a terminal would lock mimic.exe.
  nsExec::Exec 'taskkill /F /IM mimic.exe'
  Pop $0
  ; Give Windows a moment to release the file handles of killed processes.
  Sleep 400
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro MIMIC_STOP
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Shortcuts and the taskbar keep showing a cached icon after an update; refresh it.
  nsExec::Exec 'ie4uinit.exe -show'
  Pop $0
  ; Put the `mimic` CLI on the user's PATH (handles quotes and %VARS% in PATH safely).
  nsExec::Exec '"$INSTDIR\mimic.exe" setup path-add'
  Pop $0
  ; Updating or reinstalling a Mimic that was set up: bring it back in the tray
  ; without opening a window. It also restores the sign-in entry, which the old
  ; uninstaller removes when the user picks "uninstall before installing".
  ; A first install starts from the finish page instead, which opens onboarding.
  ${If} $MimicWasRunning = 1
  ${OrIf} $UpdateMode = 1
  ${OrIf} ${FileExists} "$APPDATA\Mimic\config.toml"
    Exec '"$INSTDIR\mimic-daemon.exe" --background'
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro MIMIC_STOP
  ; Replacing this version with another one keeps PATH (the new install needs it anyway).
  ${If} $UpdateMode <> 1
    nsExec::Exec '"$INSTDIR\mimic.exe" setup path-remove'
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    SetShellVarContext current
    ; Downloaded update installers are never worth keeping.
    RMDir /r "$LOCALAPPDATA\Mimic\updates"
    ; "Delete the application data" (unchecked by default). Tauri only clears its own
    ; WebView data; Mimic keeps its settings and models under its own name.
    ${If} $DeleteAppDataCheckboxState = 1
      RMDir /r "$APPDATA\Mimic"
      RMDir /r "$LOCALAPPDATA\Mimic"
    ${EndIf}
  ${EndIf}
!macroend
