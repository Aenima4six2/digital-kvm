$ErrorActionPreference='Stop'
$bootExe=Join-Path $env:ProgramFiles 'DigitalKvm\digital-kvm.exe'
$service=Get-CimInstance Win32_Service -Filter "Name = 'DigitalKvm'"
if ($service -and $service.PathName.StartsWith('"' + $bootExe + '" ',[StringComparison]::OrdinalIgnoreCase)) {Stop-Service DigitalKvm}
$loginExe=Join-Path $env:LOCALAPPDATA 'DigitalKvm\bin\digital-kvm.exe'
Get-CimInstance Win32_Process -Filter "Name = 'digital-kvm.exe'" | Where-Object {$_.ExecutablePath -eq $loginExe} | ForEach-Object {Stop-Process -Id $_.ProcessId}
