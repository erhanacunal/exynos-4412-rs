#Requires -Version 5.1
<#
.SYNOPSIS
    Windows counterpart of the Makefile: build, package and send the workspace's applications.

.DESCRIPTION
    Same targets as `make`, run from anywhere (the script works from its own directory):

      build    cargo build --release (every application of the workspace)
      bin      build, then <app>.bin with rust-objcopy; fails if larger than bootcmd reads
      img      bin, then boot.img with mkbootimg (also kept as <app>.img)
      send     bin, then tools/httpboot.py to uart_boot over WiFi (retries -Wait seconds)
      disasm   build, then the entry code and vector table (llvm-nm / llvm-objdump)
      shaders  <app>/shaders/*.vert into <app>/src/shaders.rs (Linux only: runs through WSL)
      clean    cargo clean

    Environment variables read by build.rs (WIFI_FIRMWARE_DIR, WIFI_SSID, WIFI_PASSWORD...) work
    as with make: set them first, e.g. $env:WIFI_SSID = 'name'.

.PARAMETER Target
    One of the targets above. Default: bin.

.PARAMETER App
    The application to package (a workspace member). Default: hello_world.

.PARAMETER BoardHost
    'send': the board's address or name. Default: $env:EXYNOS_HOST, else httpboot's default (exynos).

.PARAMETER Wait
    'send': seconds to keep retrying while the board starts. Default: 30.

.PARAMETER Save
    'send': save the image to CACHE as the application to boot (httpboot --save).

.PARAMETER MkBootImg
    'img': another mkbootimg to use (Android's Python script). Default: $env:MKBOOTIMG,
    else tools/mkbootimg.py (this repository's, the same image as Android 4.3's).

.EXAMPLE
    .\make.ps1 bin
    .\make.ps1 img -App irq_timer
    .\make.ps1 img -App uart_boot -MkBootImg C:\tools\mkbootimg\mkbootimg.py
    .\make.ps1 send -App hello_world -BoardHost 192.168.88.120
    .\make.ps1 disasm -App smp_test
    .\make.ps1 shaders -App gpu_cube_test
#>
param(
    [Parameter(Position = 0)]
    [ValidateSet('build', 'bin', 'img', 'send', 'disasm', 'shaders', 'clean')]
    [string] $Target = 'bin',
    [string] $App = 'hello_world',
    [string] $BoardHost = $env:EXYNOS_HOST,
    [int] $Wait = 30,
    [switch] $Save,
    [string] $MkBootImg = $env:MKBOOTIMG
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Root = $PSScriptRoot
$Out  = Join-Path $Root 'target\armv7a-none-eabihf\release'
$Elf  = Join-Path $Out $App
$Bin  = Join-Path $Out "$App.bin"
$Img  = Join-Path $Out 'boot.img'
# bootcmd reads 0x4FC0 sectors of 512 bytes, one 4096-byte boot.img page may precede the image.
$MaxSize = 0x4FC0 * 512 - 4096

function Fail([string] $Text) { Write-Host "error: $Text" -ForegroundColor Red; exit 1 }

function Invoke-Checked([string] $Exe, [string[]] $Arguments) {
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { Fail "$([IO.Path]::GetFileName($Exe)) failed ($LASTEXITCODE)" }
}

# The toolchain's own LLVM tools (rustup component add llvm-tools), like $(TOOLS) in the Makefile.
function Get-Tool([string] $Name) {
    $sysroot = (& rustc --print sysroot).Trim()
    $hostLine = (& rustc -vV) | Where-Object { $_ -like 'host: *' }
    $hostTriple = $hostLine.Substring(6).Trim()
    $path = Join-Path $sysroot "lib\rustlib\$hostTriple\bin\$Name.exe"
    if (Test-Path $path) { return $path }
    $onPath = Get-Command $Name -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    Fail "$Name not found. Run: rustup component add llvm-tools"
}

# `python3` on Windows is often the Store stub; prefer python, then the py launcher.
function Get-Python {
    foreach ($candidate in 'python', 'py') {
        $cmd = Get-Command $candidate -ErrorAction SilentlyContinue
        if ($cmd -and $cmd.Source -notlike '*WindowsApps*') {
            if ($candidate -eq 'py') { return @($cmd.Source, '-3') }
            return @($cmd.Source)
        }
    }
    Fail 'Python 3 not found on PATH.'
}

function Invoke-Python([string[]] $Arguments) {
    $py = @(Get-Python)  # @(): a one-element array would come back as a string
    $exe = $py[0]
    $all = @($py | Select-Object -Skip 1) + $Arguments
    Invoke-Checked $exe $all
}

function Step-Build {
    Push-Location $Root
    try { Invoke-Checked 'cargo' @('build', '--release') }
    finally { Pop-Location }
    if (-not (Test-Path $Elf)) { Fail "no ELF for '$App': $Elf (is it a workspace member?)" }
}

function Step-Bin {
    Step-Build
    Invoke-Checked (Get-Tool 'rust-objcopy') @('-O', 'binary', $Elf, $Bin)
    $size = (Get-Item $Bin).Length
    Write-Host "${Bin}: $size bytes"
    if ($size -gt $MaxSize) { Fail 'larger than bootcmd reads' }
}

function Step-Img {
    Step-Bin
    if (-not $MkBootImg) { $MkBootImg = Join-Path $Root 'tools\mkbootimg.py' }
    if (-not (Test-Path $MkBootImg)) { Fail "mkbootimg not found: $MkBootImg" }
    # Same invocation as ~/prepare.sh: same pagesize, no ramdisk.
    Invoke-Python @($MkBootImg, '--kernel', $Bin, '--ramdisk', 'NONE',
        '--cmdline', 'console=ttySAC2,115200', '--pagesize', '4096', '-o', $Img)
    Write-Host "${Img}: $((Get-Item $Img).Length) bytes"
    # Kept as <app>.img too, which the next `img` of another application leaves alone.
    Copy-Item $Img (Join-Path $Out "$App.img") -Force
}

function Step-Send {
    Step-Bin
    $toolArgs = @((Join-Path $Root 'tools\httpboot.py'))
    if ($BoardHost) { $toolArgs += @('--host', $BoardHost) }
    if ($Wait -gt 0) { $toolArgs += @('--wait', "$Wait") }
    if ($Save) { $toolArgs += '--save' }
    $toolArgs += $Bin
    Invoke-Python $toolArgs
}

function Show-Section([string[]] $Lines, [string] $StartPattern) {
    $on = $false
    foreach ($line in $Lines) {
        if (-not $on -and $line -cmatch $StartPattern) { $on = $true }
        if ($on) { $line }
    }
}

function Step-Disasm {
    Step-Build
    $nm = Get-Tool 'llvm-nm'
    $objdump = Get-Tool 'llvm-objdump'
    $dis = @('-d', '--no-show-raw-insn')

    & $nm $Elf | Where-Object { $_ -cmatch ' (_start|__exynos_vectors|__image_end|__bss_start|__stacks_end)$' }

    Write-Host ''; Write-Host '--- _start: zImage header, the data words at 0x24-0x33 are not code'
    Show-Section (& $objdump @dis '--disassemble-symbols=_start' $Elf) '<_start>:'

    Write-Host ''; Write-Host '--- entry code up to main'
    & $objdump @dis '--disassemble-symbols=__exynos_boot,__exynos_relocated' $Elf |
        Where-Object { $_ -cmatch '^[0-9a-f]+( <|:)' }

    Write-Host ''; Write-Host '--- vector table'
    Show-Section (& $objdump @dis '--disassemble-symbols=__exynos_vectors' $Elf) '<__exynos_vectors>:'
}

function Step-Shaders {
    # shaderc runs Mesa's lima under drm-shim: Linux only (tools/shaderc/setup.sh once, in WSL).
    $verts = @(Get-ChildItem (Join-Path $Root "$App\shaders") -Filter '*.vert' -ErrorAction SilentlyContinue |
        ForEach-Object { "$App/shaders/$($_.Name)" })
    if ($verts.Count -eq 0) { Fail "no $App/shaders/*.vert" }
    if (-not (Get-Command wsl -ErrorAction SilentlyContinue)) {
        Fail 'shaders needs Linux: install WSL (and run tools/shaderc/setup.sh there once), or run `make shaders` on Linux.'
    }
    Push-Location $Root
    try {
        Invoke-Checked 'wsl' (@('python3', 'tools/shaderc/shaderc.py', '-o', "$App/src/shaders.rs") + $verts)
    }
    finally { Pop-Location }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Fail 'cargo not found. Install Rust via rustup.' }

switch ($Target) {
    'build'   { Step-Build }
    'bin'     { Step-Bin }
    'img'     { Step-Img }
    'send'    { Step-Send }
    'disasm'  { Step-Disasm }
    'shaders' { Step-Shaders }
    'clean'   { Push-Location $Root; try { Invoke-Checked 'cargo' @('clean') } finally { Pop-Location } }
}
