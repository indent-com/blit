# Install blit on Windows — https://blit.sh
# Usage: irm https://install.blit.sh/install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "https://install.blit.sh"
$InstallDir = if ($env:BLIT_INSTALL_DIR) { $env:BLIT_INSTALL_DIR } else { "$env:LOCALAPPDATA\blit\bin" }

$Version = (Invoke-RestMethod "$Repo/latest").Trim()

$Current = $null
$BlitExe = Join-Path $InstallDir "blit.exe"
if (Test-Path $BlitExe) {
    $Current = (& $BlitExe --version 2>$null) -replace '.*\s', ''
    if ($Current -eq $Version) {
        Write-Host "blit $Version already installed."
        exit 0
    }
}

$Arch = "x86_64"
$ZipName = "blit_${Version}_windows_${Arch}.zip"
$Url = "$Repo/bin/$ZipName"

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $TmpDir | Out-Null

try {
    Write-Host "downloading blit $Version for windows/$Arch..."
    Invoke-WebRequest -Uri $Url -OutFile (Join-Path $TmpDir $ZipName) -UseBasicParsing
    Expand-Archive -Path (Join-Path $TmpDir $ZipName) -DestinationPath $TmpDir -Force

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Move-Item -Path (Join-Path $TmpDir "blit.exe") -Destination $BlitExe -Force
    Write-Host "installed blit $Version to $BlitExe"

    $UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$InstallDir;$UserPath", "User")
        $env:PATH = "$InstallDir;$env:PATH"
        Write-Host "added $InstallDir to PATH"
    }
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
