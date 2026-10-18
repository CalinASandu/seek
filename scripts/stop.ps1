# Closes the Seek window and stops a console-mode Seek service started by Seek.bat.
# (An installed Windows service is left alone.)
Get-Process seek-app -ErrorAction SilentlyContinue | Stop-Process -Force
$console = Get-CimInstance Win32_Process -Filter "Name='seek-service.exe'" |
    Where-Object { $_.CommandLine -like '*--console*' }
foreach ($p in $console) {
    try { Stop-Process -Id $p.ProcessId -Force -ErrorAction Stop }
    catch { Start-Process taskkill.exe -ArgumentList "/F /PID $($p.ProcessId)" -Verb RunAs -WindowStyle Hidden }
}
Write-Host 'Seek stopped.'
