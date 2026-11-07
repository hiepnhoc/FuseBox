<#
.SYNOPSIS
  Install or update Fusebox on Windows from the latest GitHub release and keep it
  running in the background (Task Scheduler, at logon) on http://127.0.0.1:8317.
  Running the script again updates to the newest release.

.EXAMPLE
  irm https://raw.githubusercontent.com/hiepnhoc/FuseBox/main/install/install.ps1 | iex
  powershell -ExecutionPolicy Bypass -File install.ps1 [-Codex] [-NoService] [-Force] [-Uninstall]

.PARAMETER Repo       GitHub repo whose releases are used (default hiepnhoc/FuseBox; DigitalPals/Fusebox for upstream)
.PARAMETER Version    "latest" or a tag such as v0.14.0
.PARAMETER Codex      also point ~/.codex/config.toml at Fusebox
.PARAMETER NoService  install the binary only, don't register the background task
.PARAMETER NoPath     don't add the install folder to the user PATH
.PARAMETER Force      reinstall even if this version is already installed
.PARAMETER Uninstall  remove the task and binary (keeps ~/.fusebox)
#>
[CmdletBinding()]
param(
  [string]$Repo = $(if ($env:FUSEBOX_REPO) { $env:FUSEBOX_REPO } else { 'hiepnhoc/FuseBox' }),
  [string]$Version = $(if ($env:FUSEBOX_VERSION) { $env:FUSEBOX_VERSION } else { 'latest' }),
  [switch]$Codex,
  [switch]$NoService,
  [switch]$NoPath,
  [switch]$Force,
  [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$InstallDir  = if ($env:FUSEBOX_BIN_DIR) { $env:FUSEBOX_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Fusebox' }
$FuseboxHome = if ($env:FUSEBOX_HOME) { $env:FUSEBOX_HOME } else { Join-Path $env:USERPROFILE '.fusebox' }
$Exe         = Join-Path $InstallDir 'fusebox.exe'
$Launcher    = Join-Path $InstallDir 'run-fusebox.ps1'
$Config      = Join-Path $FuseboxHome 'config.yaml'
$Port        = 8317
$TaskName    = 'Fusebox'
$Target      = 'x86_64-pc-windows-msvc'

function Say($msg)  { Write-Host "==> $msg" -ForegroundColor Green }
function Warn($msg) { Write-Host "warning: $msg" -ForegroundColor Yellow }
function Fail($msg) { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

function Test-Healthy {
  try { Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/healthz" -TimeoutSec 2 | Out-Null; $true }
  catch { $false }
}
function Wait-Healthy {
  for ($i = 0; $i -lt 30; $i++) { if (Test-Healthy) { return $true }; Start-Sleep -Milliseconds 500 }
  $false
}

# ---------------------------------------------------------------- service ----
function Stop-Fusebox {
  Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue | Stop-ScheduledTask -ErrorAction SilentlyContinue
  $procs = Get-Process -Name fusebox -ErrorAction SilentlyContinue
  if ($procs) {
    $procs | Stop-Process -Force
    $procs | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
  }
}

function Install-Service {
  New-Item -ItemType Directory -Force -Path $FuseboxHome | Out-Null
  # A small launcher runs fusebox.exe without a console window and writes its
  # output to ~/.fusebox/fusebox.log. The task runs this launcher at logon.
  @"
`$ErrorActionPreference = 'Stop'
`$exe    = Join-Path `$PSScriptRoot 'fusebox.exe'
`$fbHome = '$FuseboxHome'
`$cfg    = Join-Path `$fbHome 'config.yaml'
`$log    = Join-Path `$fbHome 'fusebox.log'
`$err    = Join-Path `$fbHome 'fusebox.err.log'
New-Item -ItemType Directory -Force -Path `$fbHome | Out-Null
foreach (`$f in @(`$log, `$err)) {
  if ((Test-Path `$f) -and (Get-Item `$f).Length -gt 10MB) { Move-Item -Force `$f "`$f.1" }
}
`$env:FUSEBOX_CONFIG = `$cfg
Start-Process -FilePath `$exe -ArgumentList @('--config', ('"' + `$cfg + '"')) -WorkingDirectory `$fbHome ``
  -WindowStyle Hidden -RedirectStandardOutput `$log -RedirectStandardError `$err -Wait
"@ | ForEach-Object { [IO.File]::WriteAllText($Launcher, $_, (New-Object Text.UTF8Encoding $false)) }

  $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$Launcher`"" `
    -WorkingDirectory $InstallDir
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
  $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
    -MultipleInstances IgnoreNew -StartWhenAvailable
  $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited

  Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
  Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal `
    -Description 'Fusebox: local AI proxy on http://127.0.0.1:8317' | Out-Null
  Start-ScheduledTask -TaskName $TaskName
  Say "Scheduled task '$TaskName' installed, runs at logon (log: $FuseboxHome\fusebox.log)"
}

function Remove-Service {
  Stop-Fusebox
  Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
  Remove-Item -Force -ErrorAction SilentlyContinue $Launcher
}

# -------------------------------------------------------------- uninstall ----
if ($Uninstall) {
  Say 'Removing Fusebox task and binary'
  Remove-Service
  Remove-Item -Force -ErrorAction SilentlyContinue $Exe
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath) {
    $newPath = ($userPath -split ';' | Where-Object { $_ -and $_ -ne $InstallDir }) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
  }
  Say "Done. Sign-ins, config and usage database are still in $FuseboxHome; delete that folder to remove them too."
  exit 0
}

if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
  Warn 'No native Windows ARM64 build is published; installing the x64 build, which runs under emulation.'
}

# ------------------------------------------------------------- resolve tag ----
if ($Version -eq 'latest') {
  try {
    # Newest release including pre-releases such as v0.10.1-hibox.1, which
    # GitHub's /releases/latest skips.
    $rel = @(Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$Repo/releases?per_page=1" `
      -Headers @{ 'User-Agent' = 'fusebox-install' })
    $Tag = $rel[0].tag_name
    if (-not $Tag) { throw 'no releases' }
  } catch {
    # API rate-limited or blocked: follow the /releases/latest redirect instead.
    $resp = [Net.HttpWebRequest]::Create("https://github.com/$Repo/releases/latest")
    $resp.AllowAutoRedirect = $false
    $Tag = ($resp.GetResponse().Headers['Location'] -split '/')[-1]
  }
  if (-not $Tag -or $Tag -notlike 'v*') { Fail "could not resolve the latest release of $Repo" }
} else {
  $Tag = if ($Version -like 'v*') { $Version } else { "v$Version" }
}

$installed = $null
if (Test-Path $Exe) { try { $installed = (& $Exe --version 2>$null) -join ' ' } catch { } }

if (-not $Force -and $installed -and $installed.Contains($Tag.TrimStart('v'))) {
  Say "Fusebox $Tag is already installed at $Exe"
  if (-not $NoService -and -not (Test-Healthy)) {
    Say 'It is not running; (re)installing the task'
    Install-Service
    if (Wait-Healthy) { Say "Fusebox is up at http://127.0.0.1:$Port" } else { Warn 'not answering yet; check the log' }
  }
} else {
  # ---------------------------------------------------------- download ----
  $Asset = "fusebox-$Tag-$Target.zip"
  $Base  = "https://github.com/$Repo/releases/download/$Tag"
  $Tmp   = Join-Path ([IO.Path]::GetTempPath()) ("fusebox-install-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
  try {
    Say "Downloading $Asset from $Repo"
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset" -OutFile (Join-Path $Tmp $Asset)
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/SHA256SUMS" -OutFile (Join-Path $Tmp 'SHA256SUMS')

    Say 'Verifying checksum'
    $line = Get-Content (Join-Path $Tmp 'SHA256SUMS') | Where-Object { $_ -like "* $Asset" -or $_ -like "* *$Asset" } | Select-Object -First 1
    if (-not $line) { Fail "$Asset is not listed in SHA256SUMS" }
    $expected = ($line -split '\s+')[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $Asset)).Hash.ToLower()
    if ($expected -ne $actual) { Fail "checksum mismatch for $Asset" }

    Expand-Archive -Path (Join-Path $Tmp $Asset) -DestinationPath (Join-Path $Tmp 'x') -Force
    $bin = Get-ChildItem -Path (Join-Path $Tmp 'x') -Recurse -Filter 'fusebox.exe' | Select-Object -First 1
    if (-not $bin) { Fail "fusebox.exe not found in $Asset" }

    # ----------------------------------------------------------- install ----
    New-Item -ItemType Directory -Force -Path $InstallDir, $FuseboxHome | Out-Null
    if (-not $NoService) { Stop-Fusebox }
    Copy-Item -Force $bin.FullName $Exe
    Unblock-File $Exe -ErrorAction SilentlyContinue
    $ver = try { (& $Exe --version 2>$null) -join ' ' } catch { "fusebox $Tag" }
    Say "Installed $ver to $Exe"

    if (-not $NoService) {
      Install-Service
      if (Wait-Healthy) { Say "Fusebox is up at http://127.0.0.1:$Port" } else { Warn "Fusebox is not answering on port $Port yet; check $FuseboxHome\fusebox.err.log" }
    }
  } finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $Tmp
  }
}

# ------------------------------------------------------------------ PATH ----
if (-not $NoPath) {
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not (($userPath -split ';') -contains $InstallDir)) {
    [Environment]::SetEnvironmentVariable('Path', (($userPath, $InstallDir | Where-Object { $_ }) -join ';'), 'User')
    Say "Added $InstallDir to your user PATH (open a new terminal to use 'fusebox')"
  }
  if (-not (($env:Path -split ';') -contains $InstallDir)) { $env:Path = "$env:Path;$InstallDir" }
}

# --------------------------------------------------------------- codex ----
$codexDone = 'or rerun this script with -Codex'
if ($Codex) {
  $codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
  $codexCfg = Join-Path $codexHome 'config.toml'
  New-Item -ItemType Directory -Force -Path $codexHome | Out-Null
  $existing = if (Test-Path $codexCfg) { Get-Content $codexCfg -Raw } else { '' }
  if ($existing -match '(?m)^\[model_providers\.fusebox\]') {
    Say "Codex already has [model_providers.fusebox] in $codexCfg; left as is"
  } else {
    if ($existing) { Copy-Item -Force $codexCfg "$codexCfg.bak" }
    # Top-level keys must come before any [table]: new ones first, then the
    # existing file, then the provider table at the end.
    $head = ''
    if ($existing -notmatch '(?m)^model_provider\s*=') { $head += "model_provider = `"fusebox`"`n" }
    if ($existing -notmatch '(?m)^model\s*=') { $head += "model = `"gpt-6-astra`"`n" }
    $tail = "`n[model_providers.fusebox]`nname = `"Fusebox`"`nbase_url = `"http://127.0.0.1:$Port/v1`"`nwire_api = `"responses`"`n"
    # WriteAllText with an explicit encoding: Set-Content -Encoding UTF8 adds a BOM
    # on Windows PowerShell 5.1, which TOML parsers reject.
    $body = if ($existing) { $head + $existing.TrimEnd() + "`n" + $tail } else { $head + $tail }
    [IO.File]::WriteAllText($codexCfg, $body, (New-Object Text.UTF8Encoding $false))
    $kept = if ($existing) { '; previous file kept as config.toml.bak' } else { '' }
    Say "Codex now uses Fusebox ($codexCfg$kept)"
  }
  $codexDone = 'already done'
}

Write-Host @"

Next steps
  1. Sign in once on this machine (tokens stay in $FuseboxHome):
       open http://127.0.0.1:$Port   and click "Connect account", or
       fusebox --config "$Config" login codex
       fusebox --config "$Config" login claude
  2. Point Codex at it ($codexDone), in %USERPROFILE%\.codex\config.toml:

       model = "gpt-6-astra"
       model_provider = "fusebox"

       [model_providers.fusebox]
       name = "Fusebox"
       base_url = "http://127.0.0.1:$Port/v1"
       wire_api = "responses"

  Update later: run this script again.   Remove: install.ps1 -Uninstall
"@
