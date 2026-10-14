# Give one explicitly selected operator Modify access to application data.
# Never grants Everyone/Users access and never changes equipment configuration.
param(
    [string]$DataDir = "$env:ProgramData\OPC Modbus Server",
    [string]$OperatorSid = "",
    [switch]$CheckOnly
)
$ErrorActionPreference = 'Stop'
if (-not $OperatorSid) { $OperatorSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value }
if ($OperatorSid -notmatch '^S-1-5-21-\d+-\d+-\d+-\d+$') { throw 'Specify an individual Windows operator SID' }
$taskDataPath = [IO.Path]::GetFullPath($DataDir).TrimEnd('\')
if ($taskDataPath -eq [IO.Path]::GetPathRoot($taskDataPath).TrimEnd('\')) { throw 'Refusing to change permissions at a drive root' }
if (-not (Test-Path -LiteralPath $taskDataPath -PathType Container)) { throw "Data directory does not exist: $taskDataPath" }
if ((Get-Item -LiteralPath $taskDataPath).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Data directory must not be a junction' }
$taskLinks = Get-ChildItem -LiteralPath $taskDataPath -Recurse -Force | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }
if ($taskLinks) { throw 'Refusing recursive ACL changes through a junction inside application data' }
if ($CheckOnly) {
    [pscustomobject]@{ DataDir = $taskDataPath; OperatorSid = $OperatorSid; Grant = 'Modify, this folder and its children' }
    exit 0
}
$taskAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if (-not $taskAdmin) {
    $taskElevated = Start-Process powershell -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList (
        '-NoProfile -ExecutionPolicy Bypass -File "{0}" -DataDir "{1}" -OperatorSid "{2}"' -f $PSCommandPath, $taskDataPath, $OperatorSid)
    exit $taskElevated.ExitCode
}
& "$env:SystemRoot\System32\icacls.exe" $taskDataPath /grant ("*{0}:(OI)(CI)(M)" -f $OperatorSid) /T /C /Q
if ($LASTEXITCODE -ne 0) { throw "Application data permission repair failed: $LASTEXITCODE" }
Write-Host "Granted Modify on $taskDataPath to $OperatorSid"
