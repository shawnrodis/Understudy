; Understudy Windows installer (NSIS).
; Built by CI: flutter build windows --release output + the Rust engine
; binary + icon are staged into build\staging before makensis runs.
; NOTE: makensis changes its working directory to this script's folder by
; default, so CI must pass /NOCD for build\staging (repo-root relative)
; to resolve. Do not run this script bare from another directory.
; Unsigned installer: Windows SmartScreen will prompt on first run.
; That is expected (same as the owner's other unsigned apps).
; The install pages are: folder choice, then a desktop-shortcut choice
; (checkbox, checked by default), then install.

!include "nsDialogs.nsh"
!include "LogicLib.nsh"

!define APPNAME "Understudy"
!define COMPANY "Shawn Rodis"
!ifndef VERSION
  !define VERSION "0.1.5"
!endif

Name "${APPNAME} ${VERSION}"
OutFile "Understudy_Setup_${VERSION}.exe"
InstallDir "$PROGRAMFILES\${APPNAME}"
RequestExecutionLevel admin

; Desktop-shortcut choice state (default: checked).
Var DesktopCheckbox
Var DesktopCheckboxState

Page directory
Page custom DesktopPageCreate DesktopPageLeave
Page instfiles

Function DesktopPageCreate
  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 0 0 100% 12u "Understudy can add a shortcut to your desktop for quick launching."
  Pop $0
  ${NSD_CreateCheckbox} 0 16u 100% 12u "Create a desktop shortcut"
  Pop $DesktopCheckbox
  ${NSD_SetState} $DesktopCheckbox ${BST_CHECKED}
  nsDialogs::Show
FunctionEnd

Function DesktopPageLeave
  ${NSD_GetState} $DesktopCheckbox $DesktopCheckboxState
FunctionEnd

UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File /r "build\staging\*.*"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\understudy.exe"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\Uninstall.exe"
  ${If} $DesktopCheckboxState == ${BST_CHECKED}
    CreateShortcut "$DESKTOP\Understudy.lnk" "$INSTDIR\understudy.exe"
  ${EndIf}
SectionEnd

Section "Uninstall"
  ; Do not delete the engine data dir (%APPDATA%\Understudy): it holds the
  ; owner's pairing token and action log. Removing the app must not silently
  ; destroy the audit trail.
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  Delete "$DESKTOP\Understudy.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir /r "$INSTDIR"
SectionEnd
