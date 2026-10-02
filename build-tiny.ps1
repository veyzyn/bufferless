<#
.SYNOPSIS
    Builds the small variants of bufferless.exe into dist\.

.DESCRIPTION
    Builds with the `nostd` feature on nightly Rust: core and alloc are rebuilt
    with size optimizations and the immediate-abort panic strategy, and no C
    runtime is linked at all (see src/rt.rs). Produces:

      dist\bufferless-small.exe   unpacked (~107 KB)
      dist\bufferless-tiny.exe    packed with UPX (~55 KB)

    UPX-packed executables are more likely to be flagged by antivirus
    heuristics, so share the small or standard build instead. The standard
    build is just `cargo build --release` on stable.

.PARAMETER NoUpx
    Only build bufferless-small.exe.

.PARAMETER Toolchain
    Nightly toolchain to use, e.g. nightly-2026-08-23 to pin a known-good one.
#>
param(
    [switch]$NoUpx,
    [string]$Toolchain = 'nightly'
)

$ErrorActionPreference = 'Stop'
$target = 'x86_64-pc-windows-msvc'
$root = $PSScriptRoot
$dist = Join-Path $root 'dist'
$targetDir = Join-Path $root 'target\tiny'

if (-not (rustup component list --toolchain $Toolchain --installed | Select-String -Quiet '^rust-src')) {
    Write-Host "Installing rust-src for $Toolchain..."
    rustup component add rust-src --toolchain $Toolchain
    if ($LASTEXITCODE) { throw "Could not install rust-src (is the $Toolchain toolchain installed?)" }
}

# RUSTFLAGS replaces .cargo\config.toml's rustflags.
$saved = @{
    RUSTFLAGS = $env:RUSTFLAGS
    CARGO_PROFILE_RELEASE_OPT_LEVEL = $env:CARGO_PROFILE_RELEASE_OPT_LEVEL
    BUFFERLESS_TINY = $env:BUFFERLESS_TINY
}
try {
    $env:RUSTFLAGS = @(
        # No C runtime: rt.rs provides the entry point and what compiled code needs.
        '-C link-arg=/NODEFAULTLIB'
        '-C link-arg=/ENTRY:bufferless_start'
        '-Zunstable-options -Cpanic=immediate-abort'
        # Drop panic locations and Debug formatting.
        '-Zlocation-detail=none'
        '-Zfmt-debug=none'
        # Drop the linker's "Rich" header and build-tool metadata.
        '-C link-arg=/EMITTOOLVERSIONINFO:NO'
        '-C link-arg=/EMITPOGOPHASEINFO'
    ) -join ' '
    $env:CARGO_PROFILE_RELEASE_OPT_LEVEL = 'z'
    $env:BUFFERLESS_TINY = '1' # build.rs: minified manifest
    cargo "+$Toolchain" build --release --features nostd --manifest-path (Join-Path $root 'Cargo.toml') `
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
$small = Join-Path $dist 'bufferless-small.exe'
Copy-Item (Join-Path $targetDir "$target\release\bufferless.exe") $small -Force
Write-Host ('{0}: {1:N0} bytes' -f $small, (Get-Item $small).Length) -ForegroundColor Green

if (-not $NoUpx) {
    $upx = Get-Command upx -ErrorAction SilentlyContinue
    if (-not $upx) { throw 'UPX not found. Install it with "winget install UPX.UPX", or pass -NoUpx.' }
    $tiny = Join-Path $dist 'bufferless-tiny.exe'
    & $upx.Source --ultra-brute --lzma --quiet --force -o $tiny $small | Out-Null
    if ($LASTEXITCODE) { throw 'UPX failed' }
    Write-Host ('{0}: {1:N0} bytes' -f $tiny, (Get-Item $tiny).Length) -ForegroundColor Green
}
