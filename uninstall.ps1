# dsync Uninstaller for Windows
# Run as Administrator in PowerShell

$ErrorActionPreference = "Stop"

$ServiceName = "dsync"
$InstallDir = "C:\Program Files\dsync"
# dsync#147: the daemon uses C:\ProgramData\dsync, else the pre-1.25 dir in place; offer both.
$DataDirs = @("C:\ProgramData\dsync", "C:\ProgramData\DanteSync")
# The pre-1.25 installation, removed too.
$LegacyServiceName = "dantesync"
$LegacyInstallDir = "C:\Program Files\DanteSync"

Write-Host ">>> dsync Windows Uninstaller <<<" -ForegroundColor Cyan

# 1. Stop and remove tray app
Write-Host "Stopping tray application..."
Stop-Process -Name "dsync-tray" -Force -ErrorAction SilentlyContinue
Stop-Process -Name "dantesync-tray" -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

# 2. Remove scheduled task
Write-Host "Removing scheduled task..."
Unregister-ScheduledTask -TaskName "DsyncTray" -Confirm:$false -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName "DanteSyncTray" -Confirm:$false -ErrorAction SilentlyContinue

# 3. Remove registry startup entries
Write-Host "Removing registry startup entries..."
$RegPathCU = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$RegPathLM = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"

try {
    Remove-ItemProperty -Path $RegPathCU -Name "DsyncTray" -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path $RegPathCU -Name "DanteSyncTray" -ErrorAction SilentlyContinue
    Write-Host "  - Removed current user registry entry." -ForegroundColor Gray
} catch { }

try {
    Remove-ItemProperty -Path $RegPathLM -Name "DsyncTray" -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path $RegPathLM -Name "DanteSyncTray" -ErrorAction SilentlyContinue
    Write-Host "  - Removed machine-wide registry entry." -ForegroundColor Gray
} catch { }

# 3b. Remove Start Menu shortcut
Write-Host "Removing Start Menu shortcut..."
foreach ($StartMenuShortcut in @("$env:ProgramData\Microsoft\Windows\Start Menu\Programs\dsync.lnk", "$env:ProgramData\Microsoft\Windows\Start Menu\Programs\DanteSync.lnk")) {
    try {
        if (Test-Path $StartMenuShortcut) {
            Remove-Item -Path $StartMenuShortcut -Force -ErrorAction SilentlyContinue
            Write-Host "  - Removed Start Menu shortcut $StartMenuShortcut" -ForegroundColor Gray
        }
    } catch { }
}

# 3c. Remove Add/Remove Programs registry entry
Write-Host "Removing from Add/Remove Programs..."
foreach ($UninstallKey in @("HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\dsync", "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\DanteSync")) {
    try {
        if (Test-Path $UninstallKey) {
            Remove-Item -Path $UninstallKey -Recurse -Force -ErrorAction SilentlyContinue
            Write-Host "  - Removed from Add/Remove Programs ($UninstallKey)" -ForegroundColor Gray
        }
    } catch { }
}

# 4. Stop and remove the service (and a pre-1.25 one)
foreach ($Name in @($ServiceName, $LegacyServiceName)) {
    $Service = Get-Service -Name $Name -ErrorAction SilentlyContinue
    if ($Service) {
        Write-Host "Stopping service '$Name'..."
        Stop-Service -Name $Name -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2

        Write-Host "Removing service '$Name'..."
        sc.exe delete $Name | Out-Null
        Start-Sleep -Seconds 1
    }
}

# Kill any remaining processes
Stop-Process -Name "dsync" -Force -ErrorAction SilentlyContinue
Stop-Process -Name "dantesync" -Force -ErrorAction SilentlyContinue

# 5. Remove program files
Write-Host "Removing program files..."
foreach ($Dir in @($InstallDir, $LegacyInstallDir)) {
    if (Test-Path $Dir) {
        Remove-Item -Path $Dir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "  - Removed $Dir" -ForegroundColor Gray
    }
}

# 6. Ask about each data directory (contains config and logs)
foreach ($DataDir in $DataDirs) {
    if (Test-Path $DataDir) {
        $Response = Read-Host "Remove configuration and logs at $DataDir? (y/N)"
        if ($Response -eq 'y' -or $Response -eq 'Y') {
            Remove-Item -Path $DataDir -Recurse -Force -ErrorAction SilentlyContinue
            Write-Host "  - Removed $DataDir" -ForegroundColor Gray
        } else {
            Write-Host "  - Kept $DataDir (config and logs preserved)" -ForegroundColor Gray
        }
    }
}

Write-Host "Uninstallation Complete!" -ForegroundColor Green
