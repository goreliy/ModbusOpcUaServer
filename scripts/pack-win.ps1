# Собрать актуальный серверный EXE и самостоятельный EXE-установщик.
# Запуск из любой папки: powershell -ExecutionPolicy Bypass -File scripts/pack-win.ps1
param(
    [string]$Version = "0.1.0-trial",
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$releaseDir = Join-Path $repo 'target\release'
$workDir = Join-Path $repo "target\package\OPC-Modbus-Server-$Version-win64"
$outputDir = Join-Path $repo 'dist\windows'
$payloadZip = Join-Path (Split-Path -Parent $workDir) "installer-payload-$Version-win64.zip"
$installer = Join-Path $outputDir "OPC-Modbus-Server-Setup-$Version-win64.exe"
$serverOutput = Join-Path $outputDir 'opc-modbus-server.exe'

Push-Location $repo
try {
    if (-not $SkipBuild) {
        # Static CRT keeps the release self-contained on clean Windows hosts.
        $oldRustFlags = $env:RUSTFLAGS
        try {
            $env:RUSTFLAGS = '-C target-feature=+crt-static'
            & cargo build --release --locked --offline -p app -p gui
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed: $LASTEXITCODE" }
        }
        finally {
            $env:RUSTFLAGS = $oldRustFlags
        }
    }

    $server = Join-Path $releaseDir 'opc-modbus-server.exe'
    $config = Join-Path $releaseDir 'opc-modbus-config.exe'
    if (-not (Test-Path -LiteralPath $server)) { throw "Missing $server" }
    if (-not (Test-Path -LiteralPath $config)) { throw "Missing $config" }

    if (Test-Path -LiteralPath $workDir) { Remove-Item -LiteralPath $workDir -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $workDir, $outputDir | Out-Null

    Copy-Item -LiteralPath $server -Destination $serverOutput -Force
    Copy-Item -LiteralPath $server, $config,
        (Join-Path $repo 'config.example.json'),
        (Join-Path $repo 'README.md'),
        (Join-Path $repo 'LICENSE.md'),
        (Join-Path $repo 'installers\windows\install.cmd'),
        (Join-Path $repo 'installers\windows\install.ps1'),
        (Join-Path $repo 'installers\windows\uninstall.ps1') -Destination $workDir -Force

    # Remove obsolete public outputs from older packaging layouts. The GUI and
    # payload ZIP are implementation details contained inside the installer.
    Remove-Item -LiteralPath (Join-Path $outputDir 'opc-modbus-config.exe') -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $outputDir "OPC-Modbus-Server-$Version-win64.zip") -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $payloadZip) { Remove-Item -LiteralPath $payloadZip -Force }
    Compress-Archive -Path (Join-Path $workDir '*') -DestinationPath $payloadZip -CompressionLevel Optimal

    # Build a self-contained bootstrap EXE that embeds the exact ZIP above.
    # It extracts the payload to a unique temp directory, runs install.ps1,
    # waits for UAC/installation to finish, and removes the temporary files.
    $oldPayload = $env:OMS_INSTALLER_PAYLOAD
    $env:OMS_INSTALLER_PAYLOAD = $payloadZip
    if (Test-Path -LiteralPath $installer) { Remove-Item -LiteralPath $installer -Force }
    try {
        & rustc --edition=2021 -C opt-level=z -C lto=fat -C codegen-units=1 `
            -C panic=abort -C target-feature=+crt-static -C strip=symbols `
            -o $installer (Join-Path $repo 'installers\windows\bootstrap.rs')
        if ($LASTEXITCODE -ne 0) { throw "installer bootstrap build failed: $LASTEXITCODE" }
    }
    finally {
        $env:OMS_INSTALLER_PAYLOAD = $oldPayload
        Remove-Item -LiteralPath $payloadZip -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath ([System.IO.Path]::ChangeExtension($installer, '.pdb')) -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path -LiteralPath $installer)) { throw "Installer was not created: $installer" }

    Get-FileHash -Algorithm SHA256 $serverOutput, $installer |
        Select-Object Path, Hash
}
finally {
    Pop-Location
}
