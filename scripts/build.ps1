$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    if (Get-Command cargo -ErrorAction SilentlyContinue) {
        & cargo build --release --locked
    } else {
        & "$PSScriptRoot\rust-env.ps1" build --release --locked
    }
    if ($LASTEXITCODE -ne 0) { throw 'A compilação falhou.' }
    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    Copy-Item (Join-Path $root 'target\release\mldo.exe') (Join-Path $dist 'MLDO.exe') -Force
    Copy-Item (Join-Path $root 'README.md') (Join-Path $dist 'README.md') -Force
    Copy-Item (Join-Path $root 'THIRD_PARTY.md') (Join-Path $dist 'THIRD_PARTY.md') -Force
    $licenses = Join-Path $dist 'licenses'
    New-Item -ItemType Directory -Force -Path $licenses | Out-Null
    Copy-Item (Join-Path $root 'licenses\*') $licenses -Force
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    foreach ($entry in @(
        @{ Pattern = 'registry\src\*\wry-0.57.*\LICENSE-MIT'; Name = 'Wry-MIT.txt' },
        @{ Pattern = 'registry\src\*\tao-0.37.*\LICENSE'; Name = 'Tao-APACHE.txt' }
    )) {
        $source = Get-ChildItem (Join-Path $cargoHome $entry.Pattern) -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($source) { Copy-Item $source.FullName (Join-Path $licenses $entry.Name) -Force }
        else { Write-Warning "Licenca da dependencia nao encontrada: $($entry.Name)" }
    }
    Write-Host "Executável gerado em $dist\MLDO.exe"
} finally { Pop-Location }
