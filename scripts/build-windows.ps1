param([switch]$SkipChecks)
$ErrorActionPreference='Stop'
$projectRoot=Split-Path -Parent $PSScriptRoot
$cargoCommand=Get-Command cargo.exe -ErrorAction SilentlyContinue
$cargoPath=if ($cargoCommand) { $cargoCommand.Source } else { Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
if (-not (Test-Path -LiteralPath $cargoPath)) { throw 'Install Rust from https://rustup.rs first.' }
$rustcPath=Join-Path (Split-Path -Parent $cargoPath) 'rustc.exe'
$rustHost=(& $rustcPath -vV | Select-String '^host:').ToString()
if ($rustHost -match 'windows-gnu') {
    $dlltoolCommand=Get-Command dlltool.exe,llvm-dlltool.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    $taskDlltool=if ($dlltoolCommand) { $dlltoolCommand.Source } else {
        $toolsRoot=Join-Path $env:USERPROFILE '.local\share\digital-kvm-tools'
        Get-ChildItem -LiteralPath $toolsRoot -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending | ForEach-Object { Join-Path $_.FullName 'bin\llvm-dlltool.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    }
    if (-not $taskDlltool) { throw 'GNU Rust requires dlltool (MinGW or LLVM-MinGW). Alternatively use Rust MSVC with the Visual Studio C++ build tools.' }
    $previousEncodedFlags=$env:CARGO_ENCODED_RUSTFLAGS
    $env:CARGO_ENCODED_RUSTFLAGS='-C' + [char]31 + 'dlltool=' + ($taskDlltool -replace '\\','/')
}
Push-Location $projectRoot
try {
    if (-not $SkipChecks) {
        & $cargoPath fmt -- --check
        if ($LASTEXITCODE -ne 0) { throw 'Formatting failed.' }
        & $cargoPath test --locked
        if ($LASTEXITCODE -ne 0) { throw 'Tests failed.' }
        & $cargoPath clippy --locked --all-targets -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw 'Lint checks failed.' }
    }
    & $cargoPath build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $dist=Join-Path $projectRoot 'dist\windows'
    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $projectRoot 'target\release\digital-kvm.exe') -Destination $dist
    Copy-Item -LiteralPath (Join-Path $projectRoot 'examples\windows.json') -Destination (Join-Path $dist 'config.json')
    Write-Output ('Built ' + (Join-Path $dist 'digital-kvm.exe'))
} finally {
    Pop-Location
    if ($rustHost -match 'windows-gnu') { $env:CARGO_ENCODED_RUSTFLAGS=$previousEncodedFlags }
}
