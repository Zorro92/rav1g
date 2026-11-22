# Builds the Rust library as rav1g.dll for the test player's package (docs/test-player.md, "Building"):
# linked by lld-link for an app container, against the app runtime (VCLibs). The library has no C
# (docs/rust-port.md, "No C in the library"); clang-cl is named to cargo for the build scripts that ask.
#
#   build_rav1g_dll.ps1 -Shaders <dir> -Out <dir> [-Features capi,app-container] [-TargetDir <dir>]
#
# -Shaders   the directory whose dx/shaders/dxil has the bytecode of the shaders (RAV1G_SHADER_DIR:
#            testing/shader_cache/shaders.py build --dxc <dxc> --root <dir>)
# -Out       gets rav1g.dll, rav1g.dll.lib (the import library the player links) and rav1g.pdb; the player
#            is then built with /p:Rav1gRustDll=<that directory>
# -Features  capi,app-container, plus uavsync or shader-pick for the corresponding experiment
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
# x86-64-v3 (AVX2, BMI1/2, LZCNT, MOVBE, FMA): the console's CPU (Zen 2) has all of it. The serial entropy
# decode of a tile runs on one core (AGENTS.md, CPU), so its codegen is what sets the speed of 1-tile 4K.
$env:CARGO_ENCODED_RUSTFLAGS = "-Ctarget-feature=-crt-static${separator}--cfg${separator}windows_slim_errors${separator}-Ctarget-cpu=x86-64-v3"
$env:RAV1G_SHADER_DIR = $Shaders
# One unit and whole-program LTO for the DLL of the package only: inlining across rav1d, the sink and the host
# on the serial parse path (AGENTS.md, CPU). The other release builds keep the default profile.
$env:CARGO_PROFILE_RELEASE_LTO = 'fat'
$env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = '1'
$env:CC_x86_64_pc_windows_msvc = Join-Path $llvm 'clang-cl.exe'
$env:AR_x86_64_pc_windows_msvc = Join-Path $llvm 'llvm-lib.exe'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = Join-Path $llvm 'lld-link.exe'
# WindowsApp.lib first on the link: lld-link reads LINK in front of its arguments and takes a function
# from the first library that has it, so the functions of Windows come from the API sets and not from
# kernel32.dll (docs/rust-port.md, "The app container").
$env:LINK = 'WindowsApp.lib'
# The DLL exports the functions of rav1g.h and nothing else. (It used to export nine functions of libav1 for
# the player's stream readers; the library has no C of libav1 any more, and the readers of testing/common
# stand by themselves: docs/phase2-rav1d.md, step 6.)
$linkArgs = @('-Clink-arg=/APPCONTAINER', "-Clink-arg=/LIBPATH:$store")
cargo rustc --manifest-path (Join-Path $repo 'Cargo.toml') --release --locked -p rav1g --lib --features $Features `
  --crate-type cdylib --target x86_64-pc-windows-msvc --target-dir $TargetDir -- @linkArgs
if ($LASTEXITCODE -ne 0) { throw 'cargo rustc of rav1g.dll failed' }
New-Item -ItemType Directory -Force $Out | Out-Null
$built = Join-Path $TargetDir 'x86_64-pc-windows-msvc/release'
Copy-Item (Join-Path $built 'rav1g.dll'), (Join-Path $built 'rav1g.dll.lib'), (Join-Path $built 'rav1g.pdb') $Out
Get-ChildItem $Out | ForEach-Object { Write-Host "$($_.Name): $($_.Length) bytes" }
