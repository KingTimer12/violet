# Builds Violet for Windows x64 and packs it:
#
#   scripts/package-windows.ps1 <version> <out-dir>
#
# writes into <out-dir>, each with its .sha256, named as the updater
# (fastframe-update) looks for them:
#
# - violet-<version>-x86_64-pc-windows-msvc.zip: the portable copy, with
#   the marker that lets it replace itself;
# - violet-<version>-x86_64-pc-windows-msvc.msi: the Windows Installer
#   package (packaging/windows/violet.wxs), per user, which updates by
#   installing the next release's package.
#
# <version> is the tag (v0.1.7) or dev-<commit>; a dev build's package is
# numbered 0.0.0, as Windows Installer only takes numbers. Needs the WiX
# Toolset 5 and its Util extension:
#
#   dotnet tool install --global wix --version 5.0.2
#   wix extension add --global WixToolset.Util.wixext/5.0.2
#
# Pass -SkipBuild to pack target/release/violet.exe as it is.
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $SkipBuild) {
    cargo build --release --locked --bin violet
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
$exe = Join-Path $root 'target/release/violet.exe'
if (-not (Test-Path $exe)) { throw "$exe is missing" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$out = (Resolve-Path $OutDir).Path
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("violet-package-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null

function Write-Checksum([string]$path) {
    $hash = (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
    $name = Split-Path -Leaf $path
    "$hash  $name" | Out-File -Encoding ascii "$path.sha256"
}

try {
    $name = "violet-$Version-x86_64-pc-windows-msvc"

    # The portable archive: the marker lets a copy from it replace itself.
    $dir = Join-Path $work $name
    New-Item -ItemType Directory -Path $dir | Out-Null
    Copy-Item $exe $dir
    Set-Content -NoNewline -Encoding ascii -Path (Join-Path $dir 'violet-portable.txt') -Value "violet-portable-v1`n"
    Copy-Item README.md $dir
    $zip = Join-Path $out "$name.zip"
    Remove-Item -Force -ErrorAction SilentlyContinue $zip
    Compress-Archive -Path $dir -DestinationPath $zip
    Write-Checksum $zip

    # The installer package, with its own marker instead.
    $marker = Join-Path $work 'violet-installer.txt'
    Set-Content -NoNewline -Encoding ascii -Path $marker -Value "violet-installer-v1`n"
    $number = $Version.TrimStart('v')
    if ($number -notmatch '^\d+\.\d+\.\d+$') { $number = '0.0.0' }
    $msi = Join-Path $out "$name.msi"
    wix build -arch x64 -ext WixToolset.Util.wixext `
        -d "Version=$number" -d "Executable=$exe" -d "Marker=$marker" `
        -d "Icon=$(Join-Path $root 'assets/app/violet.ico')" `
        -o $msi (Join-Path $root 'packaging/windows/violet.wxs')
    if ($LASTEXITCODE -ne 0) { throw "wix build failed" }
    # wix leaves the debug symbols beside the package.
    Remove-Item -Force -ErrorAction SilentlyContinue ([System.IO.Path]::ChangeExtension($msi, '.wixpdb'))
    Write-Checksum $msi
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $work
}

Get-ChildItem $out
