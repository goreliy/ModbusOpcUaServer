# Собрать актуальные серверный/GUI EXE и самостоятельный EXE-установщик.
# Запуск из любой папки: powershell -ExecutionPolicy Bypass -File scripts/pack-win.ps1
param(
    [string]$Version = "",
    [switch]$SkipBuild,
    [switch]$Trial,
    [string]$OutputDirectory = ""
)

$ErrorActionPreference = 'Stop'
if (-not $Version) { $Version = if ($Trial) { '0.1.0-trial' } else { '0.1.0-full' } }
if ($Version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') { throw 'Invalid package version' }
if (($Version -match 'trial') -ne [bool]$Trial) { throw 'Package version and -Trial must agree' }
$repo = Split-Path -Parent $PSScriptRoot
$releaseDir = Join-Path $repo 'target\release'
$workDir = Join-Path $repo "target\package\OPC-Modbus-Server-$Version-win64"
$outputDir = Join-Path $repo 'dist\windows'
if ($OutputDirectory) {
    $outputDir = [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($OutputDirectory)) { $OutputDirectory } else { Join-Path $repo $OutputDirectory }))
    $allowedOutput = [IO.Path]::GetFullPath((Join-Path $repo 'dist'))
    if (-not $outputDir.StartsWith($allowedOutput + [IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)) { throw 'OutputDirectory must be inside workspace dist' }
    if ((Test-Path -LiteralPath $outputDir) -and ((Get-Item -LiteralPath $outputDir).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'OutputDirectory must not be a junction' }
}
$payloadZip = Join-Path (Split-Path -Parent $workDir) "installer-payload-$Version-win64.zip"
$installer = Join-Path $outputDir "OPC-Modbus-Server-Setup-$Version-win64.exe"
$serverOutput = Join-Path $outputDir 'opc-modbus-server.exe'
$configOutput = Join-Path $outputDir 'opc-modbus-config.exe'

function Read-BuildInfo([string]$Exe, [string]$Arguments) {
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $Exe
    $start.Arguments = $Arguments
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::Start($start)
    try {
        $stdout = $process.StandardOutput.ReadToEnd()
        $stderr = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) { throw "Build info failed: $Exe`: $stderr" }
        return ($stdout | ConvertFrom-Json)
    } finally { $process.Dispose() }
}

Push-Location $repo
try {
    if (-not $SkipBuild) {
        # Static CRT keeps the release self-contained on clean Windows hosts.
        $oldRustFlags = $env:RUSTFLAGS
        try {
            $env:RUSTFLAGS = '-C target-feature=+crt-static'
            $buildArgs = @('build', '--release', '--locked', '--offline', '--no-default-features', '-p', 'app', '-p', 'gui')
            if ($Trial) { $buildArgs += @('--features', 'app/trial,gui/trial') }
            & cargo @buildArgs
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

    $serverInfo = Read-BuildInfo $server 'build-info'
    $configInfo = Read-BuildInfo $config '--build-info'
    foreach ($info in @($serverInfo, $configInfo)) {
        if ($info.trial -ne [bool]$Trial -or $info.debug_assertions -or $info.arch -ne 'x86_64' -or $info.os -ne 'windows') {
            throw 'Binaries do not match the requested Windows x64 release build'
        }
        if (-not $Trial -and $null -ne $info.runtime_limit_seconds) { throw 'Full build has a runtime limit' }
    }

    # Validate the absolute cleanup target inside this workspace before recursion.
    $packageRoot = [System.IO.Path]::GetFullPath((Join-Path $repo 'target\package'))
    $workDir = [System.IO.Path]::GetFullPath($workDir)
    if (-not $workDir.StartsWith($packageRoot + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Unsafe package staging path: $workDir"
    }
    if ((Test-Path -LiteralPath $workDir) -and ((Get-Item -LiteralPath $workDir).Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw "Refusing to clean a package junction: $workDir"
    }
    if (Test-Path -LiteralPath $workDir) { Remove-Item -LiteralPath $workDir -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $workDir, $outputDir | Out-Null

    Copy-Item -LiteralPath $server -Destination $serverOutput -Force
    Copy-Item -LiteralPath $config -Destination $configOutput -Force
    Copy-Item -LiteralPath $server, $config,
        (Join-Path $repo 'config.example.json'),
        (Join-Path $repo 'README.md'),
        (Join-Path $repo 'LICENSE.md'),
        (Join-Path $repo 'installers\windows\install.cmd'),
        (Join-Path $repo 'installers\windows\install.ps1'),
        (Join-Path $repo 'installers\windows\repair-data-permissions.ps1'),
        (Join-Path $repo 'installers\windows\uninstall.ps1') -Destination $workDir -Force

    $manifest = [ordered]@{
        mcp_documentation = 'docs/MCP.md'
        package_version = $Version
        server = $serverInfo
        configurator = $configInfo
        sha256 = @{
            server = (Get-FileHash -Algorithm SHA256 -LiteralPath $serverOutput).Hash
            configurator = (Get-FileHash -Algorithm SHA256 -LiteralPath $configOutput).Hash
        }
    }
    $manifestPath = Join-Path $outputDir 'build-info.json'
    $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $manifestPath -Encoding UTF8
    Copy-Item -LiteralPath $manifestPath -Destination $workDir -Force
    New-Item -ItemType Directory -Force -Path (Join-Path $workDir 'docs') | Out-Null
    Copy-Item -LiteralPath (Join-Path $repo 'docs\MCP.md') -Destination (Join-Path $workDir 'docs\MCP.md') -Force

    # The payload ZIP is an implementation detail contained inside the installer.
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

    Get-FileHash -Algorithm SHA256 $serverOutput, $configOutput, $installer |
        Select-Object Path, Hash
}
finally {
    Pop-Location
}
