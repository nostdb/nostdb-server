param(
    [string]$Binary = "$PSScriptRoot\nostosd.exe",
    [string]$Config = "$env:ProgramData\NostosDB\server.toml",
    [string]$ServiceName = "NostosDB"
)

$ErrorActionPreference = "Stop"
$binaryPath = (Resolve-Path $Binary).Path
$configPath = (Resolve-Path $Config).Path
$command = '"{0}" serve --config "{1}"' -f $binaryPath, $configPath

if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    throw "Service '$ServiceName' already exists; this candidate script never replaces it."
}

sc.exe create $ServiceName binPath= $command start= auto DisplayName= "NostosDB Database Server"
if ($LASTEXITCODE -ne 0) { throw "sc.exe create failed with exit code $LASTEXITCODE" }
sc.exe description $ServiceName "NostosDB single-node database daemon"
if ($LASTEXITCODE -ne 0) { throw "sc.exe description failed with exit code $LASTEXITCODE" }
sc.exe failure $ServiceName reset= 86400 actions= restart/5000
if ($LASTEXITCODE -ne 0) { throw "sc.exe failure failed with exit code $LASTEXITCODE" }

Write-Host "Created $ServiceName. Review its restricted service identity before starting it."
