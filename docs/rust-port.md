# Rust port, phase 1: design

Status: started 2026-11-12; steps 1 (build skeleton, `rav1g-dec`) and 2 (bindings for the shared structs,
layout tests) are in the tree, step 3 (leaf modules) is under way, see [Progress](#progress).
Branch `main`. The C++ decoder at tag `v0.3.0` (branch `cpp`) is the reference: every step below has to
decode the conformance set md5-identically to it and to dav1d.

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
License 1.0 notices. The crate name `rav1g` is published on crates.io once the Rust API of step 5 is
usable, not earlier (decided 2026-11-12, after step 1: no placeholder release).

## Progress

### Step 1: build skeleton (2026-11-12)

What is in the tree:

| Path | What |
| --- | --- |
| `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml`, `rustfmt.toml` | the workspace: edition 2024, Rust 1.99 (the stable of that day), static C runtime as `warp_decoder` has it |
| `crates/rav1g-sys` | `build.rs` compiles the 79 C and C++ files of `libav1.vcxproj` with `cc` into static libraries and runs `bindgen` on the C interface |
| `crates/rav1g` | today's interface behind types that own what they create: `PipelineCache`, `peek_stream_info`, `Config`, `Decoder` (`decode`, `drain`, the two layer controls), and the process-wide switches of `av1_perf.h` as `unsafe` functions |
| `tools/rav1g-dec` | the command-line decoder: `warp_decoder`'s flags, lines, files and exit codes ([warp-decoder.md](warp-decoder.md#rav1g-dec)) |
| `testing/conformance/compare_md5.py` | the comparison of two conformance runs, md5 per frame and stage dumps |

How the C and C++ are built. `rav1g-sys` reads the list of sources and their per-file optimisation from
`testing/warp_decoder/libav1_sources.props`, the file `sync_sources.py` writes from `libav1.vcxproj` and CI
checks, so the three builds (the library project, `warp_decoder`, cargo) compile the same files: 71 C files,
8 C++ files, `/O1 /Ot`, `/O2` for the seven files the project marks, `/sdl`, no `NDEBUG` (the asserts of the
library stay in, as in the player), `/EHsc` for the C++, whatever the cargo profile is. One difference from
the MSBuild projects: no whole program optimisation (`/GL`), which they switch on and a cargo build cannot
use (the objects go through rustc's archives, and the Linux build links with lld).

**MSVC compiles `av1_mi_push_block` wrongly without `/GL`.** The first cargo build with MSVC (toolset
14.51.36231) decoded 2 of 251 streams right, with another md5 from run to run, while the same sources built
by MSBuild (`warp_decoder`) and by clang-cl (the Linux build) decoded all of them. Found by building
variants on a throwaway branch: everything at `/Od` passes; `prediction.cpp` alone at `/Od` passes; of its
functions only `av1_mi_push_block` matters; for that file `/O1` without `/Ot` passes and so does `/Ob0`,
while `/O1 /Ot`, `/O2`, `/Ob1`, without `/sdl`, with `/GS-`, `/Oy-` or `/d2SSAOptimizer-` fail. The stage
dumps of a good and a bad build differ first in the prediction block records of a key frame, and a print of
the block lists shows why: the wavefront iteration of the intra blocks comes out too high (23 instead of 13
iterations for the first frame of the 66x66 vector), so blocks are counted into the lists of other
iterations. The listing of the bad build (`/FAs`) has the cause: in the loop over the transform blocks of an
intra block (`for (int x = x_start; ...)`, luma) the compiler keeps `subblk_w = 1 << txw` in a stack
temporary that it loads at the top of the loop and stores only after the load; nothing initialises it before
the first iteration. The first transform block of every call therefore takes its width from whatever the
stack holds, which decides how many neighbours above it are looked at and its top right flag. The source is
well defined at that point (`const int subblk_w = 1 << txw;` in the loop body), so this is the compiler.
A second, independent analysis of the same builds came to the same temporary and the same first differing
buffer.

The fix is in the source, because the MSBuild builds compile the same loop and only happen to get it right
with `/GL`: `subblk_w` and `subblk_h`, which do not change inside the loops, are declared once per plane in
front of them (`prediction.cpp`, luma and chroma; made on `cpp`, commit 1eebffe, and merged). The settings
stay `/O1 /Ot` in every build. For one commit (eb69620) `rav1g-sys` instead compiled `prediction.cpp`
without `/Ot` under MSVC, which decodes the set right as well (run 43244241029: 250 of 251, the one being
`svc-L2T1-2`, which failed the same way in `warp_decoder` of that run and passes alone); that is gone again. To keep in mind with a new
toolset: the conformance run is what found it, and it runs for every build.

Seen on the way, and not part of the port: decoded as the eleventh stream of a process, behind the ten
8-bit vectors `av1-1-b8-00-quantizer-00` to `-09`, the 10-bit vector `av1-1-b10-00-quantizer-00.ivf` came
out wrong from its first frame, with `warp_decoder` and with `rav1g-dec` alike, with one thread and with
four, in seven of seven passes each (run 43245926770). In the order of the conformance manifest it was
right. Resolved on 2026-11-12 (dda14c7 on `cpp`, merged): `gen_pred_blocks.hlsl` read the filter-intra flag
of a block together with two bytes of struct padding behind it in the mode info record, and a decoder that
got the memory of a destroyed decoder found other bytes than 0 there. `svc-L2T1-2` behind `svc-L2T1`, the
failure taken for one of slow runners, was the same read
([gpu-av1-decoder.md](gpu-av1-decoder.md#padding-of-the-mode-info-record-2026-11-12)). The bindings of
step 2 gave the audit its ground: the layout of `MB_MODE_INFO` as the C compiler has it, and since then a
layout test (`mode_info_padding_is_where_the_shaders_mask_it`) that fails when the padding of the record
moves (bytes 93 and 114 to 115 today).

Shaders. The build takes the FXC bytecode headers the other builds take: `RAV1G_SHADER_DIR` names the
directory whose `dx/shaders/h` has them (what MSBuild gets as `/p:Rav1gPrebuiltShaders`; in CI the `normal`
output of `.github/actions/shaders`), and the build stops with a message when it is not set or has no headers.
CI checks with `shaders.py verify` that `rav1g-dec.exe` holds the bytecode of this run's headers, byte for
byte, as it does for the player and `warp_decoder`. The feature `uavsync` defines `RAV1G_UAVSYNC` for a build
from the headers of that variant.

`rav1g-dec` is Rust for what the program does itself: arguments and task lists, the D3D12 device (the `windows`
crate), the pool of readback buffers behind the output callbacks, the md5 of a frame, the comparison with a
reference, the decode loop with the change of decoder for a new sequence, the timeout, the `RESULT` lines. It
calls the C++ that the test player uses for the rest, behind a C interface of its own
(`tools/rav1g-dec/shim`): the stream readers of `testing/common` with libwebm, the summary of the timing run
(`perf_stats.h`), the stage dump files (`stage_dump.h`), the capability report and the Shader Model 6 test,
and `warp_decoder`'s crash line. `-sm6test` has its DXC shaders when the build is given
`RAV1G_SM6TEST_SHADER_DIR`.

CI (`.github/workflows/build.yml`), next to the unchanged `conformance` job with `warp_decoder`:

- `rust` (Windows, MSVC): `cargo build --release`, the shader check, `cargo test`, `-sm6test`, then the
  conformance set, the order check (`order_check.py`: streams one after the other in one process, nothing
  decoded again alone), 137 streams with `-poisonmi` and the same with `-poisonbuf`, the timing smoke test
  and the stage dumps with `rav1g-dec`. With the workflow's `experiments` input also the whole set with
  `-interfast`, with `-poisonbuf` and with both.
- `rust-equal`: `compare_md5.py` on the artifacts of `conformance` and `rust`. Every stream of the manifest
  must be in both runs with the same md5 file, byte for byte, and the stage dumps of the frames in
  `dump_tasks.txt` must compare equal.
- `shader-headers` (Windows): the headers of the Shaders step as an artifact, for the Linux build.
- `rust-cross` (Linux, `ubuntu-26.04` like the other Linux jobs): `cargo xwin build --release --target
  x86_64-pc-windows-msvc`, that is clang-cl and
  lld-link with the SDK and C runtime xwin fetches (kept in the Actions cache), and the shader check on the
  program it built.
- `rust-cross-run` (Windows): the conformance set with the program of the Linux build.

Result (run 43247143678, commit 0fde9ab; Rust 1.99.1 in both builds):

| | |
| --- | --- |
| Gate: `rav1g-dec` (cargo, MSVC) on WARP | 251 of 251 streams pass, 2.0 min |
| Gate: md5 per frame against `warp_decoder` | equal for 251 of 251 streams, 1005 frames |
| Stage dumps against `warp_decoder` | equal for the 7 streams of `dump_tasks.txt` |
| `rav1g-dec` of the Linux build on WARP | 251 of 251 on the second attempt; the first had `svc-L2T1-2` wrong behind `svc-L2T1` and right alone, then known as the failure of slow runners, which `warp_decoder` had in run 43244241029; it was the padding read fixed in dda14c7 |
| `-sm6test`, timing smoke test, stage dump script | pass with `rav1g-dec`; the Shader Model 6 lines are those of `warp_decoder` (9 pass, 5 pipeline states refused by WARP) |
| Shader check | the bytecode of this run's headers is in both programs (93 of 97 headers, the count of `warp_decoder`) |
| `cargo build --release` on the Windows runner | 70 s; the job 5.6 min |
| `cargo xwin build --release` on the Linux runner (`ubuntu-26.04`, clang 21.1.8) | 66 s; the job 1.6 min with the xwin cache (10 min for the first run, which fetched 1.2 GB) |
| Time of a run | still set by the `conformance` job (8.5 to 11.5 min); `rust-equal` adds 20 s behind it. About 11 runner minutes more per push |

The Linux build needed no change to the sources. clang-cl stops at two things MSVC compiles: the thread
function `aom_thread.h` hands to `CreateThread` returns `unsigned int` where a `DWORD` is wanted
(`-Wincompatible-function-pointer-types`), and the host takes the address of temporaries such as
`&CD3DX12_RESOURCE_BARRIER::Transition(...)` in the argument list of a call (`-Waddress-of-temporary`, 47
places). `rav1g-sys` turns both back into warnings for clang-cl only. The C++ standard library of the MSVC
runtime wants Clang 19 or newer.

### Step 2: bindings for the shared structs (2026-11-12)

`crates/rav1g-sys` has a second set of bindings, the module `shared`, from `av1/common/blockd.h` and
`mv.h`: the structs that cross a boundary.

| Struct | Bytes | Who shares it |
| --- | --- | --- |
| `MB_MODE_INFO` with `PALETTE_MODE_INFO`, `WarpedMotionParams`, `INTERINTER_COMPOUND_DATA`, `FILTER_INTRA_MODE_INFO`, `int_mv`, `MV` | 224 | the entropy decoder writes the mode info pool; `gen_pred_blocks.hlsl` and the loop filter shaders read it as `StructuredBuffer<MB_MODE_INFO>` (`dx/shaders/mode_info.h`, `ModeInfoSize`) |
| `PredictionBlock` | 16 | the prediction block records `gen_pred_blocks.hlsl` writes and the prediction shaders read |
| `InterWarpBlock` | 48 | the same for warped blocks |
| `tx_block_info_gpu` | 16 | the transform block records the entropy decoder writes for the inverse transform shaders |
| `PredictionBlockSortingInfo` | 8 | the sort keys of the block lists |
| `tx_block_info`, `av1_tile_data` | 24, 264 | entropy decoder and host: a transform block on the host side, and a tile's cursors into the buffers of a pipeline slot (coefficients, block records, block index list, block maps) |

Layout tests (`crates/rav1g-sys/src/layout_tests.rs`, run by `cargo test` in the `rust` job, on Windows with
MSVC):

- **Bindings against the C compiler of the build.** bindgen computes layouts with libclang and asserts them
  when the bindings compile, which says that Rust agrees with clang. The decoder is compiled by MSVC, so
  `build.rs` also writes a C file with `sizeof`, `__alignof` and `offsetof` of every probed struct and
  field, compiles it into the library with the compiler that builds the decoder, and writes the same table
  from the bindings; the test compares the two entry by entry. The fields are read from the bindings, so
  none is left out: 23 structs (the 13 above and 10 of the C interface that `rav1g` and `rav1g-dec` fill or
  read: `aom_codec_dec_cfg_t`, `aom_image_t`, `av1_decoded_frame_buffer_t`, `av1_perf_frame`,
  `av1_stage_dump` and others) with 245 fields.
- **The mode info record against the shaders' header.** The test reads `dx/shaders/mode_info.h`, lays its
  structs out as a structured buffer does (32-bit words one behind the other) and checks every field of the
  shader's `MB_MODE_INFO` against the C struct: same offset and size, and where the shader reads several C
  fields as one word (`block_type` is `sb_type`, `partition` and the two `ref_frame`; `tx_info` is
  `tx_size`, `skip`, `skip_mode`, `segment_id`; and so on) those fields follow each other byte by byte.
  `ModeInfoSize` must be the size of the C struct.
- **Record sizes**: the numbers above, which are also the record sizes of the stage dump format.

All three pass on the Windows runner with MSVC (run 43247143678), and in the Linux build run under wine.

Not bound: `libav1/dx/types.h`. It is the C++ host's own header (COM pointers, `std` containers, the frame
and slot structures), not a C header that bindgen could read as one, and nothing outside the host includes
it. Its records that reach the GPU are written by shaders (`InterBlock` is the 16-byte `PredictionBlock`
record) or are constant buffer structs defined inside the `.cpp` files (`GenBlockData`, `GenLfData`,
`LoopfilterData`). They get Rust definitions and tests against the C++ in step 3, with the module that
owns each.

Deviations from the design above, and why:

- **No `crates/rav1g-shaders` yet.** The shaders stay FXC bytecode headers included by `av1_compute.cpp`
  until the DXC switch, which has its own gate (console tests). The crate comes with that switch.
- **The equality check compares with `warp_decoder` of the same commit**, not of tag `v0.3.0`: until step 3
  the C++ on `main` is the reference (`v0.3.0` plus what was merged from `cpp` since), and the job that builds
  it exists. Building the reference from the tag starts when the first C++ file is replaced.
- **`bindgen` runs from step 1**, on the C interface (`aom_decoder.h`, `aom_codec.h`, `aomdx.h`,
  `av1_perf.h`): the wrapper needs those types. Step 2 adds the layouts shared with the GPU and their tests.
  Four entry points that are macros in C (`aom_codec_dec_init`, `aom_codec_control`) are functions in
  `crates/rav1g-sys/helpers.c`.
- **`rav1g-dec` still links test program C++** (see above). It leaves with the stages that replace it;
  decision 2 keeps the `-gputime` and `-dumpstages` entry points as they are until step 4.
- **Nothing is published or reserved on crates.io**; the manifests say `publish = false` until step 5
  (see Licence and naming).

### Step 3: leaf modules (2026-11-12, under way)

How a module replaces a C++ file, the same for every module of this step:

- **The cargo build leaves the file out** (`REPLACED_BY_RUST` in `crates/rav1g-sys/build.rs`) and has its
  functions in Rust, in `crates/rav1g/src/host`, one module per file. The `extern "C"` functions of the
  file are Rust functions of the same names (`#[unsafe(no_mangle)]`), so the entropy decoder and the
  programs call them as before.
- **Glue for the C++ callers.** Most of the host's functions have C++ linkage: member functions
  (`av1_memory_allocator::create_buffer`), functions the other C++ files declare themselves
  (`av1_idct_run(Av1Core *)`), functions with a printf format (`av1_log`). For those the cargo build compiles
  a file of the same name in `crates/rav1g-sys/glue`, which defines each as a call of an `extern "C"`
  function `rav1g_*` of the Rust module (the two log functions print their line first: stable Rust cannot
  define a C function with a variable argument list). The glue goes with `av1_core.cpp` in step 4.
- **The structs stay those of the C++ host until step 4**, which owns them (`Av1Core`, the data of a
  pipeline slot, the allocator inside the host pool). Rust reads and writes them in place through a third
  set of bindings, the module `host` of `rav1g-sys`: bindgen reads `host.hpp` (`dx/types.h` and the other
  headers of `libav1/dx`) as C++ with the settings of the build. A `ComPtr` is eight opaque bytes there,
  the pointer, which the `windows` crate borrows as an interface. The layout test of step 2 has a C++
  twin: `build.rs` writes `host_layout_probe.cpp` with the size and alignment of every probed struct of the
  host and the offset and size of each field (private members included), the C++ compiler of the build
  compiles it, and `cargo test` compares it with the table of the bindings
  (`host_bindings_and_cpp_compiler_agree`: 13 structs, among them `Av1Core`, `av1_frame_thread_data`,
  `av1_memory_allocator`, `compute_shader_lib`, `HwFrameBuffer`).
- **Nothing in `libav1/dx` changes.** See the next point.

**The test player on `main` during steps 3 and 4 (decision).** The UWP test player and `warp_decoder` are
MSBuild projects that compile the sources of `libav1.vcxproj`. They keep doing that until step 5: the C++
files a Rust module replaces stay in `libav1/dx`, complete and unchanged, compiled by MSBuild and not by
cargo. They are the C++ decoder of branch `cpp` (a merge of `cpp` reaches them without a conflict), not a
second line of development: a fix to the host is made on `cpp`, merged, and ported by hand to the Rust
module, as [Branches](#branches) says. The files leave the tree when the player links `rav1g.h` (step 5).

The other way, linking the Rust static library into the player now, was not taken. What stands against
it, none of it tried, because trying needs the console and this step has no console runs:

- A UWP app links the DLL C runtime (the VCLibs framework package); this workspace links the static one
  (`+crt-static`, as `warp_decoder` has it). The player would need a second build of the library and of
  the C parser with the other runtime.
- The standard library of `x86_64-pc-windows-msvc` imports Win32 functions without regard to what an app
  container offers (`ntdll`, `userenv`, `ws2_32`, `bcryptprimitives`). The target made for that,
  `x86_64-uwp-windows-msvc`, is tier 3 and needs a nightly compiler to build the standard library, which
  the design rules out.
- An import the console does not have shows only on the console.

Consequences, both deviations from the design above:

- The C++ files do not leave `libav1/dx` in steps 3 and 4 (the Layout table says they do). What step 3
  removes is their part in the cargo build; the line counts below are of that.
- The equality check keeps comparing with `warp_decoder` of the same commit, which stays the whole C++
  decoder. The design had it move to `warp_decoder` of tag `v0.3.0` with the first replaced file; with the
  C++ sources in the tree that is not needed, and the reference follows the fixes merged from `cpp`.

**The GPU backend boundary** starts in `crates/rav1g/src/gpu`: the host modules do not call D3D12
themselves, they use the types of that module (`Device`, `BufferRef`, later the pipeline layouts, the
pipelines and the recording of commands), which the one backend, `gpu/d3d12.rs`, provides. No traits: a
second backend would provide the same types and methods under the same names, chosen at build time, and a
frame's recording costs no indirect call. The interface is written in terms a backend with descriptor
sets can serve (who reads and writes a buffer instead of a heap type; binding slots in the order read
buffers, written buffers, constant blocks, inline constants instead of root parameters). Fences,
timestamp queries and readback stay in the C++ of `av1_core.cpp` and join in step 4.

A local test loop came with this step: `rav1g-dec` of the Linux build decodes under wine with wine's
D3D12 (vkd3d) on Mesa's lavapipe, the "third D3D12 implementation" of [Testing](#testing). The 244 vectors
of AOM pass there in two minutes. CI on WARP stays the gate.

| Module | C++ out of the cargo build | Rust | Glue | Notes |
| --- | --- | --- | --- | --- |
| 1. Allocator and log lines | `av1_memory.cpp` (356 lines), `vp9dx_linear_allocator.cpp` (121) | `host/log.rs` (156), `host/memory.rs` (534), `gpu/mod.rs` (93), `gpu/d3d12.rs` (227); tests included | `glue/av1_memory.cpp` (98) | see below |
| 2. Queues | `av1_thread.cpp` (151) | `host/thread.rs` (406) | `glue/av1_thread.cpp` (53) | see below |

Module 1, allocator and log lines:

- Same sizes and decisions: a device buffer is rounded up to 256 bytes, a request of the host pool to 32
  with its offset a multiple of the alignment asked for, the planning allocator counts the same way (unit
  tests hold the arithmetic against that of the C++), the descriptors are handed out in the same order, the
  heap, flags and initial state of each memory type are those of the C++, and the "MEMORY ..." lines have
  the same text.
- `-poisonmi` and `-poisonbuf` fill where the C++ filled: a buffer right behind its creation, the
  constants of a stage when they are handed out.
- `vp9dx_linear_allocator.cpp` has no Rust: nothing calls it (`la_init`, `la_allocate`, `la_free` have no
  caller in the decoder, the player or the tools). Neither has `upload_helper` of `av1_memory.cpp`, for the
  same reason.
- The budget guard (the decision of `av1_create_gpu_decoder` whether the decoder fits what the application
  has left) is in `av1_core.cpp` and moves with step 4. It decides on the numbers of this module's planning
  allocator, which are unchanged.
- A number with one decimal that lies exactly between two (0.25 MiB) prints as the lower even one from
  Rust, as from the C runtime of the MSVC build; the runtime that `xwin` fetches prints it rounded up in
  the lines the C++ still writes. Seen in one line of the Linux build (`inter_mask_lut 0.3 MiB`), not a
  difference between Rust and C++.

Module 1 on CI (run 43286946982, commit c76576f): `rav1g-dec` 251 of 251 on WARP, the order check and the
137 streams with `-poisonmi` and with `-poisonbuf` pass, md5 per frame and stage dumps equal to
`warp_decoder` of the commit (`rust-equal`), 251 of 251 with the program of the Linux build, the test player
builds; `D3D12 DEVICE REMOVALS: 0` in every pass of the three decoders. The bindings of the C++ host are
made on the Windows runner with the libclang it has, and the C++ layout probe, compiled by MSVC, agrees
with them.

Module 2, queues:

- `DataPtrQueue` (a ring of 100 pointers) and `DataPtrQueueMT` (the ring behind a lock, with a condition
  to wait on) stay the C++ structs: they sit inside `Av1Core` and the shader library, and `av1_core.cpp`
  takes the lock of some of them itself. So the lock stays what `aom_thread.h` makes of a pthread mutex and
  condition on Windows, a critical section and a condition variable inside the struct, and the Rust takes
  and wakes them with the same calls. A Rust owner of the queues (channels, `Mutex` and `Condvar`) comes
  with step 4.
- Same behaviour, including what is odd about it: the ring starts at its first slot again whenever it
  runs empty, `MTQueuePush` does not wait when the ring is full, and a pop of an empty queue does nothing.
- Unit tests: the order of the ring across its wrap, and a thread that takes 1000 items another pushes.

## Estimate

Steps 1-2: about a day. Step 3: two to three days. Step 4: one to two days. Step 5: one to two days.
About a working week to a Rust decoder that is md5-identical on WARP and the console, assuming no large
surprises in step 4.

## Decisions

1. The UWP test player stays C++ in phase 1 and links `rav1g.h`; it is test tooling.
2. `rav1g-dec` keeps the `-gputime` and `-dumpstages` diagnostics from step 1, through the same C entry
   points until step 4 moves them.
3. Minimum supported Rust version: the latest stable when step 1 lands, raised freely.
