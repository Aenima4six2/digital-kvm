param(
    [string]$Config,
    [ValidateSet('Auto','Boot','Login')][string]$Mode='Auto',
    [switch]$DryRun,
    [switch]$NoStart,
    [switch]$NoStartup
)
$ErrorActionPreference='Stop'
$projectRoot=Split-Path -Parent $PSScriptRoot
$sourceExe=Join-Path $PSScriptRoot 'digital-kvm.exe'
$defaultConfig=Join-Path $PSScriptRoot 'config.json'
if (-not (Test-Path -LiteralPath $sourceExe)) {
    $sourceExe=Join-Path $projectRoot 'dist\windows\digital-kvm.exe'
    $defaultConfig=Join-Path $projectRoot 'examples\windows.json'
}
if (-not (Test-Path -LiteralPath $sourceExe)) {throw 'Build with scripts\build-windows.ps1 or extract a Windows release package first.'}
$admin=[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($Mode -ne 'Login' -and -not $admin -and -not $NoStartup) {throw 'Boot setup requires an Administrator PowerShell. Run this installer there, or explicitly select -Mode Login.'}
if ($NoStartup -and $Mode -eq 'Auto') {$Mode='Login'}
$boot=$Mode -ne 'Login'
$installRoot=if ($boot) {Join-Path $env:ProgramData 'DigitalKvm'} else {Join-Path $env:LOCALAPPDATA 'DigitalKvm'}
$binRoot=if ($boot) {Join-Path $env:ProgramFiles 'DigitalKvm'} else {Join-Path $installRoot 'bin'}
$destinationExe=Join-Path $binRoot 'digital-kvm.exe'
$destinationConfig=Join-Path $installRoot 'config.json'
$logPath=Join-Path $installRoot 'digital-kvm.log'
$bootExe=Join-Path $env:ProgramFiles 'DigitalKvm\digital-kvm.exe'
function Assert-OwnedService([string]$Name) {
    $existing=Get-CimInstance Win32_Service -Filter ("Name = '" + $Name + "'")
    if ($existing -and -not $existing.PathName.StartsWith('"' + $bootExe + '" ',[StringComparison]::OrdinalIgnoreCase)) {throw "Service $Name belongs to a different executable; refusing to replace it."}
    return $existing
}
$existing=Assert-OwnedService 'DigitalKvm'
if ($existing) {
    Stop-Service DigitalKvm -ErrorAction Stop
    (Get-Service DigitalKvm).WaitForStatus('Stopped',[TimeSpan]::FromSeconds(10))
    if (-not $boot -and -not $NoStartup) {
        if (-not $admin) {throw 'Changing an existing boot service to login startup requires Administrator PowerShell.'}
        & sc.exe delete DigitalKvm | Out-Null
        if ($LASTEXITCODE -ne 0) {throw 'Cannot remove the previous boot service.'}
        $existing=$null
    }
}
$ownedPaths=@($bootExe,$destinationExe,(Join-Path $env:LOCALAPPDATA 'DigitalKvm\bin\digital-kvm.exe'))
Get-CimInstance Win32_Process -Filter "Name = 'digital-kvm.exe'" | Where-Object {$_.ExecutablePath -in $ownedPaths} | ForEach-Object {
    $ownedProcess=Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue
    if ($ownedProcess) {
        Stop-Process -InputObject $ownedProcess
        if (-not $ownedProcess.WaitForExit(10000)) {throw 'The old Digital KVM executable did not stop.'}
    }
}
New-Item -ItemType Directory -Path $binRoot,$installRoot -Force | Out-Null
if (-not $Config -and -not (Test-Path -LiteralPath $destinationConfig)) {$Config=$defaultConfig}
if ($Config) {
    $resolvedConfig=(Resolve-Path -LiteralPath $Config).Path
    if ((Get-Content -LiteralPath $resolvedConfig -Raw).Contains('REPLACE_WITH_')) {
        & $sourceExe learn-switch --config $resolvedConfig --output $destinationConfig
        if ($LASTEXITCODE -ne 0) {throw 'Physical switch discovery failed.'}
        $resolvedConfig=$destinationConfig
    }
    & $sourceExe validate --config $resolvedConfig
    if ($LASTEXITCODE -ne 0) {throw 'Configuration validation failed.'}
}
Copy-Item -LiteralPath $sourceExe -Destination $destinationExe -Force
if ($Config -and $resolvedConfig -ne $destinationConfig) {Copy-Item -LiteralPath $resolvedConfig -Destination $destinationConfig -Force}
& $destinationExe validate --config $destinationConfig
if ($LASTEXITCODE -ne 0) {throw 'Installed configuration validation failed.'}
if ($boot -and -not $NoStartup) {
    $probeLog=Join-Path $installRoot 'boot-probe.log'
    $priorProbe=Assert-OwnedService 'DigitalKvmProbe'
    if ($priorProbe) {throw 'A previous owned DigitalKvmProbe exists; remove it before reinstalling.'}
    $probeArgs='"' + $destinationExe + '" service-probe --service-name DigitalKvmProbe --force --config "' + $destinationConfig + '" --log "' + $probeLog + '"'
    $probeStarted=Get-Date
    $reachable=$false
    try {
        New-Service -Name DigitalKvmProbe -BinaryPathName $probeArgs -StartupType Manual | Out-Null
        Start-Service DigitalKvmProbe
        $deadline=(Get-Date).AddSeconds(20)
        do {
            Start-Sleep -Milliseconds 200
            $probeRecords=if (Test-Path -LiteralPath $probeLog) {Get-Content -LiteralPath $probeLog | ForEach-Object {$_ | ConvertFrom-Json} | Where-Object {$_.event -eq 'monitor_probe' -and $_.time_ms -ge [DateTimeOffset]::new($probeStarted).ToUnixTimeMilliseconds()}}
        } while (-not $probeRecords -and (Get-Date) -lt $deadline)
        $reachable=($probeRecords | Select-Object -Last 1).details.reachable -eq $true
    } finally {
        $probeService=Get-Service DigitalKvmProbe -ErrorAction SilentlyContinue
        if ($probeService -and $probeService.Status -ne 'Stopped') {Stop-Service DigitalKvmProbe}
        if ($probeService) {& sc.exe delete DigitalKvmProbe | Out-Null; if ($LASTEXITCODE -ne 0) {throw 'Cannot remove the temporary boot probe.'}}
    }
    if (-not $reachable) {
        if ($Mode -eq 'Boot') {throw "Monitor control failed in the Windows service context. Evidence: $probeLog. Use -Mode Login as fallback."}
        Write-Output "Boot monitor control was not demonstrated. Installing login fallback; evidence: $probeLog"
        if ($existing) {& sc.exe delete DigitalKvm | Out-Null}
        & $PSCommandPath -Config $destinationConfig -Mode Login -DryRun:$DryRun -NoStart:$NoStart
        return
    }
    $serviceArgs='"' + $destinationExe + '" service --config "' + $destinationConfig + '" --log "' + $logPath + '"'
    if ($DryRun) {$serviceArgs += ' --dry-run'}
    if ($existing) {
        $change=Invoke-CimMethod -InputObject $existing -MethodName Change -Arguments @{PathName=$serviceArgs;StartMode='Automatic'}
        if ($change.ReturnValue -ne 0) {throw "Cannot update service configuration (Windows status $($change.ReturnValue))."}
    } else {New-Service -Name DigitalKvm -DisplayName 'Digital KVM' -BinaryPathName $serviceArgs -StartupType Automatic -Description 'USB switch driven monitor input selection' | Out-Null}
    & sc.exe failure DigitalKvm reset= 86400 actions= restart/5000/restart/15000/restart/30000 | Out-Null
    if ($LASTEXITCODE -ne 0) {throw 'Cannot configure service recovery.'}
    Remove-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name DigitalKvm -ErrorAction SilentlyContinue
    if (-not $NoStart) {Start-Service DigitalKvm}
    Write-Output 'Installed native Windows service DigitalKvm with automatic boot startup.'
} elseif (-not $NoStartup) {
    $arguments='run --background --config "' + $destinationConfig + '" --log "' + $logPath + '"'
    if ($DryRun) {$arguments += ' --dry-run'}
    $startupKey='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    New-Item -Path $startupKey -Force | Out-Null
    New-ItemProperty -Path $startupKey -Name DigitalKvm -PropertyType String -Value ('"' + $destinationExe + '" ' + $arguments) -Force | Out-Null
    if (-not $NoStart) {Start-Process -FilePath $destinationExe -ArgumentList $arguments -WindowStyle Hidden}
    Write-Output 'Installed Windows login fallback.'
}
Write-Output "Executable: $destinationExe"
Write-Output "Configuration: $destinationConfig"
Write-Output "Log: $logPath"
