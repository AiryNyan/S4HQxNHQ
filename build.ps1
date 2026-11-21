# Builds both DLLs and stages ONLY the files to copy to a game folder into ..\dist:
#   dist\배포용\Game\Bin\d3d11.dll      normal build (quiet log)
#   dist\로그수집용\Game\Bin\d3d11.dll  same code with detailed logging always on
# Each mirrors the game folder layout (…\Game\Bin -> <The Sims 4>\Game\Bin).
# -Dist <folder>: stage somewhere else (e.g. a separate test build); relative to this script.
param([string]$Dist = '..\dist')
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$def = (Join-Path $PSScriptRoot 'd3d11.def') -replace '\\', '/'
$dist = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot $Dist)).TrimEnd('\')
# The staging folder is deleted first: refuse this source folder, anything
# inside it or above it, and any folder that holds sources.
$src = $PSScriptRoot.TrimEnd('\')
$ic = [StringComparison]::OrdinalIgnoreCase
if ($src.Equals($dist, $ic) -or $src.StartsWith($dist + '\', $ic) -or $dist.StartsWith($src + '\', $ic) -or
    (Test-Path (Join-Path $dist '.git')) -or (Test-Path (Join-Path $dist 'Cargo.toml')) -or (Test-Path (Join-Path $dist 'probe'))) {
    throw "refusing to use '$dist' as the output folder: it would delete sources"
}
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist -Confirm:$false }

# File properties (name, version, copyright): version.rc compiled with the
# toolchain's windres. The version comes from VERSION in src\lib.rs.
if ((Get-Content 'src\lib.rs' -Raw) -notmatch 'const VERSION: &str = "(\d+)\.(\d+)\.(\d+)"') { throw 'VERSION (x.y.z) not found in src\lib.rs' }
$major = $Matches[1]; $minor = $Matches[2]; $patch = $Matches[3]
$windres = (Get-Command 'x86_64-w64-mingw32-windres.exe' -ErrorAction SilentlyContinue).Source
if (-not $windres) {
    $cfg = Get-Content (Join-Path $env:USERPROFILE '.cargo\config.toml') -Raw
    if ($cfg -match 'linker = "([^"]+)/[^"/]+"') { $windres = Join-Path $Matches[1] 'x86_64-w64-mingw32-windres.exe' }
}
if (-not $windres -or -not (Test-Path $windres)) { throw 'windres not found' }
New-Item -ItemType Directory -Force 'target' | Out-Null

foreach ($v in @(@{ name = '로그수집용'; key = 'diag'; args = '--features diag'; def = '-DDIAG' }, @{ name = '배포용'; key = 'normal'; args = ''; def = '' })) {
    $res = (Join-Path $PSScriptRoot "target\version$($v.def).o") -replace '\\', '/'
    Set-Content -Path 'target\ver.h' -Encoding ascii -Value @("#define VER_MAJOR $major", "#define VER_MINOR $minor", "#define VER_PATCH $patch", "#define VER_STR `"$major.$minor.$patch`"")
    $wargs = @('-c', '65001', '-O', 'coff')
    if ($v.def) { $wargs += $v.def }
    & $windres @wargs 'version.rc' '-o' $res
    if ($LASTEXITCODE -ne 0) { throw "windres failed: $($v.name)" }
    # Each variant has its own target folder: both link to the same file name
    # (d3d11.dll), so a shared folder could hand out the other variant when
    # cargo finds nothing to rebuild. The DLL is deleted first so it is always
    # linked again (cargo does not notice a changed version.o).
    $tdir = "target\variant-$($v.key)"
    $rel = "$tdir\x86_64-pc-windows-gnullvm\release"
    $built = "$rel\d3d11.dll"
    Remove-Item -Force -ErrorAction SilentlyContinue $built, "$rel\deps\d3d11.dll"
    cmd /c "cargo rustc --lib --release --offline --target-dir $tdir $($v.args) -- -C link-arg=`"$def`" -C link-arg=`"$res`" 2>&1" | Select-String -Pattern '^(error|warning)|Finished' -Context 0,8
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $built)) { throw "build failed: $($v.name)" }
    $out = Join-Path $dist "$($v.name)\Game\Bin"
    New-Item -ItemType Directory -Force $out | Out-Null
    Copy-Item $built $out
}
# Optional static files (not in the public repository): dist_static\* is copied as is.
if (Test-Path (Join-Path $PSScriptRoot 'dist_static')) { Copy-Item -Recurse (Join-Path $PSScriptRoot 'dist_static\*') $dist }
$root = (Resolve-Path $dist).Path
Get-ChildItem -Recurse -File $dist | ForEach-Object { '{0,9}  {1}' -f $_.Length, $_.FullName.Substring($root.Length + 1) }
