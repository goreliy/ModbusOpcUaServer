$ErrorActionPreference = 'Stop'
$mcpTracePath = Join-Path $PSScriptRoot 'target\mcp-live-install.log'
Start-Transcript -LiteralPath $mcpTracePath -Force | Out-Null
try {
    $mcpOperatorSid = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'target\mcp-live-before.json') -Raw | ConvertFrom-Json).operator_sid
    & (Join-Path $PSScriptRoot 'target\package\OPC-Modbus-Server-0.1.0-full-win64\install.ps1') -InstallDir 'C:\Program Files\OPC Modbus Server' -DataDir 'C:\ProgramData\OPC Modbus Server' -OperatorSid $mcpOperatorSid
    exit 0
} catch {
    Write-Output $_.Exception.ToString()
    exit 1
} finally {
    Stop-Transcript | Out-Null
}
