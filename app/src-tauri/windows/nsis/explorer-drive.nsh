!define TERARELAY_EXPLORER_CLSID "{3F1DA139-ED78-4BD2-9C96-EBF7981E5E4B}"
!define TERARELAY_FOLDER_INSTANCE_CLSID "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}"

; Microsoft-documented root-level File Explorer namespace integration for the
; same per-user folder that TeraRelay registers with the Cloud Files API.
!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}" "" "TeraRelay"
  WriteRegExpandStr HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}\DefaultIcon" "" "%SystemRoot%\system32\imageres.dll,-1043"
  WriteRegDWORD HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}" "System.IsPinnedToNameSpaceTree" 0x1
  WriteRegDWORD HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}" "SortOrderIndex" 0x42
  WriteRegExpandStr HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}\InProcServer32" "" "%SystemRoot%\system32\shell32.dll"
  WriteRegStr HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}\Instance" "CLSID" "${TERARELAY_FOLDER_INSTANCE_CLSID}"
  WriteRegDWORD HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}\Instance\InitPropertyBag" "Attributes" 0x11
  WriteRegExpandStr HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}\Instance\InitPropertyBag" "TargetFolderPath" "%USERPROFILE%\TeraRelay Drive"
  WriteRegDWORD HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}\ShellFolder" "FolderValueFlags" 0x28
  WriteRegDWORD HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}\ShellFolder" "Attributes" 0xF080004D
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\${TERARELAY_EXPLORER_CLSID}" "" "TeraRelay"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel" "${TERARELAY_EXPLORER_CLSID}" 0x1
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Preserve the Cloud Files registration during /UPDATE so dehydrated
  ; placeholders survive an in-place app upgrade. A true uninstall must
  ; detach the sync root before removing the executable.
  ${If} $UpdateMode != 1
    ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --unregister-drive-sync-root' $0
    ${If} $0 != 0
      MessageBox MB_ICONSTOP|MB_OK "TeraRelay could not detach its Windows Drive. Close TeraRelay and retry uninstalling."
      Abort
    ${EndIf}
  ${EndIf}

  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\${TERARELAY_EXPLORER_CLSID}"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel" "${TERARELAY_EXPLORER_CLSID}"
  DeleteRegKey HKCU "Software\Classes\CLSID\${TERARELAY_EXPLORER_CLSID}"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
!macroend
