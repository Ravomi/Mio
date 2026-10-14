# Mio official installer for Windows PowerShell
# Repository: https://github.com/Ravomi/Mio

[CmdletBinding()]
param(
    [string]$Version = $env:MIO_VERSION,
    [string]$InstallDir = $env:MIO_INSTALL_DIR,
    [string]$BaseUrl = $env:MIO_RELEASE_BASE_URL
)

$ErrorActionPreference = 'Stop'

# Explicitly Windows-only guard
if (-not ($PSVersionTable.PSEdition -ne "Core" -or [System.Environment]::OSVersion.Platform -eq "Win32NT")) {
    Write-Error "Error: install.ps1 is designed for Windows PowerShell on Windows operating systems."
    exit 1
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = "latest"
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:USERPROFILE ".mio\bin"
}

if ([string]::IsNullOrWhiteSpace($BaseUrl)) {
    $BaseUrl = "https://github.com/Ravomi/Mio/releases"
}

# Require HTTPS unless explicitly in insecure test mode
$insecureTest = $env:MIO_ALLOW_INSECURE_TEST_HTTP -eq "1"
if (-not $insecureTest -and -not $BaseUrl.StartsWith("https://", [System.StringComparison]::OrdinalIgnoreCase)) {
    Write-Error "Error: BaseUrl must use HTTPS ($BaseUrl)"
    exit 1
}

Write-Host "==> Installing Mio coding agent for Windows..." -ForegroundColor Cyan

# Architecture check
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLower()
if ($arch -eq "x64") {
    $target = "x86_64-pc-windows-msvc"
} else {
    Write-Error "Error: Mio prebuilt binaries currently support Windows x64 only (detected: $arch)."
    exit 1
}

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("mio-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

try {
    $cleanBaseUrl = $BaseUrl.TrimEnd('/')
    if ($Version -eq "latest") {
        $downloadRoot = "$cleanBaseUrl/latest/download"
    } else {
        $downloadRoot = "$cleanBaseUrl/download/$Version"
    }

    $archiveName = "mio-$target.zip"
    $archiveUrl = "$downloadRoot/$archiveName"
    $checksumsUrl = "$downloadRoot/SHA256SUMS"

    $archivePath = Join-Path $tempDir $archiveName
    $checksumsPath = Join-Path $tempDir "SHA256SUMS"

    Write-Host "Detected target: $target"
    Write-Host "Target version:  $Version"
    Write-Host "Downloading checksums: $checksumsUrl"

    # Set TLS 1.2 safely without throwing if Tls13 enum doesn't exist
    try {
        $protocol = [Net.SecurityProtocolType]::Tls12
        if ([System.Enum]::IsDefined([Net.SecurityProtocolType], 'Tls13')) {
            $protocol = $protocol -bor [System.Enum]::Parse([Net.SecurityProtocolType], 'Tls13')
        }
        [Net.ServicePointManager]::SecurityProtocol = $protocol
    } catch {
        # Keep existing protocol if configuration fails
    }

    try {
        Invoke-WebRequest -Uri $checksumsUrl -OutFile $checksumsPath -UseBasicParsing -TimeoutSec 120
    } catch {
        Write-Error "Error: Failed to download SHA256SUMS from: $checksumsUrl. (Note: releases may not be published yet on Ravomi/Mio)"
        exit 1
    }

    Write-Host "Downloading archive: $archiveUrl"
    try {
        Invoke-WebRequest -Uri $archiveUrl -OutFile $archivePath -UseBasicParsing -TimeoutSec 120
    } catch {
        Write-Error "Error: Failed to download archive from: $archiveUrl"
        exit 1
    }

    # Verify SHA256 with exact match and reject duplicate entries for the same target
    $foundShaList = @()
    Get-Content -LiteralPath $checksumsPath | ForEach-Object {
        $line = $_.Trim()
        if ($line -match "^([a-fA-F0-9]{64})\s+(\*?)(.+)$") {
            $hash = $matches[1].ToLower()
            $name = $matches[3].Trim()
            if ($name -eq $archiveName) {
                $foundShaList += $hash
            }
        }
    }

    if ($foundShaList.Count -eq 0) {
        Write-Error "Error: Archive $archiveName not found in SHA256SUMS."
        exit 1
    }
    if ($foundShaList.Count -gt 1) {
        Write-Error "Error: Multiple entries found for $archiveName in SHA256SUMS."
        exit 1
    }

    $expectedSha = $foundShaList[0]

    $fileHashObj = Get-FileHash -LiteralPath $archivePath -Algorithm SHA256
    $actualSha = $fileHashObj.Hash.ToLower()

    if ($expectedSha -ne $actualSha) {
        Write-Error "Error: SHA256 checksum mismatch! Expected: $expectedSha, Actual: $actualSha"
        exit 1
    }

    Write-Host "SHA256 checksum verified: $actualSha" -ForegroundColor Green

    $extractDir = Join-Path $tempDir "extracted"
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force

    $exePath = Join-Path $extractDir "mio.exe"
    if (-not (Test-Path $exePath)) {
        Write-Error "Error: Archive does not contain 'mio.exe' at root."
        exit 1
    }

    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }

    $finalExe = Join-Path $InstallDir "mio.exe"
    $stageExe = Join-Path $InstallDir (".mio.tmp." + [System.Guid]::NewGuid().ToString("N") + ".exe")
    Copy-Item -Path $exePath -Destination $stageExe -Force
    Move-Item -Path $stageExe -Destination $finalExe -Force

    Write-Host "==> Successfully installed mio.exe to: $finalExe" -ForegroundColor Green

    # Exact semicolon-delimited PATH check and automatic addition for User & current session
    $normalizedInstallDir = $InstallDir.TrimEnd('\')
    
    # 1. Current process PATH
    $currentPathParts = ($env:PATH -split ';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    $alreadyInProcess = $false
    foreach ($p in $currentPathParts) {
        if ($p.TrimEnd('\').Equals($normalizedInstallDir, [System.StringComparison]::OrdinalIgnoreCase)) {
            $alreadyInProcess = $true
            break
        }
    }
    if (-not $alreadyInProcess) {
        $env:PATH = "$normalizedInstallDir;$env:PATH"
    }

    # 2. User environment PATH
    $userPathRaw = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($null -eq $userPathRaw) { $userPathRaw = "" }
    $userPathParts = ($userPathRaw -split ';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    $alreadyInUser = $false
    foreach ($p in $userPathParts) {
        if ($p.TrimEnd('\').Equals($normalizedInstallDir, [System.StringComparison]::OrdinalIgnoreCase)) {
            $alreadyInUser = $true
            break
        }
    }
    if (-not $alreadyInUser) {
        $newUserPath = if ([string]::IsNullOrWhiteSpace($userPathRaw)) { $normalizedInstallDir } else { "$userPathRaw;$normalizedInstallDir" }
        [Environment]::SetEnvironmentVariable("PATH", $newUserPath, "User")
        Write-Host "Added $normalizedInstallDir to User PATH." -ForegroundColor Cyan
    }
} finally {
    if (Test-Path $tempDir) {
        Remove-Item -Path $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
