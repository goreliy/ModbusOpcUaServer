# Установщик OPC Modbus Server (ПРОБНАЯ СБОРКА) для Windows.
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
    [string]$DataDir    = "$env:ProgramData\OPC Modbus Server"
)

$ErrorActionPreference = 'Stop'

# --- самоподъём прав администратора ---
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Требуются права администратора — перезапуск через UAC..."
    Start-Process powershell -Verb RunAs -ArgumentList `
        "-ExecutionPolicy Bypass -File `"$PSCommandPath`" -InstallDir `"$InstallDir`" -DataDir `"$DataDir`""
    exit
}

$src = $PSScriptRoot
$server = Join-Path $src 'opc-modbus-server.exe'
$config = Join-Path $src 'opc-modbus-config.exe'
if (-not (Test-Path $server)) { throw "Не найден $server рядом со скриптом" }

Write-Host "Установка в $InstallDir ..."
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
Copy-Item $server $InstallDir -Force
Copy-Item $config $InstallDir -Force

# Пример конфига — только если своего ещё нет (не затираем рабочий).
$cfgPath = Join-Path $DataDir 'config.json'
if (-not (Test-Path $cfgPath)) {
    Copy-Item (Join-Path $src 'config.example.json') $cfgPath -Force
    Write-Host "Создан стартовый конфиг: $cfgPath"
} else {
    Write-Host "Конфиг уже существует, оставлен как есть: $cfgPath"
}

# --- служба Windows ---
$serverExe = Join-Path $InstallDir 'opc-modbus-server.exe'
Write-Host "Регистрация службы..."
& $serverExe service install --config $cfgPath
Write-Host "Запуск службы..."
& sc.exe start opc-modbus-server | Out-Null

# --- ярлык на конфигуратор в меню Пуск ---
$startMenu = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs'
$lnk = Join-Path $startMenu 'Конфигуратор OPC Modbus Server.lnk'
$wsh = New-Object -ComObject WScript.Shell
$sc = $wsh.CreateShortcut($lnk)
$sc.TargetPath = Join-Path $InstallDir 'opc-modbus-config.exe'
$sc.WorkingDirectory = $InstallDir
$sc.Description = 'Конфигуратор OPC Modbus Server'
$sc.Save()

# --- деинсталлятор ---
Copy-Item (Join-Path $src 'uninstall.ps1') $InstallDir -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "Готово. Служба 'opc-modbus-server' установлена (автозапуск)."
Write-Host "ВНИМАНИЕ: это ПРОБНАЯ сборка — сервер работает непрерывно не более 3 часов,"
Write-Host "после чего останавливается; перезапустите службу для продолжения."
Write-Host "Конфиг: $cfgPath   |   Конфигуратор: меню Пуск."
