# Install blit on Windows — https://blit.sh
# Usage: irm https://install.blit.sh/install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "https://install.blit.sh"
$InstallDir = if ($env:BLIT_INSTALL_DIR) { $env:BLIT_INSTALL_DIR } else { "$env:LOCALAPPDATA\blit\bin" }

$Version = (Invoke-RestMethod "$Repo/latest").Trim()

$Current = $null
$BlitExe = Join-Path $InstallDir "blit.exe"

# A running blit.exe can be renamed during an upgrade but cannot be deleted
# until that process exits. Clean up backups left by earlier upgrades.
if (Test-Path -LiteralPath $InstallDir) {
    Get-ChildItem -LiteralPath $InstallDir -Filter "blit.exe.old.*" -File -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
}

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
    # Windows PowerShell's progress rendering can make Invoke-WebRequest much
    # slower than the network transfer itself. Suppress it only for the
    # download and restore the caller's preference afterwards.
    $OldProgressPreference = $ProgressPreference
    try {
        $ProgressPreference = "SilentlyContinue"
        Invoke-WebRequest -Uri $Url -OutFile (Join-Path $TmpDir $ZipName) -UseBasicParsing
    } finally {
        $ProgressPreference = $OldProgressPreference
    }

    Write-Host "extracting blit $Version..."
    Expand-Archive -Path (Join-Path $TmpDir $ZipName) -DestinationPath $TmpDir -Force

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null

    # Windows does not let us overwrite an executable while it is running.
    # Renaming it is allowed, however, and leaves the running process attached
    # to the old file while new invocations use the replacement.
    $InstallId = [guid]::NewGuid().ToString("N")
    $StagedExe = "$BlitExe.new.$InstallId"
    $BackupExe = $null
    Copy-Item -LiteralPath (Join-Path $TmpDir "blit.exe") -Destination $StagedExe -Force

    try {
        if (Test-Path -LiteralPath $BlitExe) {
            $BackupExe = "$BlitExe.old.$InstallId"
            Move-Item -LiteralPath $BlitExe -Destination $BackupExe
        }
        Move-Item -LiteralPath $StagedExe -Destination $BlitExe
    } catch {
        $InstallError = $_
        Remove-Item -LiteralPath $StagedExe -Force -ErrorAction SilentlyContinue
        if ($BackupExe -and
            (Test-Path -LiteralPath $BackupExe) -and
            -not (Test-Path -LiteralPath $BlitExe)) {
            try {
                Move-Item -LiteralPath $BackupExe -Destination $BlitExe
            } catch {
                Write-Warning "failed to restore the previous blit.exe from $BackupExe"
            }
        }
        throw $InstallError
    }

    # This succeeds for ordinary installs. During `blit upgrade` the old file
    # remains in use until this installer and its parent return, so a later
    # install removes it via the stale-backup cleanup above.
    if ($BackupExe) {
        Remove-Item -LiteralPath $BackupExe -Force -ErrorAction SilentlyContinue
    }
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
