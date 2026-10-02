# Recompiles the shaders in this folder with fxc from the Windows SDK.
# The compiled .cso files are committed, so this is only needed after
# editing an .hlsl file.

$ErrorActionPreference = 'Stop'
$fxc = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\fxc.exe" |
    Sort-Object FullName | Select-Object -Last 1
if (-not $fxc) { throw 'fxc.exe not found. Install the Windows SDK.' }

$src = Join-Path $PSScriptRoot 'cursor.hlsl'
& $fxc.FullName /nologo /O3 /Qstrip_reflect /Qstrip_debug /T vs_4_0 /E vs_main /Fo (Join-Path $PSScriptRoot 'cursor_vs.cso') $src
if ($LASTEXITCODE) { throw 'vertex shader failed to compile' }
& $fxc.FullName /nologo /O3 /Qstrip_reflect /Qstrip_debug /T ps_4_0 /E ps_main /Fo (Join-Path $PSScriptRoot 'cursor_ps.cso') $src
if ($LASTEXITCODE) { throw 'pixel shader failed to compile' }
