<# : batch part first, PowerShell part below
@echo off
rem Tunes a small Windows Server VM (about 8 GB RAM, few cores) used for
rem office work over RDP: fewer out-of-memory errors, less background load.
rem Run as administrator, then restart Windows.
net session >nul 2>&1
if errorlevel 1 (
    echo Asking for administrator rights...
    powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    exit /b
)
powershell -NoProfile -ExecutionPolicy Bypass -Command "iex ([IO.File]::ReadAllText('%~f0'))"
echo.
pause
exit /b
#>

# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------

# Page file. Windows refuses memory once RAM + page file is used up, so the
# maximum is what prevents out-of-memory errors. Needs this much free disk.
$PagefileMinMB = 8192
$PagefileMaxMB = 16384

# Windows Search indexing: Outlook desktop search needs it; set to $true
# only when nobody uses Outlook desktop.
$DisableWindowsSearch = $false

# Print Spooler: printing (also to a printer redirected over RDP) needs it;
# set to $true only when nobody prints.
$DisablePrintSpooler = $false

# ---------------------------------------------------------------------------

$ErrorActionPreference = 'Stop'
$restartNeeded = $false

function Step($name, [scriptblock]$action) {
    try {
        $result = & $action
        if ($result -is [string]) {
            Write-Host "[SKIP] ${name}: $result" -ForegroundColor Yellow
        }
        else {
            Write-Host "[ OK ] $name" -ForegroundColor Green
        }
    }
    catch {
        Write-Host "[FAIL] ${name}: $($_.Exception.Message)" -ForegroundColor Red
    }
}

function Set-Dword($path, $name, $value) {
    if (-not (Test-Path $path)) {
        New-Item -Path $path -Force | Out-Null
    }
    New-ItemProperty -Path $path -Name $name -Value $value -PropertyType DWord -Force | Out-Null
}

Write-Host "Tuning this VM for office work on low memory"
Write-Host ""

Step "Page file $PagefileMinMB-$PagefileMaxMB MB" {
    $settings = @(Get-CimInstance Win32_PageFileSetting)
    $paths = if ($settings.Count) { $settings.Name } else { @('C:\pagefile.sys') }
    foreach ($path in $paths) {
        $drive = $path.Substring(0, 2)
        $disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$drive'"
        $usage = Get-CimInstance Win32_PageFileUsage | Where-Object { $_.Name -eq $path }
        $availableMB = $disk.FreeSpace / 1MB + $(if ($usage) { $usage.AllocatedBaseSize } else { 0 })
        if ($availableMB -lt $PagefileMaxMB + 5120) {
            throw "only $([int]$availableMB) MB available on $drive; lower `$PagefileMaxMB or free up space"
        }
    }
    $computer = Get-CimInstance Win32_ComputerSystem
    if ($computer.AutomaticManagedPagefile) {
        Set-CimInstance -InputObject $computer -Property @{ AutomaticManagedPagefile = $false }
    }
    if (-not $settings.Count) {
        $settings = @(New-CimInstance -ClassName Win32_PageFileSetting -Property @{ Name = 'C:\pagefile.sys' })
    }
    foreach ($setting in $settings) {
        Set-CimInstance -InputObject $setting -Property @{
            InitialSize = [uint32]$PagefileMinMB
            MaximumSize = [uint32]$PagefileMaxMB
        }
    }
    $global:restartNeeded = $true
}

Step "Memory compression" {
    if ((Get-MMAgent).MemoryCompression) {
        return 'already on'
    }
    Enable-MMAgent -MemoryCompression
    $global:restartNeeded = $true
}

Step "Server Manager does not open at logon" {
    Get-ScheduledTask -TaskName ServerManager -ErrorAction SilentlyContinue |
        Disable-ScheduledTask | Out-Null
    Set-Dword 'HKLM:\SOFTWARE\Microsoft\ServerManager' DoNotOpenServerManagerAtLogon 1
}

Step "Defender: scans use at most 20% CPU, weekly scan Sunday 02:00" {
    if (-not (Get-Command Set-MpPreference -ErrorAction SilentlyContinue)) {
        return 'Defender is not installed'
    }
    Set-MpPreference -ScanAvgCPULoadFactor 20 -ScanScheduleDay 1 -ScanScheduleTime '02:00:00'
    if ((Get-Command Set-MpPreference).Parameters.ContainsKey('EnableLowCpuPriority')) {
        Set-MpPreference -EnableLowCpuPriority $true
    }
}

Step "Windows Update does not upload to other PCs" {
    Set-Dword 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization' DODownloadMode 0
}

Step "Hibernation off (frees disk space the size of RAM)" {
    cmd /c "powercfg /hibernate off >nul 2>&1"
}

Step "High performance power plan" {
    cmd /c "powercfg /setactive SCHEME_MIN >nul 2>&1"
    if ($LASTEXITCODE) { throw 'powercfg could not switch the plan' }
}

Step "Edge: sleeping tabs after 5 min, no background running, no GPU mode" {
    $edge = 'HKLM:\SOFTWARE\Policies\Microsoft\Edge'
    Set-Dword $edge SleepingTabsEnabled 1
    Set-Dword $edge SleepingTabsTimeout 300
    Set-Dword $edge StartupBoostEnabled 0
    Set-Dword $edge BackgroundModeEnabled 0
    Set-Dword $edge HardwareAccelerationModeEnabled 0
}

Step "Chrome: Memory Saver, no background running, no GPU mode" {
    $chrome = 'HKLM:\SOFTWARE\Policies\Google\Chrome'
    Set-Dword $chrome HighEfficiencyModeEnabled 1
    Set-Dword $chrome BackgroundModeEnabled 0
    Set-Dword $chrome HardwareAccelerationModeEnabled 0
}

# Per-user setting: applied now for this account, and through Active Setup
# once for every other user at their next logon
Step "Office: hardware graphics acceleration off, for every user" {
    $command = 'reg add "HKCU\Software\Microsoft\Office\16.0\Common\Graphics" /v DisableHardwareAcceleration /t REG_DWORD /d 1 /f'
    Set-Dword 'HKCU:\Software\Microsoft\Office\16.0\Common\Graphics' DisableHardwareAcceleration 1
    $activeSetup = 'HKLM:\SOFTWARE\Microsoft\Active Setup\Installed Components\SmallVmOfficeGraphics'
    if (-not (Test-Path $activeSetup)) {
        New-Item -Path $activeSetup -Force | Out-Null
    }
    Set-ItemProperty -Path $activeSetup -Name '(default)' -Value 'Office graphics acceleration off'
    Set-ItemProperty -Path $activeSetup -Name StubPath -Value $command
    Set-ItemProperty -Path $activeSetup -Name Version -Value '1'
    Set-Dword $activeSetup IsInstalled 1
}

Step "Windows Search indexing off" {
    if (-not $DisableWindowsSearch) {
        return 'kept on (Outlook search needs it; see settings at the top)'
    }
    Stop-Service WSearch -Force -ErrorAction SilentlyContinue
    Set-Service WSearch -StartupType Disabled
}

Step "Print Spooler off" {
    if (-not $DisablePrintSpooler) {
        return 'kept on (printing needs it; see settings at the top)'
    }
    Stop-Service Spooler -Force -ErrorAction SilentlyContinue
    Set-Service Spooler -StartupType Disabled
}

Write-Host ""
$os = Get-CimInstance Win32_OperatingSystem
Write-Host ("RAM {0:N1} GB, commit limit {1:N1} GB (RAM + page file, before restart)" -f `
    ($os.TotalVisibleMemorySize / 1MB), ($os.TotalVirtualMemorySize / 1MB))
Write-Host "Largest processes now:"
Get-Process | Sort-Object WorkingSet64 -Descending | Select-Object -First 10 |
    Format-Table Name, @{ n = 'MB'; e = { [int]($_.WorkingSet64 / 1MB) } } -AutoSize | Out-Host

if ($restartNeeded) {
    Write-Host "Restart Windows to apply the page file and memory compression." -ForegroundColor Cyan
}
Write-Host "Users need to restart Edge, Chrome and Office to pick up their settings."
