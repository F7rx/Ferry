; Explorer integration, per user (no admin rights), removed on uninstall.
;
; - "Send with Ferry" on files and folders. Windows 11 lists classic verbs
;   under "Show more options"; each selected item starts the app once and the
;   single-instance handler forwards it to the running window.
; - "Send to › Ferry" passes every selected item in one launch.

!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "Software\Classes\*\shell\Ferry" "" "Send with Ferry"
  WriteRegStr HKCU "Software\Classes\*\shell\Ferry" "Icon" '"$INSTDIR\${MAINBINARYNAME}.exe",0'
  WriteRegStr HKCU "Software\Classes\*\shell\Ferry\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" "%1"'
  WriteRegStr HKCU "Software\Classes\Directory\shell\Ferry" "" "Send with Ferry"
  WriteRegStr HKCU "Software\Classes\Directory\shell\Ferry" "Icon" '"$INSTDIR\${MAINBINARYNAME}.exe",0'
  WriteRegStr HKCU "Software\Classes\Directory\shell\Ferry\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" "%1"'
  CreateShortCut "$APPDATA\Microsoft\Windows\SendTo\Ferry.lnk" "$INSTDIR\${MAINBINARYNAME}.exe" "" "$INSTDIR\${MAINBINARYNAME}.exe" 0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DeleteRegKey HKCU "Software\Classes\*\shell\Ferry"
  DeleteRegKey HKCU "Software\Classes\Directory\shell\Ferry"
  Delete "$APPDATA\Microsoft\Windows\SendTo\Ferry.lnk"
!macroend
