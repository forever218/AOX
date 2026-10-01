# 向 AOX GUI 窗口发送按键（开发期自动化验证用）
# 用法: powershell -ExecutionPolicy Bypass -File tools/gui_key.ps1 -Keys "y"
param(
  [string]$ProcName = "aox-gui",
  [string]$Keys = "y"
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class AoxFocus {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
}
"@

[void][AoxFocus]::SetProcessDPIAware()
$h = [IntPtr]::Zero
for ($i = 0; $i -lt 30; $i++) {
  $p = Get-Process -Name $ProcName -ErrorAction SilentlyContinue |
       Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
  if ($p) { $h = $p.MainWindowHandle; break }
  Start-Sleep -Milliseconds 300
}
if ($h -eq [IntPtr]::Zero) { "NOHWND" | Set-Content E:\rustapp\gui\_key.txt; exit 2 }

[void][AoxFocus]::ShowWindow($h, 9)
[void][AoxFocus]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 600
[System.Windows.Forms.SendKeys]::SendWait($Keys)
"sent [$Keys]" | Set-Content E:\rustapp\gui\_key.txt
