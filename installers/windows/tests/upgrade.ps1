# Exercises the real installer control flow with in-memory files and a fake SCM.
# No installation files, ACLs, shortcuts, or real services are changed.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.ServiceProcess
$installerPath = Join-Path $PSScriptRoot '..\install.ps1'
$source = Get-Content -LiteralPath $installerPath -Raw -Encoding UTF8
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw $parseErrors[0] }
$elevation = $ast.Find({ param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text -eq '-not $isAdmin' }, $true)
if (-not $elevation) { throw 'Installer elevation block was not found' }
$source = $source.Remove($elevation.Extent.StartOffset, $elevation.Extent.EndOffset - $elevation.Extent.StartOffset)
$source = $source.Replace('$src = $PSScriptRoot', '$src = ''C:\package''')
$source = $source.Replace('& powershell ', '& Invoke-FakePermissions ')
$source = $source.Replace('& $serverExe ', '& Invoke-FakeServer ')
$installer = [ScriptBlock]::Create($source)

function Assert-True($condition, [string]$message) { if (-not $condition) { throw $message } }
function Write-Host { param([Parameter(ValueFromRemainingArguments=$true)]$Arguments) }
function Test-Path { param($LiteralPath, $Path, $PathType) $script:files.ContainsKey([string]$(if ($LiteralPath) { $LiteralPath } else { $Path })) }
function New-Item { param($ItemType, $Path, [switch]$Force) $script:files[[string]$Path] = '<directory>' }
function Copy-Item {
    param($LiteralPath, $Path, $Destination, [switch]$Force, $ErrorAction)
    $from = [string]$(if ($LiteralPath) { $LiteralPath } else { $Path })
    $to = [string]$Destination
    if ($script:files[$to] -eq '<directory>') { $to = Join-Path $to ([IO.Path]::GetFileName($from)) }
    $script:events.Add("copy:$from->$to")
    if ($script:failConfigCopy -and $from.EndsWith('-config.new') -and $to.EndsWith('opc-modbus-config.exe')) { throw 'GUI executable is in use' }
    if (-not $script:files.ContainsKey($from)) {
        if ($ErrorAction -eq 'SilentlyContinue') { return }
        throw "Missing fake source: $from"
    }
    $script:files[$to] = $script:files[$from]
}
function Remove-Item { param([string[]]$LiteralPath, [switch]$Force, $ErrorAction) foreach ($path in $LiteralPath) { $script:files.Remove($path) } }
function Get-FileHash { param($LiteralPath, $Algorithm) [pscustomobject]@{ Hash = $script:files[[string]$LiteralPath] } }
function Get-Service { param($Name, $ErrorAction) $script:fakeService }
function Get-CimInstance { param($ClassName, $Filter, $ErrorAction) [pscustomobject]@{ PathName = $script:servicePath } }
function Invoke-CimMethod {
    param($InputObject, $MethodName, $Arguments, $ErrorAction)
    Assert-True ($MethodName -eq 'Change') 'Unexpected service rollback method'
    $script:servicePath = $Arguments.PathName
    $script:events.Add('restore-scm:' + $script:servicePath)
    [pscustomobject]@{ ReturnValue = 0 }
}
function Stop-Service { param($Name, $ErrorAction) $script:events.Add('stop'); $script:fakeService.Status = [ServiceProcess.ServiceControllerStatus]::Stopped }
function Start-Service {
    param($Name, $ErrorAction)
    $script:events.Add('start:' + $script:files['C:\install\opc-modbus-server.exe'])
    $script:fakeService.Status = [ServiceProcess.ServiceControllerStatus]::Running
    if ($script:startFailuresRemaining -gt 0) { $script:startFailuresRemaining--; throw 'New service startup failed' }
}
function Invoke-FakePermissions { param([Parameter(ValueFromRemainingArguments=$true)]$Arguments) $script:events.Add('permissions'); $global:LASTEXITCODE = 0 }
function New-FakeService {
    $value = [pscustomobject]@{ Status = [ServiceProcess.ServiceControllerStatus]::Running }
    $value | Add-Member ScriptMethod WaitForStatus {
        param($Status, $Timeout)
        $script:events.Add("wait:$Status")
        $this.Status = $Status
        if ($Status -eq [ServiceProcess.ServiceControllerStatus]::Running -and $script:waitFailuresRemaining -gt 0) { $script:waitFailuresRemaining--; throw 'New service Running wait timed out' }
    }
    $value
}
function Invoke-FakeServer {
    param([Parameter(ValueFromRemainingArguments=$true)]$Arguments)
    $script:events.Add('server:' + ($Arguments -join '|'))
    $global:LASTEXITCODE = 0
    if ($Arguments[0] -eq 'build-info') { '{"trial":false}' }
    elseif ($Arguments[0] -eq 'service' -and $Arguments[1] -eq 'install') {
        if (-not $script:fakeService) { $script:fakeService = New-FakeService; $script:fakeService.Status = [ServiceProcess.ServiceControllerStatus]::Stopped }
        $script:servicePath = 'new-command-with-config-path'
        $global:LASTEXITCODE = $script:registerExit
    } elseif ($Arguments[0] -eq 'service' -and $Arguments[1] -eq 'uninstall') { $script:fakeService = $null }
}
function New-Object {
    param($ComObject)
    if ($ComObject -ne 'WScript.Shell') { throw 'Unexpected COM object' }
    $value = [pscustomobject]@{}
    $value | Add-Member ScriptMethod CreateShortcut {
        param($Path)
        $shortcut = [pscustomobject]@{ TargetPath=''; Arguments=''; WorkingDirectory=''; Description='' }
        $shortcut | Add-Member ScriptMethod Save { $script:events.Add('shortcut') }
        $shortcut
    }
    $value
}
function Reset-Fakes([bool]$Existing) {
    $script:events = [Collections.Generic.List[string]]::new()
    $script:files = @{
        'C:\package\opc-modbus-server.exe' = 'new-server'
        'C:\package\opc-modbus-config.exe' = 'new-configurator'
        'C:\package\config.example.json' = 'example-config'
        'C:\package\repair-data-permissions.ps1' = 'permission-helper'
        'C:\install' = '<directory>'
        'C:\data' = '<directory>'
        'C:\data\config.json' = 'operator-config-must-not-change'
    }
    if ($Existing) {
        $script:files['C:\install\opc-modbus-server.exe'] = 'old-server'
        $script:files['C:\install\opc-modbus-config.exe'] = 'old-configurator'
        $script:fakeService = New-FakeService
    } else { $script:fakeService = $null }
    $script:failConfigCopy = $false
    $script:registerExit = 0
    $script:startFailuresRemaining = 0
    $script:waitFailuresRemaining = 0
    $script:servicePath = 'original-command-with-original-config-path'
}
function Run-Installer { & $installer -InstallDir 'C:\install' -DataDir 'C:\data' -OperatorSid 'S-1-5-21-111-222-333-4444' }

Reset-Fakes $true
Run-Installer
Assert-True ($script:files['C:\data\config.json'] -eq 'operator-config-must-not-change') 'Upgrade changed operator config'
Assert-True ($script:files['C:\install\opc-modbus-server.exe'] -eq 'new-server') 'Upgrade did not install the server'
$stopIndex = $script:events.IndexOf('stop')
$replaceIndex = $script:events.FindIndex([Predicate[string]]{ param($event) $event -match '-server.new->C:\\install\\opc-modbus-server.exe$' })
Assert-True ($stopIndex -ge 0 -and $replaceIndex -gt $stopIndex) 'Server was replaced before the old service stopped'
Assert-True (($script:events | Where-Object { $_ -like 'server:service|install|*' }).Count -eq 1) 'Upgrade did not use the idempotent registration CLI'
Assert-True ($script:fakeService.Status -eq [ServiceProcess.ServiceControllerStatus]::Running) 'Upgrade did not start the service'

Reset-Fakes $false
Run-Installer
Assert-True (($script:events | Where-Object { $_ -like 'server:service|install|*' }).Count -eq 1) 'Fresh installation did not register service'
Assert-True ($script:files['C:\data\config.json'] -eq 'operator-config-must-not-change') 'Fresh installation replaced an existing config'

Reset-Fakes $true
$script:files.Remove('C:\package\opc-modbus-config.exe')
$failed = $false
try { Run-Installer } catch { $failed = $true }
Assert-True $failed 'Installer accepted an incomplete package'
Assert-True (-not $script:events.Contains('stop')) 'Invalid source package stopped the existing service'

Reset-Fakes $true
$script:failConfigCopy = $true
$failed = $false
try { Run-Installer } catch { $failed = $true }
Assert-True $failed 'Expected locked GUI executable failure'
Assert-True ($script:files['C:\install\opc-modbus-server.exe'] -eq 'old-server') 'Failed upgrade did not restore server'
Assert-True ($script:files['C:\install\opc-modbus-config.exe'] -eq 'old-configurator') 'Failed upgrade did not preserve configurator'
Assert-True ($script:files['C:\data\config.json'] -eq 'operator-config-must-not-change') 'Failed upgrade changed config'
Assert-True ($script:fakeService.Status -eq [ServiceProcess.ServiceControllerStatus]::Running) 'Failed upgrade did not restart previous service'

Reset-Fakes $true
$script:registerExit = 5
$failed = $false
try { Run-Installer } catch { $failed = $true }
Assert-True $failed 'Installer ignored SCM update error'
Assert-True ($script:files['C:\install\opc-modbus-server.exe'] -eq 'old-server') 'Registration failure did not restore server'
Assert-True ($script:files['C:\install\opc-modbus-config.exe'] -eq 'old-configurator') 'Registration failure did not restore configurator'
Assert-True ($script:files['C:\data\config.json'] -eq 'operator-config-must-not-change') 'Registration failure changed config'
Assert-True ($script:servicePath -eq 'original-command-with-original-config-path') 'Registration failure did not restore original SCM path'
Assert-True ($script:events.Contains('start:old-server')) 'Registration failure did not restart previous service'
Assert-True (-not $script:events.Contains('start:new-server')) 'Installer started new service after registration failure'
Assert-True (-not $script:events.Contains('shortcut')) 'Installer reported success after SCM update failure'

foreach ($failureKind in @('start', 'wait')) {
    Reset-Fakes $true
    if ($failureKind -eq 'start') { $script:startFailuresRemaining = 1 } else { $script:waitFailuresRemaining = 1 }
    $failed = $false
    try { Run-Installer } catch { $failed = $true }
    Assert-True $failed "Installer ignored service $failureKind failure"
    Assert-True ($script:files['C:\install\opc-modbus-server.exe'] -eq 'old-server') "Service $failureKind failure did not restore server"
    Assert-True ($script:files['C:\install\opc-modbus-config.exe'] -eq 'old-configurator') "Service $failureKind failure did not restore configurator"
    Assert-True ($script:files['C:\data\config.json'] -eq 'operator-config-must-not-change') "Service $failureKind failure changed config"
    Assert-True ($script:servicePath -eq 'original-command-with-original-config-path') "Service $failureKind failure did not restore original SCM path"
    Assert-True ($script:events.Contains('start:old-server')) "Service $failureKind failure did not restart previous service"
    Assert-True ($script:fakeService.Status -eq [ServiceProcess.ServiceControllerStatus]::Running) "Service $failureKind failure left original service stopped"
    Assert-True ($script:events.IndexOf('start:new-server') -lt $script:events.LastIndexOf('stop')) "Service $failureKind failure did not stop the new process before restoring binaries"
}

Reset-Fakes $false
$script:startFailuresRemaining = 1
$failed = $false
try { Run-Installer } catch { $failed = $true }
Assert-True $failed 'Fresh install ignored startup failure'
Assert-True (-not $script:fakeService) 'Failed fresh install retained an invalid new service'
Assert-True (-not $script:files.ContainsKey('C:\install\opc-modbus-server.exe')) 'Failed fresh install retained invalid new server'
Assert-True ($script:files['C:\data\config.json'] -eq 'operator-config-must-not-change') 'Failed fresh install changed existing data'
[Console]::WriteLine('Installer upgrade tests passed: existing/fresh service, incomplete package, locked EXE, registration/start/wait rollback, failed fresh install cleanup.')
