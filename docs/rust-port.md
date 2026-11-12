# Rust port, phase 1: design

Status: started 2026-11-12 (step 1). Branch `main`. The C++ decoder at tag `v0.3.0` (branch `cpp`) is the
reference: every step below has to decode the conformance set md5-identically to it and to dav1d.

## Goal and scope

Phase 1 replaces the C++ D3D12 host (`libav1/dx`, about 7,800 lines in 14 files) with Rust, and gives the
decoder a Rust API and a C ABI for embedding (the test player, later Bale). It does not change:

- the HLSL shaders (92, about 20,000 lines): same sources, same FXC, same bytecode;
- the libaom-derived entropy decoder (about 85,000 lines of C in `libav1/av1`, `aom*`): built by the Rust
  build with `cc`, called as today. Replacing it (rav1d's parser, frame-parallel) is phase 2.

Output must stay byte-identical. Speed must stay within noise of `v0.3.0` on the console.

## Approach: replace the host file by file, behind the existing C interfaces

The C parser and the C++ host talk through plain C functions and shared C structs: the parser calls
`av1_setup_frame`, `av1_mi_push_block`, the tile and frame hand-off functions, and writes mode info,
coefficients and block records straight into GPU-mapped buffers whose layouts are C structs
(`MB_MODE_INFO`, the block records `gen_pred_blocks.hlsl` reads, `types.h`). That boundary allows a port
in steps, each of which leaves a decoder that works:

1. **Build skeleton.** A Cargo workspace on `main` whose `build.rs` compiles the existing C and C++
   sources (`cc`) and links them into a Rust crate that exposes today's API unchanged. A Rust command-line
   tool, `rav1g-dec`, replaces `testing/warp_decoder` (decode IVF/OBU/WebM, per-frame md5 in the player's
   format, WARP or a hardware adapter). CI runs the conformance set with it.
   Gate: 251/251 on WARP, identical md5 to `warp_decoder`.
2. **Bindings for the shared structs.** `bindgen` on the C headers that define the shared layouts, with
   layout tests (size and offsets) in CI, so Rust and C agree on every byte the GPU reads.
3. **Leaf modules first**, one commit each, each replacing a C++ file with Rust that exports the same
   `extern "C"` functions (so the rest of the C++ and the C parser do not notice):
   memory allocator and budget guard (`av1_memory.cpp`, `vp9dx_linear_allocator.cpp`), shader library and
   pipeline states (`av1_compute.cpp`), transforms (`transform.cpp`), film grain, loop filter/CDEF/LR/superres
   recording (`loopfilters.cpp`), prediction recording (`prediction.cpp`), threads (`av1_thread.cpp`).
   Gate per step: conformance 251/251 on WARP; host checks unchanged.
4. **Frame pipeline** (`av1_core.cpp`: slots, frame buffers, output images, completion thread, fences,
   show-existing copies, reconfiguration, `-gputime` counters). The largest step; split into slots/frame
   buffers, command recording and submission, completion and output.
   Gate: conformance on WARP, then the console md5 set and `-gputime` timing against `v0.3.0`.
5. **Rust API and C ABI.** A safe `rav1g::Decoder` (create with a D3D12 device and queue or let it create
   them; decode a temporal unit; receive frames as D3D12 resources plus fences; settings for slots, memory,
   operating point, output of all layers). `cbindgen` generates `rav1g.h`; the UWP test player links the Rust
   static library through it instead of libav1. Gate: console md5 set and timing.

At the end no C++ remains in the decoder: the C parser, the HLSL and Rust.

## Layout on `main`

| Path | What |
| --- | --- |
| `crates/rav1g` | the decoder: Rust host modules, API, `capi` feature for the C ABI |
| `crates/rav1g-sys` | builds the C parser (and, until step 4 ends, the remaining C++) with `cc`; `bindgen` bindings |
| `crates/rav1g-shaders` | shader bytecode: `build.rs` runs DXC (see Requirements) and embeds the DXIL with `include_bytes!` |
| `tools/rav1g-dec` | command-line decoder for conformance, md5 and stage dumps |
| `libav1/`, `testing/` | as today; C++ files leave `libav1/dx` as steps 3-4 replace them |

Dependencies: `windows` (D3D12/DXGI bindings), `cc`, `bindgen`, `cbindgen`. Edition 2024, stable Rust.
Target `x86_64-pc-windows-msvc`, and the UWP target for the console build of the test player.

## Requirements

Agreed on 2026-11-12, in addition to the steps above:

- **Linux-native builds, no wine.** Everything except the UWP test player builds on Linux with ordinary
  tools: `cargo xwin` (clang and lld with the Windows SDK and C runtime headers and libraries that `xwin`
  fetches) for `x86_64-pc-windows-msvc`, the C parser through `cc` the same way, and the shaders with DXC's
  native Linux release. The UWP test player is C++/CX and stays an MSVC build in CI, as does the MSIX
  packaging. Running the decoder still needs D3D12 (WARP in CI, or the console).
- **Shaders compiled with DXC, not FXC.** FXC exists only for Windows. The console takes DXIL signed by DXC
  up to shader model 6.4, with waves of 64 lanes, no native 16-bit types and no `[WaveSize]`
  ([gpu-av1-decoder.md](gpu-av1-decoder.md#shader-model-6-feasibility-2026-11-12)). Baseline `cs_6_0`; the
  shaders must not assume a wave width (WARP has waves of 4). Before anything relies on it: the existing
  HLSL compiled unchanged with DXC must give md5-identical output on WARP and on the console at the same
  speed, and the Linux build of DXC must produce signed DXIL that the console accepts. The DXBC listing
  scan (`testing/dxbc_scan`) does not read DXIL; a DXIL equivalent, or a reason why the pattern cannot
  occur, comes with the switch.
- **A GPU backend boundary.** The host talks to the GPU through one internal interface (buffers, pipeline
  states, dispatch, barriers, fences, timestamps, readback). Phase 1 implements it for D3D12 only; a
  Vulkan backend (SPIR-V from the same HLSL through DXC; Linux, Android, macOS via MoltenVK, and
  conformance on Linux runners with Mesa's lavapipe) is a later phase and must not need a rewrite.

## Testing

- **Every push:** the existing workflow plus `cargo build`/`cargo test` on Windows, the conformance set
  decoded by `rav1g-dec` on WARP (md5 against dav1d/AOM), the host shader checks unchanged, the listings
  scan unchanged (the shaders do not change).
- **Equality with the C++ reference:** until step 4, CI also builds `warp_decoder` from tag `v0.3.0` and
  checks that both produce the same md5 per frame.
- **Console:** after steps 4 and 5, the md5 set (8-bit and 10-bit, hidden frames, film grain, global
  motion, layers, size changes, scaled references, superres) and `-gputime` timing against `v0.3.0`.
- **Local (optional):** `cargo xwin` cross-builds for Windows from Linux; running under wine with
  vkd3d-proton gives a third D3D12 implementation for quick checks, not a replacement for WARP in CI.

## Branches

- `main`: the port. `cpp`: C++ fixes and experiments. `original`: upstream, frozen.
- A fix to something both branches share (shaders, the C parser, the conformance set, the CI scripts) is
  made on `cpp` and merged into `main`. A fix to the C++ host after its Rust replacement exists is ported
  by hand; the conformance set catches a missed one.

## Licence and naming

The Rust code is derived from the BSD-2-Clause host and stays BSD-2-Clause with the AOMedia Patent
License 1.0 notices. The crate name `rav1g` is reserved on crates.io at the end of step 1, when the crate
contains working code; not earlier.

## Estimate

Steps 1-2: about a day. Step 3: two to three days. Step 4: one to two days. Step 5: one to two days.
About a working week to a Rust decoder that is md5-identical on WARP and the console, assuming no large
surprises in step 4.

## Decisions

1. The UWP test player stays C++ in phase 1 and links `rav1g.h`; it is test tooling.
2. `rav1g-dec` keeps the `-gputime` and `-dumpstages` diagnostics from step 1, through the same C entry
   points until step 4 moves them.
3. Minimum supported Rust version: the latest stable when step 1 lands, raised freely.
