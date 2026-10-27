; Understudy Windows installer (NSIS).
; Built by CI: flutter build windows --release output + the Rust engine
; binary + icon are staged into build\staging before makensis runs.
; NOTE: makensis changes its working directory to this script's folder by
; default, so CI must pass /NOCD for build\staging (repo-root relative)
; to resolve. Do not run this script bare from another directory.
; Unsigned installer: Windows SmartScreen will prompt on first run.
; That is expected (same as the owner's other unsigned apps).

!define APPNAME "Understudy"
!define COMPANY "Shawn Rodis"
!ifndef VERSION
  !define VERSION "0.1.1"
!endif

Name "${APPNAME} ${VERSION}"
OutFile "Understudy_Setup_${VERSION}.exe"
InstallDir "$PROGRAMFILES\${APPNAME}"
RequestExecutionLevel admin

Page directory
Page instfiles

UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File /r "build\staging\*.*"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\understudy.exe"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
  ; Do not delete the engine data dir (%APPDATA%\Understudy): it holds the
  ; owner's pairing token and action log. Removing the app must not silently
  ; destroy the audit trail.
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir /r "$INSTDIR"
SectionEnd
