# Builds dist\Frameback\: frameback.exe plus FFmpeg's DLLs beside it.
# Expects FFMPEG_DIR to point at an FFmpeg 9 shared build (CI uses BtbN's).
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
if (-not $env:FFMPEG_DIR) { throw 'Set FFMPEG_DIR to an FFmpeg 9 build' }

cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$out = 'dist\Frameback'
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Path $out | Out-Null
Copy-Item 'target\release\frameback.exe' $out
Copy-Item (Join-Path $env:FFMPEG_DIR 'bin\*.dll') $out
Write-Host "Built $out"
