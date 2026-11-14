; Harness NSIS installer hooks (bundle.windows.nsis.installerHooks).
; Spec: Worse/docs/specs/harness/phase-6-packaging.md section 6.
;
; PREINSTALL / PREUNINSTALL: stop Harness processes running from $INSTDIR (graceful close first)
;   before Tauri's CheckIfAppIsRunning, which in silent mode only force-kills harness-app.exe and
;   would leave node.exe / harness-host.exe holding files under $INSTDIR.
;   The PowerShell below (one line, `$$` = literal `$` in NSIS):
;     1. every harness-app.exe whose image lives under $INSTDIR gets CloseMainWindow (WM_CLOSE), so
;        the shell's close barrier disposes the Host and the agent;
;     2. Wait-Process up to 15 s for those shells;
;     3. Stop-Process -Force whatever still runs from $INSTDIR, then Wait-Process up to 5 s.
;   Processes outside $INSTDIR are never touched. The directory is passed through the
;   HARNESS_STOP_DIR environment variable to avoid quoting a path inside the command.
;   修复原因：Tauri 会把 hooks 文件复制到 target/release/nsis/x64 再 !include，
;   ${__FILEDIR__} 指向复制后的目录，旁边的独立 .ps1 找不到，makensis 报 "no files found"；
;   因此把脚本内联为一行命令，不再依赖额外文件。
; POSTUNINSTALL: remove HKCU\Software\${MANUFACTURER}\${PRODUCTNAME} (install-location key) unless
;   this is an update; Tauri's template only removes it when "delete app data" is ticked, so a
;   silent uninstall would leave it behind.
; User data (~/.zcode or the custom data directory) is never touched.

!define HARNESS_STOP_PS `$$r=[IO.Path]::GetFullPath($$env:HARNESS_STOP_DIR).TrimEnd('\')+'\'; function O{@(Get-CimInstance Win32_Process|?{$$_.ExecutablePath -and $$_.ExecutablePath.StartsWith($$r,[StringComparison]::OrdinalIgnoreCase)})}; $$a=@(); foreach($$s in (O|?{$$_.Name -ieq 'harness-app.exe'})){$$p=Get-Process -Id $$s.ProcessId -EA 0; if($$p -and $$p.CloseMainWindow()){$$a+=$$p.Id}}; if($$a){Wait-Process -Id $$a -Timeout 15 -EA 0}; $$l=O; foreach($$x in $$l){Stop-Process -Id $$x.ProcessId -Force -EA 0}; if($$l){Wait-Process -Id ($$l|%{$$_.ProcessId}) -Timeout 5 -EA 0}; 'stop-harness: closed {0}, force-stopped {1} under {2}' -f $$a.Count,$$l.Count,$$r`

!macro HARNESS_STOP_RUNNING
  System::Call 'Kernel32::SetEnvironmentVariable(t "HARNESS_STOP_DIR", t "$INSTDIR")'
  nsExec::ExecToLog `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "${HARNESS_STOP_PS}"`
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro HARNESS_STOP_RUNNING
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro HARNESS_STOP_RUNNING
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegKey HKCU "Software\${MANUFACTURER}\${PRODUCTNAME}"
    DeleteRegKey /ifempty HKCU "Software\${MANUFACTURER}"
  ${EndIf}
!macroend
