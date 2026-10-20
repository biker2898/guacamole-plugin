<# : batch part first, PowerShell part below
@echo off
rem Shows successful RDP logins and failed logins by source IP for the last
rem 30 days. Only reads. Run as administrator (the Security log needs it).
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

$since = (Get-Date).AddDays(-30)

Write-Host "== Successful RDP logins (last 30 days): who, from which IP" -ForegroundColor Cyan
Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-TerminalServices-RemoteConnectionManager/Operational'; Id=1149; StartTime=$since} -ErrorAction SilentlyContinue |
  ForEach-Object { [pscustomobject]@{Time=$_.TimeCreated; User=$_.Properties[0].Value; IP=$_.Properties[2].Value} } |
  Group-Object IP | Sort-Object Count -Descending | Select-Object Count, Name, @{n='Users'; e={($_.Group.User | Sort-Object -Unique) -join ', '}} |
  Format-Table -AutoSize | Out-String | Write-Host

Write-Host "== Failed logins by source IP (password guessing shows up here)" -ForegroundColor Cyan
Get-WinEvent -FilterHashtable @{LogName='Security'; Id=4625; StartTime=$since} -ErrorAction SilentlyContinue |
  ForEach-Object { $_.Properties[19].Value } | Group-Object | Sort-Object Count -Descending | Select-Object -First 15 Count, Name |
  Format-Table -AutoSize | Out-String | Write-Host
