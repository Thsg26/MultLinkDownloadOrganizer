# Não usar parâmetros avançados: o separador literal `--` do Cargo deve ser
# encaminhado intacto (por exemplo: clippy --all-targets -- -D warnings).
$Install = $args.Count -gt 0 -and $args[0] -eq '-Install'
$CargoArgs = @($args)
if ($Install) {
    if ($args.Count -gt 1) { $CargoArgs = @($args[1..($args.Count - 1)]) }
    else { $CargoArgs = @() }
}
$ErrorActionPreference = 'Stop'
$root = Join-Path $env:LOCALAPPDATA 'Temp\opencode\mldo-rust'
$env:CARGO_HOME = Join-Path $root 'cargo'
$env:RUSTUP_HOME = Join-Path $root 'rustup'
$env:Path = "$env:CARGO_HOME\bin;$env:Path"
if ($Install -and !(Test-Path "$env:CARGO_HOME\bin\rustup.exe")) {
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $installer = Join-Path $root 'rustup-init.exe'
    Invoke-WebRequest 'https://win.rustup.rs/x86_64' -OutFile $installer -UseBasicParsing
    & $installer -y --no-modify-path --profile minimal --default-toolchain stable
    if ($LASTEXITCODE -ne 0) { throw 'Não foi possível instalar a toolchain Rust isolada.' }
}
if (!(Test-Path "$env:CARGO_HOME\bin\cargo.exe")) {
    throw 'Toolchain isolada ausente. Execute scripts\rust-env.ps1 -Install ou use seu próprio cargo.'
}
if ($CargoArgs.Count -gt 0) {
    Push-Location (Split-Path $PSScriptRoot -Parent)
    try {
        & "$env:CARGO_HOME\bin\cargo.exe" @CargoArgs
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    } finally { Pop-Location }
} else {
    & "$env:CARGO_HOME\bin\rustc.exe" --version
    & "$env:CARGO_HOME\bin\cargo.exe" --version
}
