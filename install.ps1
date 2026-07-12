<#
.SYNOPSIS
    churl installer for Windows (PowerShell).

.DESCRIPTION
    Downloads the prebuilt churl binary for x86_64-pc-windows-msvc from GitHub
    Releases, verifies its SHA256 checksum, and installs it. PowerShell sibling
    of install.sh — same options and UX.

.PARAMETER To
    Install to this directory instead of the default
    ($env:LOCALAPPDATA\Programs\churl).

.PARAMETER Tag
    Install a specific release (e.g. v0.2.0-beta.1) instead of latest.

.PARAMETER Force
    Overwrite an existing churl binary.

.PARAMETER DryRun
    Print the resolved URL/target and exit without downloading.

.EXAMPLE
    irm https://github.com/AlsubaieAli/churl/releases/latest/download/install.ps1 | iex

.EXAMPLE
    pwsh install.ps1 -Tag v0.2.0 -To C:\Tools\churl -Force
#>

[CmdletBinding()]
param(
    [string]$To,
    [string]$Tag,
    [switch]$Force,
    [switch]$DryRun
)

# Stop on any error and treat a failed native command as terminating, so a
# broken download or checksum aborts the install rather than leaving a partial.
$ErrorActionPreference = 'Stop'

$Repo = 'AlsubaieAli/churl'
$Bin = 'churl'
$DefaultInstallDir = Join-Path $env:LOCALAPPDATA 'Programs\churl'

# Windows releases ship a single target triple (see release.yml matrix).
$Target = 'x86_64-pc-windows-msvc'

$InstallDir = if ($To) { $To } else { $DefaultInstallDir }

$Archive = "$Bin-$Target.zip"
# The release action names the checksum after the archive stem, not the full
# archive name: churl-<target>.sha256, NOT churl-<target>.zip.sha256. This
# mirrors install.sh exactly.
$Checksum = "$Bin-$Target.sha256"

# `latest` never resolves to a prerelease — betas are only reachable via -Tag.
$BaseUrl = if ($Tag) {
    "https://github.com/$Repo/releases/download/$Tag"
} else {
    "https://github.com/$Repo/releases/latest/download"
}
$ArchiveUrl = "$BaseUrl/$Archive"
$ChecksumUrl = "$BaseUrl/$Checksum"

if ($DryRun) {
    Write-Host "dry-run: target  = $Target"
    Write-Host "dry-run: archive = $ArchiveUrl"
    Write-Host "dry-run: install = $(Join-Path $InstallDir "$Bin.exe")"
    exit 0
}

# --- check for existing binary ---
$Dest = Join-Path $InstallDir "$Bin.exe"
if ((Test-Path -LiteralPath $Dest) -and (-not $Force)) {
    Write-Error "$Dest already exists - use -Force to overwrite"
    exit 1
}

# --- download into a temp directory ---
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $TmpDir | Out-Null
try {
    $ArchivePath = Join-Path $TmpDir $Archive
    $ChecksumPath = Join-Path $TmpDir $Checksum

    Write-Host "Downloading $ArchiveUrl ..."
    Invoke-WebRequest -Uri $ArchiveUrl -OutFile $ArchivePath -UseBasicParsing
    Invoke-WebRequest -Uri $ChecksumUrl -OutFile $ChecksumPath -UseBasicParsing

    # --- verify sha256 checksum ---
    # The checksum file is `<hash>  <archive-name>` (two-space separator, the
    # sha256sum format the release action emits). Compare the archive's actual
    # hash against the first whitespace-delimited field, case-insensitively.
    Write-Host "Verifying checksum ..."
    $Expected = ((Get-Content -LiteralPath $ChecksumPath -Raw).Trim() -split '\s+')[0]
    $Actual = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash
    if ($Actual -ne $Expected) {
        Write-Error "checksum mismatch: expected $Expected, got $Actual"
        exit 1
    }

    # --- extract + install ---
    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $TmpDir -Force
    if (-not (Test-Path -LiteralPath $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir | Out-Null
    }
    $Extracted = Join-Path $TmpDir "$Bin.exe"
    Move-Item -LiteralPath $Extracted -Destination $Dest -Force

    Write-Host "Installed $Bin to $Dest"
} finally {
    Remove-Item -Recurse -Force -LiteralPath $TmpDir -ErrorAction SilentlyContinue
}

# --- PATH hint ---
# Match install.sh: only nudge if the install dir isn't already on PATH.
$PathDirs = $env:PATH -split ';'
if ($InstallDir -notin $PathDirs) {
    Write-Host ''
    Write-Host "NOTE: $InstallDir is not in your PATH."
    Write-Host 'Add it for the current user with:'
    Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"`$([Environment]::GetEnvironmentVariable('Path','User'));$InstallDir`", 'User')"
    Write-Host 'Then open a new terminal.'
}
