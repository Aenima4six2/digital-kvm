$ErrorActionPreference='Stop'
& (Join-Path $PSScriptRoot 'stop-windows.ps1')
$bootExe=Join-Path $env:ProgramFiles 'DigitalKvm\digital-kvm.exe'
$service=Get-CimInstance Win32_Service -Filter "Name = 'DigitalKvm'"
if ($service -and $service.PathName.StartsWith('"' + $bootExe + '" ',[StringComparison]::OrdinalIgnoreCase)) {& sc.exe delete DigitalKvm | Out-Null; if ($LASTEXITCODE -ne 0) {throw 'Run as Administrator to remove the boot service.'}}
$startupKey='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$startup=(Get-ItemProperty -LiteralPath $startupKey -Name DigitalKvm -ErrorAction SilentlyContinue).DigitalKvm
$loginExe=Join-Path $env:LOCALAPPDATA 'DigitalKvm\bin\digital-kvm.exe'
if ($startup -and $startup.StartsWith('"' + $loginExe + '"',[StringComparison]::OrdinalIgnoreCase)) {Remove-ItemProperty -LiteralPath $startupKey -Name DigitalKvm}
Write-Output 'Startup removed. Executables, configuration, and logs are retained.'
