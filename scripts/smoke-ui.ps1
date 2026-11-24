param(
    [string]$Executable = (Join-Path (Split-Path $PSScriptRoot -Parent) 'dist\MLDO.exe'),
    [string]$Screenshot = (Join-Path $env:LOCALAPPDATA 'Temp\opencode\mldo-ui.png'),
    [int]$ExistingProcessId = 0,
    [switch]$SkipInputIdle
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class MldoWindow {
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out Rect r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] public static extern bool RedrawWindow(IntPtr h, IntPtr rect, IntPtr region, uint flags);
    [DllImport("dwmapi.dll")] public static extern int DwmFlush();
}
'@
[MldoWindow]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null
$process = if ($ExistingProcessId -gt 0) { Get-Process -Id $ExistingProcessId } else { Start-Process -FilePath $Executable -PassThru }
try {
    if (!$SkipInputIdle -and !$process.WaitForInputIdle(15000)) { throw 'A janela não ficou disponível em 15 segundos.' }
    # Winit cria uma janela auxiliar antes da janela gráfica real. Aguardar a
    # janela com título evita capturar/fechar essa janela auxiliar de 12 pixels.
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $process.Refresh()
        if ($process.HasExited) { throw 'O aplicativo encerrou durante a inicialização.' }
        if ($process.MainWindowTitle.StartsWith('MLDO')) { break }
        [System.Threading.Thread]::Sleep(50)
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($process.HasExited) { throw 'O aplicativo encerrou antes da validação visual.' }
    if (!$process.MainWindowTitle.StartsWith('MLDO')) { throw 'A janela principal do MLDO não foi apresentada.' }
    $handle = $process.MainWindowHandle
    if ($handle -eq [IntPtr]::Zero) { throw 'Janela principal não encontrada.' }
    [MldoWindow]::RedrawWindow($handle, [IntPtr]::Zero, [IntPtr]::Zero, 0x181) | Out-Null
    [MldoWindow]::DwmFlush() | Out-Null
    $rect = New-Object MldoWindow+Rect
    if (![MldoWindow]::GetWindowRect($handle, [ref]$rect)) { throw 'Não foi possível obter o tamanho da janela.' }
    $bitmap = New-Object System.Drawing.Bitmap(($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top))
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $dc = $graphics.GetHdc()
    try {
        if (![MldoWindow]::PrintWindow($handle, $dc, 2)) { throw 'Não foi possível capturar somente a janela do MLDO.' }
    } finally { $graphics.ReleaseHdc($dc) }
    try { $bitmap.Save($Screenshot, [System.Drawing.Imaging.ImageFormat]::Png) }
    finally { $graphics.Dispose(); $bitmap.Dispose() }
    Write-Host "Janela aberta com sucesso: $($process.MainWindowTitle)"
    Write-Host "Captura exclusiva do aplicativo: $Screenshot"
} finally {
    if (!$process.HasExited) {
        $process.CloseMainWindow() | Out-Null
        if (!$process.WaitForExit(10000)) { throw 'A janela de teste não encerrou normalmente.' }
    }
}
