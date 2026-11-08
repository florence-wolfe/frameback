# Builds dist\Frameback\: frameback.exe plus FFmpeg's DLLs beside it, and
# yt-dlp with the ffmpeg.exe it merges downloads with.
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
Copy-Item (Join-Path $env:FFMPEG_DIR 'bin\ffmpeg.exe') $out

# yt-dlp, pinned and checked against packaging\yt-dlp.sha256.
$pins = Get-Content 'packaging\yt-dlp.sha256' | Where-Object { $_ -notmatch '^#' }
$version = ($pins | Where-Object { $_ -match '^version ' }) -replace '^version ', ''
$expected = (($pins | Where-Object { $_ -match ' yt-dlp\.exe$' }) -split '\s+')[0]
$ytDlp = Join-Path $out 'yt-dlp.exe'
Invoke-WebRequest -Uri "https://github.com/yt-dlp/yt-dlp/releases/download/$version/yt-dlp.exe" -OutFile $ytDlp
$actual = (Get-FileHash -Algorithm SHA256 $ytDlp).Hash
if ($actual -ne $expected.ToUpper()) { throw "yt-dlp.exe doesn't match its checksum ($actual)" }
Write-Host "Built $out"
