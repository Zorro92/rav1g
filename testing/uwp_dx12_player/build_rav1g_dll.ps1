# Builds the Rust library as rav1g.dll for the test player's package (docs/test-player.md, "Building"):
# the C of the parser and entropy decoder compiled by clang-cl with full link time optimisation, linked
# with the Rust by lld-link for an app container, against the app runtime (VCLibs).
#
#   build_rav1g_dll.ps1 -Shaders <dir> -Out <dir> [-Features capi,app-container] [-TargetDir <dir>]
#
# -Shaders   the directory whose dx/shaders/h has the bytecode headers of the shaders (RAV1G_SHADER_DIR:
#            testing/shader_cache/shaders.py build --root <dir>)
# -Out       gets rav1g.dll, rav1g.dll.lib (the import library the player links) and rav1g.pdb; the player
#            is then built with /p:Rav1gRustDll=<that directory>
# -Features  the features of the rav1g crate; capi,app-container,uavsync for the uavsync shaders
#
# Needs stable Rust with the target x86_64-pc-windows-msvc, LLVM (clang-cl, lld-link, llvm-lib; libclang
# for bindgen) in "Program Files\LLVM", and Visual Studio with the UWP workload (the store libraries of
# the C runtime).
param(
  [Parameter(Mandatory = $true)][string]$Shaders,
  [Parameter(Mandatory = $true)][string]$Out,
  [string]$Features = 'capi,app-container',
  [string]$TargetDir = 'target-app-clang'
)
$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$llvm = Join-Path $env:ProgramFiles 'LLVM\bin'
foreach ($tool in @('clang-cl.exe', 'lld-link.exe', 'llvm-lib.exe')) {
  if (-not (Test-Path (Join-Path $llvm $tool))) { throw "$tool is not in $llvm" }
}
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
if (-not $vs) { throw 'vswhere found no Visual Studio' }
$tools = (Get-Content (Join-Path $vs 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt') -Raw).Trim()
$store = Join-Path $vs "VC\Tools\MSVC\$tools\lib\x64\store"
if (-not (Test-Path (Join-Path $store 'vcruntime.lib'))) { throw "no app runtime libraries in $store" }

# The DLL runtime instead of the static one of .cargo/config.toml, and the short error texts of the
# windows crates. CARGO_ENCODED_RUSTFLAGS, not RUSTFLAGS: the flags of the configuration file would
# come behind those and win.
$separator = [char]0x1f
$env:CARGO_ENCODED_RUSTFLAGS = "-Ctarget-feature=-crt-static${separator}--cfg${separator}windows_slim_errors"
$env:RAV1G_SHADER_DIR = $Shaders
$env:CC_x86_64_pc_windows_msvc = Join-Path $llvm 'clang-cl.exe'
$env:AR_x86_64_pc_windows_msvc = Join-Path $llvm 'llvm-lib.exe'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = Join-Path $llvm 'lld-link.exe'
# WindowsApp.lib first on the link: lld-link reads LINK in front of its arguments and takes a function
# from the first library that has it, so the functions of Windows come from the API sets and not from
# kernel32.dll (docs/rust-port.md, "The app container").
$env:LINK = 'WindowsApp.lib'
# What the player's stream readers (testing/common) call of the C, next to the functions of rav1g.h.
$exports = @('aom_codec_av1_dx', 'aom_codec_error', 'aom_codec_error_detail', 'aom_img_alloc', 'aom_img_free',
  'aom_img_plane_height', 'aom_img_plane_width', 'aom_read_obu_header', 'aom_uleb_decode')
$linkArgs = @('-Clink-arg=/APPCONTAINER', "-Clink-arg=/LIBPATH:$store") + @($exports | ForEach-Object { "-Clink-arg=/EXPORT:$_" })
cargo rustc --manifest-path (Join-Path $repo 'Cargo.toml') --release --locked -p rav1g --lib --features $Features `
  --crate-type cdylib --target x86_64-pc-windows-msvc --target-dir $TargetDir -- @linkArgs
if ($LASTEXITCODE -ne 0) { throw 'cargo rustc of rav1g.dll failed' }
New-Item -ItemType Directory -Force $Out | Out-Null
$built = Join-Path $TargetDir 'x86_64-pc-windows-msvc/release'
Copy-Item (Join-Path $built 'rav1g.dll'), (Join-Path $built 'rav1g.dll.lib'), (Join-Path $built 'rav1g.pdb') $Out
Get-ChildItem $Out | ForEach-Object { Write-Host "$($_.Name): $($_.Length) bytes" }
