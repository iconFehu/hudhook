# Build the injector first, then run this script with -Architecture x86 or x64.
# It only injects harmless fixtures into the host process created here.
param(
    [ValidateSet('x86', 'x64')][string]$Architecture = 'x64',
    [string]$Injector
)
$ErrorActionPreference = 'Stop'
$taskRepo = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$taskTarget = if ($Architecture -eq 'x86') { 'i686-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
if (-not $Injector) {
    $Injector = Join-Path $taskRepo "target\$taskTarget\release\examples\injector.exe"
}
if (-not (Test-Path -LiteralPath $Injector)) { throw "Build the injector first: $Injector" }
$taskVswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$taskVs = & $taskVswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $taskVs) { throw 'Visual Studio C++ tools are required.' }
$taskBuild = Join-Path $taskRepo "target\injector-resume-$Architecture"
New-Item -ItemType Directory -Force -Path $taskBuild | Out-Null
$taskBuildScript = @"
@echo off
call "$taskVs\VC\Auxiliary\Build\vcvarsall.bat" $Architecture
if errorlevel 1 exit /b 1
cl /nologo /LD /MD "$PSScriptRoot\fixture.c" /Fo:fixture.obj /Fe:resume_test.dll /link kernel32.lib
if errorlevel 1 exit /b 1
cl /nologo /MD "$PSScriptRoot\host.c" /Fo:host.obj /Fe:hudhook_injector_smoke_$Architecture.exe /link kernel32.lib
if errorlevel 1 exit /b 1
cl /nologo /LD /MD /DNO_CALLBACK "$PSScriptRoot\fixture.c" /Fo:plain.obj /Fe:plain_test.dll /link kernel32.lib
"@
Set-Content -LiteralPath (Join-Path $taskBuild 'build.cmd') -Value $taskBuildScript -Encoding ASCII
Push-Location $taskBuild
try {
    & .\build.cmd
    if ($LASTEXITCODE -ne 0) { throw 'Fixture build failed.' }
} finally {
    Pop-Location
}
$taskName = "hudhook_injector_smoke_$Architecture.exe"
$taskHost = Start-Process -FilePath (Join-Path $taskBuild $taskName) -WindowStyle Hidden -PassThru
$taskDll = Join-Path $taskBuild 'resume_test.dll'
$taskLog = Join-Path $taskBuild 'smoke.log'
if (Test-Path -LiteralPath $taskLog) { Remove-Item -LiteralPath $taskLog }
function Invoke-Checked([string]$Dll, [string]$Callback = 'L4D2_RequestResume', [bool]$ShouldFail = $false) {
    & $Injector $taskName $Dll $Callback
    if (($LASTEXITCODE -ne 0) -ne $ShouldFail) { throw "Unexpected injector exit code: $LASTEXITCODE" }
}
try {
    $taskOtherTarget = if ($Architecture -eq 'x86') { 'x86_64-pc-windows-msvc' } else { 'i686-pc-windows-msvc' }
    $taskOtherInjector = Join-Path $taskRepo "target\$taskOtherTarget\release\examples\injector.exe"
    if (Test-Path -LiteralPath $taskOtherInjector) {
        & $taskOtherInjector $taskName $taskDll
        if ($LASTEXITCODE -eq 0) { throw 'Cross-architecture injection should have been rejected.' }
        Write-Output 'PASS: cross-architecture injection was rejected.'
    }
    Invoke-Checked $taskDll
    # Keep the original image resident, but replace its disk path with a DLL
    # without exports. A resume must still use the resident image's export.
    $taskResidentOriginal = Join-Path $taskBuild 'resident_original.dll'
    if (Test-Path -LiteralPath $taskResidentOriginal) { Remove-Item -LiteralPath $taskResidentOriginal }
    Move-Item -LiteralPath $taskDll -Destination $taskResidentOriginal
    try {
        Copy-Item -LiteralPath (Join-Path $taskBuild 'plain_test.dll') -Destination $taskDll
        Invoke-Checked $taskDll
    } finally {
        if (Test-Path -LiteralPath $taskDll) { Remove-Item -LiteralPath $taskDll }
        Move-Item -LiteralPath $taskResidentOriginal -Destination $taskDll
    }
    Invoke-Checked $taskDll
    Invoke-Checked $taskDll 'RequestStop'
    Invoke-Checked $taskDll
    Invoke-Checked $taskDll
    Invoke-Checked $taskDll 'MissingCallback'
    Invoke-Checked $taskDll 'RequestReject' $true
    Invoke-Checked $taskDll 'RequestUnload'
    $taskExpected = "ATTACH`nRESUME`nINIT`nRESUME`nALREADY_ACTIVE`nRESUME`nALREADY_ACTIVE`nSTOP`nRESUME`nINIT`nRESUME`nALREADY_ACTIVE`nUNLOAD`nDETACH`n"
    if ((Get-Content -LiteralPath $taskLog -Raw) -ne $taskExpected) {
        throw 'The callback sequence or DLL reference count did not match expectations.'
    }
    Invoke-Checked (Join-Path $taskBuild 'plain_test.dll')
    # Conversely, a resident DLL without a callback must stay without one even
    # when its disk path has been replaced by a newer DLL that exports it.
    $taskPlain = Join-Path $taskBuild 'plain_test.dll'
    $taskResidentPlain = Join-Path $taskBuild 'resident_plain.dll'
    if (Test-Path -LiteralPath $taskResidentPlain) { Remove-Item -LiteralPath $taskResidentPlain }
    Move-Item -LiteralPath $taskPlain -Destination $taskResidentPlain
    try {
        Copy-Item -LiteralPath $taskDll -Destination $taskPlain
        $taskLogBefore = Get-Content -LiteralPath $taskLog -Raw
        Invoke-Checked $taskPlain
        if ((Get-Content -LiteralPath $taskLog -Raw) -ne $taskLogBefore -or $taskHost.HasExited) {
            throw 'The resident DLL should not acquire the newer disk file callback.'
        }
    } finally {
        if (Test-Path -LiteralPath $taskPlain) { Remove-Item -LiteralPath $taskPlain }
        Move-Item -LiteralPath $taskResidentPlain -Destination $taskPlain
    }
    Invoke-Checked (Join-Path $taskBuild 'plain_test.dll')
    Write-Output "PASS ($Architecture): initial load, repeated resume, stop/resume, missing export, rejected request, single-FreeLibrary unload, DLL without callback, and resident/disk image replacement in both directions."
} finally {
    if (-not $taskHost.HasExited) { Stop-Process -Id $taskHost.Id }
    $taskHost.WaitForExit()
}
