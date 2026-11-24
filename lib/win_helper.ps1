param(
    [Parameter(Mandatory = $true)][string]$Command,
    [string]$ArgsJson = "{}"
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

if (-not ("SmgWin32" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class SmgWin32 {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hWnd, int X, int Y, int nWidth, int nHeight, bool bRepaint);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, int dwData, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
}
[SmgWin32]::SetProcessDPIAware() | Out-Null

$ArgsObj = @{}
try {
    $parsed = ConvertFrom-Json $ArgsJson
    if ($parsed) {
        $parsed.PSObject.Properties | ForEach-Object { $ArgsObj[$_.Name] = $_.Value }
    }
} catch {}

function Get-AllWindows {
    $list = New-Object System.Collections.Generic.List[object]
    $fg = [SmgWin32]::GetForegroundWindow()
    $cb = [SmgWin32+EnumWindowsProc]{
        param($hWnd, $lParam)
        if (-not [SmgWin32]::IsWindowVisible($hWnd)) { return $true }
        $len = [SmgWin32]::GetWindowTextLength($hWnd)
        if ($len -le 0) { return $true }
        $sb = New-Object System.Text.StringBuilder ($len + 1)
        [SmgWin32]::GetWindowText($hWnd, $sb, $sb.Capacity) | Out-Null
        $title = $sb.ToString().Trim()
        if ([string]::IsNullOrWhiteSpace($title)) { return $true }
        $rect = New-Object SmgWin32+RECT
        [SmgWin32]::GetWindowRect($hWnd, [ref]$rect) | Out-Null
        $w = $rect.Right - $rect.Left
        $h = $rect.Bottom - $rect.Top
        if ($w -le 1 -or $h -le 1) { return $true }
        [uint32]$winPid = 0
        [SmgWin32]::GetWindowThreadProcessId($hWnd, [ref]$winPid) | Out-Null
        $procName = ""
        try { $procName = (Get-Process -Id $winPid -ErrorAction SilentlyContinue).ProcessName } catch {}
        $list.Add([ordered]@{
            id = [int64]$hWnd.ToInt64()
            window_id = [int64]$hWnd.ToInt64()
            pid = [int]$winPid
            app_id = $procName
            wm_class = $procName
            title = $title
            focused = ($hWnd -eq $fg)
            bounds = [ordered]@{ x = $rect.Left; y = $rect.Top; width = $w; height = $h }
        })
        return $true
    }
    [SmgWin32]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    return $list
}

function Resolve-TargetHwnd($a) {
    $wins = Get-AllWindows
    if ($a.ContainsKey("window_id") -and $null -ne $a["window_id"]) {
        $wid = [int64]$a["window_id"]
        $m = $wins | Where-Object { $_.id -eq $wid } | Select-Object -First 1
        if ($m) { return $m }
    }
    $q = ""
    if ($a.ContainsKey("app_id") -and $a["app_id"]) { $q = [string]$a["app_id"] }
    elseif ($a.ContainsKey("title") -and $a["title"]) { $q = [string]$a["title"] }
    if ($q) {
        $ql = $q.ToLowerInvariant()
        $m = $wins | Where-Object {
            ($_.app_id -and $_.app_id.ToLowerInvariant().Contains($ql)) -or
            ($_.title -and $_.title.ToLowerInvariant().Contains($ql))
        } | Select-Object -First 1
        if ($m) { return $m }
    }
    return $null
}

switch ($Command) {
    "list_windows" {
        $wins = Get-AllWindows
        @{ ok = $true; backend = "win32-uia"; window_count = $wins.Count; windows = $wins } | ConvertTo-Json -Depth 6 -Compress
    }
    "focused_window" {
        $wins = Get-AllWindows
        $fg = $wins | Where-Object { $_.focused } | Select-Object -First 1
        @{ ok = ($null -ne $fg); backend = "win32-uia"; focused_window = $fg } | ConvertTo-Json -Depth 6 -Compress
    }
    "list_apps" {
        $apps = Get-Process | Where-Object { $_.MainWindowTitle } | Select-Object -Unique -ExpandProperty ProcessName
        @{ ok = $true; backend = "win32-uia"; apps = @($apps) } | ConvertTo-Json -Depth 4 -Compress
    }
    "get_app_state" {
        $wins = Get-AllWindows
        $q = ([string]($ArgsObj["app_query"] ?? $ArgsObj["app_id"] ?? "")).ToLowerInvariant()
        $matched = if ($q) { @($wins | Where-Object { $_.app_id.ToLowerInvariant().Contains($q) -or $_.title.ToLowerInvariant().Contains($q) }) } else { $wins }
        @{ ok = $true; backend = "win32-uia"; windows = $matched } | ConvertTo-Json -Depth 6 -Compress
    }
    "activate_window" {
        $m = Resolve-TargetHwnd $ArgsObj
        if (-not $m) { @{ ok = $false; error = "Window not found" } | ConvertTo-Json -Compress; exit 0 }
        $hWnd = [IntPtr][int64]$m.id
        [SmgWin32]::ShowWindow($hWnd, 9) | Out-Null
        [SmgWin32]::SetForegroundWindow($hWnd) | Out-Null
        @{ ok = $true; window_id = $m.id; title = $m.title } | ConvertTo-Json -Compress
    }
    "move_window" {
        $m = Resolve-TargetHwnd $ArgsObj
        if (-not $m) { @{ ok = $false; error = "Window not found" } | ConvertTo-Json -Compress; exit 0 }
        $hWnd = [IntPtr][int64]$m.id
        $x = if ($ArgsObj.ContainsKey("x")) { [int]$ArgsObj["x"] } else { $m.bounds.x }
        $y = if ($ArgsObj.ContainsKey("y")) { [int]$ArgsObj["y"] } else { $m.bounds.y }
        $w = if ($ArgsObj.ContainsKey("width")) { [int]$ArgsObj["width"] } else { $m.bounds.width }
        $h = if ($ArgsObj.ContainsKey("height")) { [int]$ArgsObj["height"] } else { $m.bounds.height }
        [SmgWin32]::MoveWindow($hWnd, $x, $y, $w, $h, $true) | Out-Null
        @{ ok = $true; window_id = $m.id; bounds = @{ x = $x; y = $y; width = $w; height = $h } } | ConvertTo-Json -Depth 4 -Compress
    }
    "resize_window" {
        $m = Resolve-TargetHwnd $ArgsObj
        if (-not $m) { @{ ok = $false; error = "Window not found" } | ConvertTo-Json -Compress; exit 0 }
        $hWnd = [IntPtr][int64]$m.id
        $w = [int]($ArgsObj["width"] ?? $m.bounds.width)
        $h = [int]($ArgsObj["height"] ?? $m.bounds.height)
        [SmgWin32]::MoveWindow($hWnd, $m.bounds.x, $m.bounds.y, $w, $h, $true) | Out-Null
        @{ ok = $true; window_id = $m.id; bounds = @{ x = $m.bounds.x; y = $m.bounds.y; width = $w; height = $h } } | ConvertTo-Json -Depth 4 -Compress
    }
    "screenshot" {
        $m = Resolve-TargetHwnd $ArgsObj
        if ($m) {
            $x = [int]$m.bounds.x; $y = [int]$m.bounds.y; $w = [int]$m.bounds.width; $h = [int]$m.bounds.height
        } else {
            $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
            $x = $vs.Left; $y = $vs.Top; $w = $vs.Width; $h = $vs.Height
        }
        $bmp = New-Object System.Drawing.Bitmap $w, $h
        $gfx = [System.Drawing.Graphics]::FromImage($bmp)
        $gfx.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size $w, $h))
        $ms = New-Object System.IO.MemoryStream
        $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
        $b64 = [Convert]::ToBase64String($ms.ToArray())
        $gfx.Dispose(); $bmp.Dispose(); $ms.Dispose()
        @{ ok = $true; format = "png"; width = $w; height = $h; data_base64 = $b64 } | ConvertTo-Json -Compress
    }
    "click" {
        $x = [int]($ArgsObj["x"] ?? 0)
        $y = [int]($ArgsObj["y"] ?? 0)
        $btn = ([string]($ArgsObj["button"] ?? "left")).ToLowerInvariant()
        [SmgWin32]::SetCursorPos($x, $y) | Out-Null
        $down = if ($btn -eq "right") { 0x0008 } elseif ($btn -eq "middle") { 0x0020 } else { 0x0002 }
        $up = if ($btn -eq "right") { 0x0010 } elseif ($btn -eq "middle") { 0x0040 } else { 0x0004 }
        $clicks = if ($ArgsObj["double"] -or $ArgsObj["click_count"] -eq 2) { 2 } else { 1 }
        for ($i = 0; $i -lt $clicks; $i++) {
            [SmgWin32]::mouse_event($down, 0, 0, 0, [UIntPtr]::Zero)
            [SmgWin32]::mouse_event($up, 0, 0, 0, [UIntPtr]::Zero)
        }
        @{ ok = $true; x = $x; y = $y; button = $btn } | ConvertTo-Json -Compress
    }
    "type_text" {
        $txt = [string]($ArgsObj["text"] ?? "")
        $escaped = [System.Text.RegularExpressions.Regex]::Replace($txt, '([+^%~(){}\[\]])', '{$1}')
        [System.Windows.Forms.SendKeys]::SendWait($escaped)
        @{ ok = $true; typed_chars = $txt.Length } | ConvertTo-Json -Compress
    }
    "press_key" {
        $key = [string]($ArgsObj["key"] ?? $ArgsObj["keys"] ?? "")
        [System.Windows.Forms.SendKeys]::SendWait($key)
        @{ ok = $true; key = $key } | ConvertTo-Json -Compress
    }
    "scroll" {
        $delta = [int]($ArgsObj["delta_y"] ?? $ArgsObj["amount"] ?? -120)
        if ($ArgsObj.ContainsKey("x") -and $ArgsObj.ContainsKey("y")) {
            [SmgWin32]::SetCursorPos([int]$ArgsObj["x"], [int]$ArgsObj["y"]) | Out-Null
        }
        [SmgWin32]::mouse_event(0x0800, 0, 0, $delta, [UIntPtr]::Zero)
        @{ ok = $true; delta = $delta } | ConvertTo-Json -Compress
    }
    "drag" {
        $x1 = [int]($ArgsObj["start_x"] ?? $ArgsObj["from_x"] ?? 0)
        $y1 = [int]($ArgsObj["start_y"] ?? $ArgsObj["from_y"] ?? 0)
        $x2 = [int]($ArgsObj["end_x"] ?? $ArgsObj["to_x"] ?? 0)
        $y2 = [int]($ArgsObj["end_y"] ?? $ArgsObj["to_y"] ?? 0)
        [SmgWin32]::SetCursorPos($x1, $y1) | Out-Null
        [SmgWin32]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 60
        [SmgWin32]::SetCursorPos($x2, $y2) | Out-Null
        Start-Sleep -Milliseconds 60
        [SmgWin32]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
        @{ ok = $true } | ConvertTo-Json -Compress
    }
    "launch_app" {
        $app = [string]($ArgsObj["app"] ?? $ArgsObj["command"] ?? "")
        if (-not $app) { @{ ok = $false; error = "Missing app parameter" } | ConvertTo-Json -Compress; exit 0 }
        $proc = Start-Process -FilePath $app -PassThru
        Start-Sleep -Milliseconds 700
        @{ ok = $true; app = $app; pid = $proc.Id } | ConvertTo-Json -Compress
    }
    "walk_app_tree" {
        $q = ([string]($ArgsObj["app_query"] ?? "")).ToLowerInvariant()
        $maxNodes = [int]($ArgsObj["max_nodes"] ?? 400)
        $maxDepth = [int]($ArgsObj["max_depth"] ?? 28)
        $rootEl = [System.Windows.Automation.AutomationElement]::RootElement
        $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
        $nodes = New-Object System.Collections.Generic.List[object]
        $wins = Get-AllWindows
        $targetWin = if ($q) { $wins | Where-Object { $_.app_id.ToLowerInvariant().Contains($q) -or $_.title.ToLowerInvariant().Contains($q) } | Select-Object -First 1 } else { $wins | Where-Object { $_.focused } | Select-Object -First 1 }
        if ($targetWin) {
            $startEl = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr][int64]$targetWin.id)
            $queue = New-Object System.Collections.Queue
            $queue.Enqueue(@{ el = $startEl; path = "0"; depth = 0 })
            while ($queue.Count -gt 0 -and $nodes.Count -lt $maxNodes) {
                $item = $queue.Dequeue()
                $el = $item.el
                if ($item.depth -gt $maxDepth) { continue }
                $c = $el.Current
                $r = $c.BoundingRectangle
                $nodes.Add([ordered]@{
                    node_path = $item.path
                    name = $c.Name
                    role = $c.ControlType.ProgrammaticName
                    bounds = @{ x = [int]$r.X; y = [int]$r.Y; width = [int]$r.Width; height = [int]$r.Height }
                })
                $child = $walker.GetFirstChild($el)
                $idx = 0
                while ($null -ne $child -and $nodes.Count + $queue.Count -lt $maxNodes) {
                    $queue.Enqueue(@{ el = $child; path = "$($item.path)/$idx"; depth = $item.depth + 1 })
                    $child = $walker.GetNextSibling($child)
                    $idx++
                }
            }
        }
        @{ ok = $true; backend = "win32-uia"; node_count = $nodes.Count; nodes = $nodes } | ConvertTo-Json -Depth 6 -Compress
    }
    default {
        @{ ok = $false; error = "Unsupported command on Windows: $Command" } | ConvertTo-Json -Compress
    }
}
