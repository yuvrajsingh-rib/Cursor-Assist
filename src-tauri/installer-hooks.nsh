; CursorAssist registers itself under the per-user Run key on first launch ("Start with Windows").
; Remove that entry on uninstall so nothing is left trying to start a deleted program.
!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "CursorAssist"
!macroend
