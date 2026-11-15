# Installs the built Windows setup.exe for real, launches it, checks the background agent answers, then uninstalls.
# Run on a Windows machine: powershell -File scripts/windows-install-smoke.ps1 -Installer path\to\Backer_x64-setup.exe
param([Parameter(Mandatory)][string]$Installer)
$ErrorActionPreference = 'Stop'
function Step($m) { Write-Host "::group::$m" }
function EndStep { Write-Host '::endgroup::' }
function Fail($m) { Write-Host "::error::$m"; exit 1 }

$installDir = Join-Path $env:ProgramFiles 'Backer'            # per-machine install: the program lives in Program Files
$dataDir = Join-Path $env:LOCALAPPDATA 'Backer'               # the agent's data stays per user

Step 'Silent install'
$p = Start-Process -FilePath $Installer -ArgumentList '/S' -PassThru -Wait
Write-Host "installer exit code: $($p.ExitCode)"
if ($p.ExitCode -ne 0) { Fail "Installer exited with $($p.ExitCode)" }
Get-ChildItem -Recurse $installDir | Select-Object FullName, Length | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
$uninst = Get-ChildItem -Path $installDir -Filter 'uninstall*.exe' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $uninst) { Fail 'No uninstaller was installed' }
$app = Get-ChildItem -Path $installDir -Filter '*.exe' | Where-Object { $_.Name -notlike 'uninstall*' -and $_.Name -ne 'backer-agent.exe' } | Select-Object -First 1
$agent = Join-Path $installDir 'backer-agent.exe'
if (-not $app) { Fail 'The app executable was not installed' }
if (Test-Path (Join-Path $env:LOCALAPPDATA 'Backer\backer-agent.exe')) { Fail 'The program was installed under AppData\Local instead of Program Files' }
if (-not $app.FullName.StartsWith($env:ProgramFiles)) { Fail "The app was installed outside Program Files: $($app.FullName)" }
if (-not (Test-Path $agent)) { Fail 'backer-agent.exe was not installed next to the app' }
EndStep

function Ask($method) {
  $ep = Get-Content (Join-Path $dataDir 'ui.endpoint') -Raw | ConvertFrom-Json
  $c = New-Object System.Net.Sockets.TcpClient('127.0.0.1', [int]$ep.port)
  $s = $c.GetStream(); $w = New-Object IO.StreamWriter($s); $r = New-Object IO.StreamReader($s)
  $w.NewLine = "`n"; $w.AutoFlush = $true
  $w.WriteLine($ep.token); $w.WriteLine((@{ method = $method; params = @{} } | ConvertTo-Json -Compress))
  $c.ReceiveTimeout = 5000
  $line = $r.ReadLine(); $c.Close(); return $line
}
function WaitAgent($secs) {
  for ($i = 0; $i -lt $secs * 4; $i++) {
    try { $l = Ask 'list_jobs'; if ($l -match '"ok"\s*:\s*true') { return $l } } catch {}
    Start-Sleep -Milliseconds 250
  }
  return $null
}

Step 'Launch the app'
$proc = Start-Process -FilePath $app.FullName -PassThru
Start-Sleep -Seconds 8
if ($proc.HasExited) {
  Get-WinEvent -FilterHashtable @{ LogName = 'Application'; Level = 2; StartTime = (Get-Date).AddMinutes(-5) } -ErrorAction SilentlyContinue | Select-Object -First 5 | Format-List TimeCreated, ProviderName, Message | Out-String | Write-Host
  Fail "The app exited right after launch (code $($proc.ExitCode))"
}
$reply = WaitAgent 20
if (-not $reply) { Fail 'The background agent never answered after the app was launched' }
Write-Host "agent replied: $reply"
if (-not (schtasks /Query /TN BackerAgent 2>&1 | Out-String) -match 'BackerAgent') { Fail 'Login task BackerAgent was not registered' }
EndStep

Step 'A second launch must not start a second copy'
$name = [IO.Path]::GetFileNameWithoutExtension($app.Name)
$second = Start-Process -FilePath $app.FullName -PassThru
Start-Sleep -Seconds 6
$copies = @(Get-Process -Name $name -ErrorAction SilentlyContinue)
Write-Host "running copies of ${name}: $($copies.Count)"
if ($copies.Count -ne 1) { Fail "Expected exactly one running copy after a second launch, found $($copies.Count)" }
if (-not $second.HasExited) { Fail 'The second launch did not exit on its own' }
if ($proc.HasExited) { Fail 'The first copy died when the second one was launched' }
EndStep

Step 'Quit and uninstall'
try { Ask 'quit_agent' | Out-Null } catch {}
Start-Sleep -Seconds 3
Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($installDir) } | Stop-Process -Force
schtasks /Delete /TN BackerAgent /F 2>&1 | Out-Null
$u = Start-Process -FilePath $uninst.FullName -ArgumentList '/S' -PassThru -Wait
Write-Host "uninstaller exit code: $($u.ExitCode)"
Start-Sleep -Seconds 3
if (Test-Path $app.FullName) { Fail 'The app executable is still there after uninstall' }
EndStep
Write-Host 'Install, launch, agent and uninstall all worked.'
