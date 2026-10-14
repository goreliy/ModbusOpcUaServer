# Установщик OPC Modbus Server для Windows (полная или пробная сборка).
#
# Копирует бинарники в Program Files, кладёт пример конфига в ProgramData,
# регистрирует и запускает службу, создаёт ярлык на конфигуратор в меню Пуск
# и деинсталлятор. Запускать из PowerShell от имени администратора
# (скрипт сам поднимет UAC при необходимости).
#
# Рядом должны лежать: opc-modbus-server.exe, opc-modbus-config.exe,
# config.example.json.

param(
    [string]$InstallDir = "$env:ProgramFiles\OPC Modbus Server",
    [string]$DataDir    = "$env:ProgramData\OPC Modbus Server",
    [string]$OperatorSid = ""
)

$ErrorActionPreference = 'Stop'
if (-not $OperatorSid) { $OperatorSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value }

# --- самоподъём прав администратора ---
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Требуются права администратора — перезапуск через UAC..."
    $elevated = Start-Process powershell -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList `
        "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -InstallDir `"$InstallDir`" -DataDir `"$DataDir`" -OperatorSid `"$OperatorSid`""
    exit $elevated.ExitCode
}

$src = $PSScriptRoot
$server = Join-Path $src 'opc-modbus-server.exe'
$config = Join-Path $src 'opc-modbus-config.exe'
foreach ($required in @($server, $config, (Join-Path $src 'repair-data-permissions.ps1'))) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "Не найден обязательный файл $required рядом со скриптом" }
}
$cfgPath = Join-Path $DataDir 'config.json'
if (-not (Test-Path -LiteralPath $cfgPath) -and -not (Test-Path -LiteralPath (Join-Path $src 'config.example.json') -PathType Leaf)) {
    throw 'Не найден config.example.json для новой установки'
}

Write-Host "Установка в $InstallDir ..."
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
$upgradeId = [Guid]::NewGuid().ToString('N')
$binaryUpdates = @(
    @{ Source = $server; Target = (Join-Path $InstallDir 'opc-modbus-server.exe'); Stage = (Join-Path $InstallDir ".upgrade-$upgradeId-server.new"); Backup = (Join-Path $InstallDir ".upgrade-$upgradeId-server.old") },
    @{ Source = $config; Target = (Join-Path $InstallDir 'opc-modbus-config.exe'); Stage = (Join-Path $InstallDir ".upgrade-$upgradeId-config.new"); Backup = (Join-Path $InstallDir ".upgrade-$upgradeId-config.old") }
)
# Stage and back up both binaries while the old service still runs. An invalid
# source package must not stop a working installation.
foreach ($update in $binaryUpdates) {
    Copy-Item -LiteralPath $update.Source -Destination $update.Stage -Force
    $update.HadOriginal = Test-Path -LiteralPath $update.Target -PathType Leaf
    if ($update.HadOriginal) { Copy-Item -LiteralPath $update.Target -Destination $update.Backup -Force }
}
$serviceName = 'opc-modbus-server'
$existingService = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
$priorService = if ($existingService) { Get-CimInstance -ClassName Win32_Service -Filter "Name='$serviceName'" -ErrorAction Stop } else { $null }
$priorServicePath = if ($priorService) { $priorService.PathName } else { $null }
$wasRunning = $existingService -and $existingService.Status -ne [ServiceProcess.ServiceControllerStatus]::Stopped
$registrationAttempted = $false
try {
    if ($existingService) {
        Write-Host 'Остановка службы перед обновлением файлов...'
        if ($existingService.Status -ne [ServiceProcess.ServiceControllerStatus]::Stopped) {
            Stop-Service -Name $serviceName -ErrorAction Stop
            $existingService.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(30))
        }
    }
    foreach ($update in $binaryUpdates) {
        $update.Replaced = $true
        Copy-Item -LiteralPath $update.Stage -Destination $update.Target -Force
    }
    if (Test-Path -LiteralPath (Join-Path $src 'docs\MCP.md')) {
        New-Item -ItemType Directory -Force -Path (Join-Path $InstallDir 'docs') | Out-Null
        Copy-Item -LiteralPath (Join-Path $src 'docs\MCP.md') -Destination (Join-Path $InstallDir 'docs\MCP.md') -Force
    }

    # Never replace a pre-existing equipment configuration.
    if (-not (Test-Path -LiteralPath $cfgPath)) {
        Copy-Item -LiteralPath (Join-Path $src 'config.example.json') -Destination $cfgPath -Force
        Write-Host "Создан стартовый конфиг: $cfgPath"
    } else {
        Write-Host "Конфиг уже существует, оставлен как есть: $cfgPath"
    }

    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $src 'repair-data-permissions.ps1') -DataDir $DataDir -OperatorSid $OperatorSid
    if ($LASTEXITCODE -ne 0) { throw 'Cannot grant the selected operator access to application data' }
    Copy-Item -LiteralPath (Join-Path $src 'repair-data-permissions.ps1') -Destination $InstallDir -Force
    $serverExe = Join-Path $InstallDir 'opc-modbus-server.exe'
    Write-Host "Регистрация службы..."
    # The native CLI creates or updates the SCM entry, including paths with
    # spaces, without PowerShell 5 reparsing a nested sc.exe binPath argument.
    $registrationAttempted = $true
    & $serverExe service install --config $cfgPath
    if ($LASTEXITCODE -ne 0) { throw "Не удалось зарегистрировать или обновить службу: код $LASTEXITCODE" }
    Write-Host "Запуск службы..."
    Start-Service -Name $serviceName -ErrorAction Stop
    $startedService = Get-Service -Name $serviceName -ErrorAction Stop
    $startedService.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(30))

    $startMenu = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs'
    $lnk = Join-Path $startMenu 'Конфигуратор OPC Modbus Server.lnk'
    $wsh = New-Object -ComObject WScript.Shell
    $sc = $wsh.CreateShortcut($lnk)
    $sc.TargetPath = Join-Path $InstallDir 'opc-modbus-config.exe'
    $sc.Arguments = '--config "' + $cfgPath + '"'
    $sc.WorkingDirectory = $InstallDir
    $sc.Description = 'Конфигуратор OPC Modbus Server'
    $sc.Save()

    Copy-Item -LiteralPath (Join-Path $src 'uninstall.ps1') -Destination $InstallDir -Force -ErrorAction SilentlyContinue
    $buildInfoText = & $serverExe build-info
    if ($LASTEXITCODE -ne 0) { throw "Не удалось проверить установленную сборку: код $LASTEXITCODE" }
    $buildInfo = $buildInfoText | ConvertFrom-Json
} catch {
    $installError = $_.Exception.Message
    try {
        $rollbackService = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
        if ($rollbackService -and $rollbackService.Status -ne [ServiceProcess.ServiceControllerStatus]::Stopped) {
            Stop-Service -Name $serviceName -ErrorAction Stop
            $rollbackService.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(30))
        }
        if ($registrationAttempted) {
            if ($priorServicePath) {
                $restored = Invoke-CimMethod -InputObject $priorService -MethodName Change -Arguments @{ PathName = $priorServicePath } -ErrorAction Stop
                if ($restored.ReturnValue -ne 0) { throw "Не удалось восстановить прежнюю команду службы: код $($restored.ReturnValue)" }
            } elseif ($rollbackService) {
                & $serverExe service uninstall
                if ($LASTEXITCODE -ne 0) { throw "Не удалось удалить незавершённую новую службу: код $LASTEXITCODE" }
            }
        }
        foreach ($update in $binaryUpdates) {
            if ($update.Replaced) {
                if ($update.HadOriginal) {
                    $originalHash = (Get-FileHash -LiteralPath $update.Backup -Algorithm SHA256).Hash
                    $currentHash = if (Test-Path -LiteralPath $update.Target -PathType Leaf) { (Get-FileHash -LiteralPath $update.Target -Algorithm SHA256).Hash } else { '' }
                    if ($originalHash -ne $currentHash) { Copy-Item -LiteralPath $update.Backup -Destination $update.Target -Force }
                }
                else { Remove-Item -LiteralPath $update.Target -Force }
            }
        }
        if ($wasRunning) {
            Start-Service -Name $serviceName -ErrorAction Stop
            $existingService.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(30))
        }
    } catch {
        throw "Обновление не завершено, автоматическое восстановление также не удалось: $($_.Exception.Message). Резервные копии оставлены в $InstallDir (.upgrade-$upgradeId-*.old). Исходная ошибка: $installError"
    }
    foreach ($update in $binaryUpdates) {
        Remove-Item -LiteralPath $update.Stage, $update.Backup -Force -ErrorAction SilentlyContinue
    }
    throw "Установка не завершена. Предыдущие бинарники и команда службы восстановлены; рабочий конфиг сохранён. Ошибка: $installError"
}
foreach ($update in $binaryUpdates) {
    Remove-Item -LiteralPath $update.Stage, $update.Backup -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "Готово. Служба 'opc-modbus-server' установлена (автозапуск)."
if ($buildInfo.trial) {
    Write-Host "Пробная сборка: лимит непрерывной работы $($buildInfo.runtime_limit_seconds) секунд."
} else {
    Write-Host "Полная сборка: ограничения времени работы нет."
}
Write-Host "Конфиг: $cfgPath   |   Конфигуратор: меню Пуск."
