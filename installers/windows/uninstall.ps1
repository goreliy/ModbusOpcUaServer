# Деинсталлятор OPC Modbus Server. Останавливает и удаляет службу, стирает
# файлы программы и ярлык. Данные (ProgramData\OPC Modbus Server) НЕ удаляются
# по умолчанию — уберите вручную, если нужно.

param(
    [string]$InstallDir = "$env:ProgramFiles\OPC Modbus Server"
)
$ErrorActionPreference = 'Continue'

$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if (-not $isAdmin) {
    Start-Process powershell -Verb RunAs -ArgumentList `
        "-ExecutionPolicy Bypass -File `"$PSCommandPath`" -InstallDir `"$InstallDir`""
    exit
}

$serverExe = Join-Path $InstallDir 'opc-modbus-server.exe'
if (Test-Path $serverExe) {
    Write-Host "Останов и удаление службы..."
    & sc.exe stop opc-modbus-server | Out-Null
    Start-Sleep -Seconds 1
    & $serverExe service uninstall
}

$lnk = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Конфигуратор OPC Modbus Server.lnk'
Remove-Item $lnk -ErrorAction SilentlyContinue

Write-Host "Удаление файлов программы..."
Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "Готово. Данные в '$env:ProgramData\OPC Modbus Server' оставлены."
