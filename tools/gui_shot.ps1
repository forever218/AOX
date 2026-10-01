# 截取 AOX GUI 窗口（开发期视觉验证）
# 用法: powershell -ExecutionPolicy Bypass -File tools/gui_shot.ps1 [-Out 路径] [-ProcName aox-gui]
#
# 注意：必须先把当前进程声明为 DPI aware，否则 GetWindowRect 返回的是被虚拟化
# （按缩放比例缩小）的坐标，而 CopyFromScreen 抓的是物理像素，截图会整体偏移。

param(
  [string]$Out = "E:\rustapp\gui\shot.png",
  [string]$ProcName = "aox-gui",
  [int]$W = 1240,
  [int]$H = 840
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class AoxWin {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int t, bool rp);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int t, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

[void][AoxWin]::SetProcessDPIAware()

$h = [IntPtr]::Zero
for ($i = 0; $i -lt 30; $i++) {
  $p = Get-Process -Name $ProcName -ErrorAction SilentlyContinue |
       Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
  if ($p) { $h = $p.MainWindowHandle; break }
  Start-Sleep -Milliseconds 300
}
if ($h -eq [IntPtr]::Zero) { "NOTFOUND: no visible window for $ProcName" | Set-Content ($Out + ".txt"); exit 2 }

# 固定位置，保证多次截图可比（$W/$H 传 0 表示保持原窗口尺寸，避免 resize）
if ($W -gt 0 -and $H -gt 0) { [void][AoxWin]::MoveWindow($h, 40, 40, $W, $H, $true) }
[void][AoxWin]::SetForegroundWindow($h)
# 强制置顶：否则窗口被其它程序盖住时，CopyFromScreen 抓到的是别人的画面
[void][AoxWin]::SetWindowPos($h, [IntPtr]::new(-1), 0, 0, 0, 0, 0x0003)  # HWND_TOPMOST, NOMOVE|NOSIZE
Start-Sleep -Milliseconds 1600
$r = New-Object 'AoxWin+RECT'
[void][AoxWin]::GetWindowRect($h, [ref]$r)
$w = $r.Right - $r.Left
$ht = $r.Bottom - $r.Top
if ($w -le 0 -or $ht -le 0) { "BADRECT $w x $ht" | Set-Content ($Out + ".txt"); exit 3 }

$bmp = New-Object System.Drawing.Bitmap $w, $ht
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
[void][AoxWin]::SetWindowPos($h, [IntPtr]::new(-2), 0, 0, 0, 0, 0x0003)  # 还原 HWND_NOTOPMOST
"OK ${w}x${ht} at $($r.Left),$($r.Top) -> $Out" | Set-Content ($Out + ".txt")
