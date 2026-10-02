<#
.SYNOPSIS
    Builds the smallest possible bufferless.exe into dist\.

.DESCRIPTION
    Uses nightly Rust to rebuild core and alloc with size optimizations and the
    immediate-abort panic strategy, links without any C runtime, then packs
    the result with UPX.
    The regular `cargo build --release` on stable is unaffected.

.PARAMETER NoUpx
    Skip UPX packing. Packed executables are more likely to be flagged by
    antivirus heuristics, so use this for builds you hand out.
#>
param([switch]$NoUpx)

$ErrorActionPreference = 'Stop'
$target = 'x86_64-pc-windows-msvc'
$root = $PSScriptRoot
$dist = Join-Path $root 'dist'
$targetDir = Join-Path $root 'target\tiny'

if (-not (rustup component list --toolchain nightly --installed | Select-String -Quiet '^rust-src')) {
    Write-Host 'Installing rust-src for nightly...'
    rustup component add rust-src --toolchain nightly
    if ($LASTEXITCODE) { throw 'Could not install rust-src (is the nightly toolchain installed?)' }
}

# On this branch the app is #![no_std] with its own entry point (see src/rt.rs),
# so nothing from the C runtime is linked at all. RUSTFLAGS replaces
# .cargo\config.toml's rustflags. location-detail/fmt-debug drop panic
# locations and Debug output.
$saved = @{
    RUSTFLAGS = $env:RUSTFLAGS
    CARGO_PROFILE_RELEASE_OPT_LEVEL = $env:CARGO_PROFILE_RELEASE_OPT_LEVEL
    BUFFERLESS_TINY = $env:BUFFERLESS_TINY
}
try {
    $env:RUSTFLAGS = @(
        '-C link-arg=/NODEFAULTLIB'
        '-C link-arg=/ENTRY:bufferless_start'
        '-Zunstable-options -Cpanic=immediate-abort'
        '-Zlocation-detail=none'
        '-Zfmt-debug=none'
        # Drop the linker's "Rich" header and build-tool metadata.
        '-C link-arg=/EMITTOOLVERSIONINFO:NO'
        '-C link-arg=/EMITPOGOPHASEINFO'
        # No relocation table. This disables ASLR for the exe: fine for a
        # personal build, but another reason not to hand this one out.
        '-C link-arg=/FIXED'
        '-C link-arg=/DYNAMICBASE:NO'
    ) -join ' '
    $env:CARGO_PROFILE_RELEASE_OPT_LEVEL = 'z'
    $env:BUFFERLESS_TINY = '1' # build.rs: minified manifest
    cargo +nightly build --release --manifest-path (Join-Path $root 'Cargo.toml') `
        --target $target --target-dir $targetDir `
        -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem,optimize_for_size
    if ($LASTEXITCODE) { throw 'cargo build failed' }
}
finally {
    foreach ($name in $saved.Keys) {
        if ($null -eq $saved[$name]) { Remove-Item "env:$name" -ErrorAction SilentlyContinue }
        else { Set-Item "env:$name" $saved[$name] }
    }
}

New-Item -ItemType Directory -Force $dist | Out-Null
$exe = Join-Path $dist 'bufferless.exe'
Copy-Item (Join-Path $targetDir "$target\release\bufferless.exe") $exe -Force
$unpacked = (Get-Item $exe).Length

if (-not $NoUpx) {
    $upx = Get-Command upx -ErrorAction SilentlyContinue
    if (-not $upx) { throw 'UPX not found. Install it with "winget install UPX.UPX", or pass -NoUpx.' }
    & $upx.Source --ultra-brute --lzma --quiet $exe | Out-Null
    if ($LASTEXITCODE) { throw 'UPX failed' }
}

$final = (Get-Item $exe).Length
Write-Host ('{0}: {1:N0} bytes' -f $exe, $final) -ForegroundColor Green
if (-not $NoUpx) { Write-Host ('  (before UPX: {0:N0} bytes, {1:P0} smaller)' -f $unpacked, (1 - $final / $unpacked)) }
