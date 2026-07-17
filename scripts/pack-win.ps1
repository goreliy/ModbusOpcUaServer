# Пересобрать портируемый Windows-дистрибутив (ZIP) из release-бинарников.
# Запуск из корня workspace:  powershell -File scripts/pack-win.ps1
$ErrorActionPreference = 'Stop'
$ver = "0.1.0-trial"
$stage = "target\dist\OPC-Modbus-Server-$ver-win64"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item "target\release\opc-modbus-server.exe" $stage
Copy-Item "target\release\opc-modbus-config.exe" $stage
Copy-Item "config.example.json" $stage
Copy-Item "README.md" $stage
Copy-Item "LICENSE.md" $stage
Copy-Item "installers\windows\install.ps1" $stage
Copy-Item "installers\windows\uninstall.ps1" $stage
$zip = "target\dist\OPC-Modbus-Server-$ver-win64.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path "$stage\*" -DestinationPath $zip
Write-Output "Собрано: $zip"
