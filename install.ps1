# Ollama Shepherd — Windows installer (PowerShell 5.1+)
#
#   irm https://raw.githubusercontent.com/gianni-bischoff/OllamaShepherd/master/install.ps1 | iex
#
# Optional environment overrides (set BEFORE piping):
#   $env:SHEPHERD_DEST   = "D:\Bin"     install location (default: %LOCALAPPDATA%\Programs\ollama-shepherd)
#   $env:SHEPHERD_VERSION = "v0.2.2"    pin a specific release (default: latest)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo    = "gianni-bischoff/OllamaShepherd"
$Asset   = "ollama-shepherd-x86_64-pc-windows-msvc.zip"
$Version = if ($env:SHEPHERD_VERSION) { $env:SHEPHERD_VERSION } else { "latest" }
$Dest    = if ($env:SHEPHERD_DEST)    { $env:SHEPHERD_DEST }    else { "$env:LOCALAPPDATA\Programs\ollama-shepherd" }

function Log($m)  { Write-Host "▸ $m" -ForegroundColor Cyan }
function Ok($m)   { Write-Host "✓ $m" -ForegroundColor Green }
function Die($m)  { Write-Host "✗ $m" -ForegroundColor Red; exit 1 }

$Base = "https://github.com/$Repo/releases"
$Url  = if ($Version -eq "latest") { "$Base/latest/download/$Asset" } else { "$Base/download/$Version/$Asset" }

$Tmp = Join-Path $env:Temp ("shepherd-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Tmp -Force | Out-Null
try {
    Log "downloading $Asset $Version…"
    try { Invoke-WebRequest "$Url" -OutFile "$Tmp\$Asset" -UseBasicParsing }
    catch { Die "download failed — is there a release asset for windows x86_64?" }

    Log "verifying checksum…"
    try { Invoke-WebRequest "$Url.sha256" -OutFile "$Tmp\checksums" -UseBasicParsing }
    catch { Die "checksum file download failed" }
    $Expected = (Get-Content "$Tmp\checksums" -Raw).Trim() -split '\s+' | Select-Object -First 1
    $Actual   = (Get-FileHash "$Tmp\$Asset" -Algorithm SHA256).Hash.ToLower()
    if ($Expected -ne $Actual) { Die "checksum mismatch — aborting" }

    Log "installing to $Dest…"
    Expand-Archive -LiteralPath "$Tmp\$Asset" -DestinationPath "$Tmp\extracted" -Force
    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    Copy-Item "$Tmp\extracted\ollama-shepherd.exe" $Dest -Force

    # add to user PATH if missing
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($UserPath -split ';') -notcontains $Dest) {
        [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dest", "User")
        Log "added $Dest to your user PATH — restart your terminal for it to apply"
    }

    Ok "installed $(& (Join-Path $Dest 'ollama-shepherd.exe') --version)"
    Write-Host ""
    Ok "run it:  ollama-shepherd"
    Write-Host "   update later with:  ollama-shepherd update"
}
finally {
    Remove-Item $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}