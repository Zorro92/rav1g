# Rust port, phase 1: design

Status: started 2026-11-12; steps 1 (build skeleton, `rav1g-dec`), 2 (bindings for the shared structs,
layout tests), 3 (leaf modules) and 4 (frame pipeline: the whole host is Rust in the cargo build, which
compiles no C++ of the decoder any more) are in the tree, and of step 5 the Rust interface and the
interface for C (`rav1g.h`); see [Progress](#progress). Left of step 5: the UWP test player on the Rust
library.
Branch `main`. The C++ decoder at tag `v0.3.0` (branch `cpp`) is the reference: every step below has to
decode the conformance set md5-identically to it and to dav1d.

## Goal and scope

Phase 1 replaces the C++ D3D12 host (`libav1/dx`, about 7,800 lines in 14 files) with Rust, and gives the
decoder a Rust API and a C ABI for embedding (the test player, later Bale). It does not change:

- the HLSL shaders (92, about 20,000 lines): same sources, same FXC, same bytecode;
- the libaom-derived entropy decoder (about 85,000 lines of C in `libav1/av1`, `aom*`): built by the Rust
  build with `cc`, called as today. Replacing it (rav1d's parser, frame-parallel) is phase 2.

Output must stay byte-identical. Speed must stay within noise of `v0.3.0` on the console.

The project's goals, in order (2026-11-12): heavy 4K at 30 fps in real time (at most 33.3 ms per frame),
then faster still, then efficiency: decoding while leaving as much of the CPU, GPU and memory as possible
to the application. Speed work is judged on 4K first; a change that costs a little below 4K is acceptable
while 1080p60 and 1440p60 keep clear headroom (16.7 ms per frame). Phase 1 only has to keep the speed of
`v0.3.0`; reaching 4K30 is the work of phase 2 (frame-parallel parsing) and of the GPU stages, and once it
is reached, frame threads and memory are sized to the target rather than to the maximum.

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
| `crates/rav1g` | the decoder: Rust host modules, API, `capi` feature for the C ABI (`include/rav1g.h`) |
| `crates/rav1g-sys` | builds the C parser with `cc` (with it three C files of its own: the macros of the C interface as functions, the host functions that raise the decoder's error, the shader bytecode table); `bindgen` bindings |
| `crates/rav1g-shaders` | shader bytecode: `build.rs` runs DXC (see Requirements) and embeds the DXIL with `include_bytes!` |
| `tools/rav1g-dec` | command-line decoder for conformance, md5 and stage dumps |
| `tools/rav1g-header` | writes `rav1g.h` from `crates/rav1g/src/capi.rs` with `cbindgen`, or checks it |
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
the MSBuild projects at this step: no whole program optimisation (`/GL`), which they switch on. The build
got it later: [The entropy decoder with link time optimisation](#the-entropy-decoder-with-link-time-optimisation-2026-11-12).

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

### Step 3: leaf modules (2026-11-12)

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
| 3. Shader library, constant tables | `av1_compute.cpp` (759) | `host/compute.rs` (729), `host/tables.rs` (167); `gpu/mod.rs` and `gpu/d3d12.rs` grow to 114 and 509 | `glue/av1_compute.cpp` (231, of which 93 includes and 93 table lines) | see below |
| 4. Inverse transform stage | `transform.cpp` (141) | `host/transform.rs` (159), `host/core.rs` (123); `gpu/d3d12.rs` grows to 669 | `glue/transform.cpp` (18), `glue/av1_core_calls.cpp` (22) | see below |
| 5. Loop filter, CDEF, superres, loop restoration, film grain, output copy | `loopfilters.cpp` (1,226) | `host/loopfilter.rs` (303), `host/postfilter.rs` (587), `host/film_grain.rs` (448), `host/output.rs` (327); `host/core.rs` grows to 236, `gpu/d3d12.rs` to 743, `gpu/mod.rs` to 152 | `glue/loopfilters.cpp` (42), `glue/av1_core_calls.cpp` grows to 37 | see below |
| 6. Prediction | `prediction.cpp` (1,590) | `host/blocks.rs` (1,125), `host/prediction.rs` (1,285), `host/intra_v2.rs` (366); `host/tables.rs` grows to 363, `host/core.rs` to 270 | `glue/prediction.cpp` (29), `glue/av1_core_calls.cpp` grows to 45 | see below |

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

Module 2 on CI (run 43289148532, commit 8991bd0): the same gates as module 1, all met; `D3D12 DEVICE
REMOVALS: 0` in every pass.

Module 3, shader library and constant tables:

- **The library** is still the C++ struct `compute_shader_lib`, because the frame pipeline reads its
  pipeline states and root signatures. Rust creates it, zeroed as its constructor left it, as the first
  field of a value of its own that also holds the threads; the handle of
  `av1_create_pipeline_cache_handle` points to that value, and Rust releases its 97 pipeline fields and 16
  layouts when the handle is destroyed. The 16 layouts and the 93 pipelines are created in the order of the
  C++, so the number in the log line of a pipeline that fails is the same.
- **The bytecode** is in the FXC headers, which a C++ file has to include: the glue does and hands them
  over as a table by name. The program holds the same 93 of the 97 headers as before (CI checks the
  bytes).
- **The threads** are `std::thread`s at the priority the C++ gave them, and take their tasks by a shared
  counter, not from a `DataPtrQueueMT`. With that the fallback of the C++ is gone (pipelines on the
  calling thread when tasks and threads did not fit the queue of 100 entries; 93 and at most 6 did). A
  thread count below 0 counts as 0 (the C++ started no thread and created nothing). A creation that fails
  releases what it made (the C++ leaked the library).
- **The constant tables** (`av1_upload_luts`: random register of the film grain for luma and chroma, the
  Gaussian sequence, the wedge and inter-intra masks, the warp filter) are computed by the same
  arithmetic; unit tests hold the random register against the C++ function as it was written. The staging
  buffers are zeroed before they are filled. The C++ left the bytes it did not write as the heap gave
  them: behind the Gaussian sequence, and behind chroma masks smaller than 64 bytes.
- **Tables of the C and C++ sources** that Rust needs a copy of are in `host/tables.rs`, each with a test
  against its source: `block_size_wide`, `block_size_high` and `ss_size_lookup`, which are `static` in a
  header, against the bytes the C compiler of the build has (`rav1g_sys::c_table`, a table probe next to
  the layout probe); the Gaussian sequence against the text of `av1_compute.cpp`, for as long as that file
  is in the tree. Tables that are `extern` in the C headers (`warped_filter`, `wedge_params_lookup`,
  `ii_weights1d`, `ii_size_scales`) are read where they are.
- **The backend boundary** got the pipeline layouts (`LayoutDesc`: read buffers, written buffers,
  constant blocks, inline constants), the compute pipelines, and `Device::upload`, the blocking copy of
  constant data into buffers.

Module 3 on CI (run 43292284538, commit 6b764d1, with the merge of `cpp` 4b92364 under it): the same
gates, all met; `D3D12 DEVICE REMOVALS: 0` in every pass.

Module 4, the inverse transform stage (`av1_idct_run`), the first stage that records commands:

- **Order of the suggested list changed:** film grain is not a file of its own. `av1_filmgrain_run` is
  part of `loopfilters.cpp` and is called by the output copy of the same file, so it moves with module 5,
  as a Rust module of its own. Module 4 is `transform.cpp` alone.
- **The recording of commands** joins the backend boundary: `Commands` (the layout and the pipeline of
  the next dispatches, buffers and constant blocks at the binding slots, inline constants, dispatch, the
  barrier behind dispatches that wrote a buffer the next ones read). A place in a buffer is a
  `BufferLocation`, in D3D12 the GPU virtual address a root descriptor takes. The calls on the command
  list are those of the C++, one for one and in its order.
- **`host/core.rs`** has what the stages share: the command list, buffers, pipelines and layouts of the
  C++ structs as the types of the boundary, and the constants of a stage
  (`ComputeCommandBuffer::Alloc`: the next piece of the frame's upload memory, 256-byte steps, the fill of
  `-poisonbuf` when it is handed out). A stage writes the fields of a record one by one through a
  pointer, as the C++ did, and no others: what a record holds besides is part of what `-dumpstages`
  compares between the two decoders.
- **Calls into `av1_core.cpp`** (the timestamp behind a stage, the stage dumps) go the other way through
  `glue/av1_core_calls.cpp`, which gives those C++ functions `extern "C"` names.
- Tables: `tx_size_wide` and `tx_size_high` are copies with a test against the header; the scan orders
  (`av1_idct_scans`) are read where they are.

Module 4 on CI (run 43294200425, commit 6018f42): the same gates, all met (251 of 251, md5 of 1005 frames
and the stage dumps of 7 streams equal to `warp_decoder`); `D3D12 DEVICE REMOVALS: 0` in every pass.

Module 5, everything of `loopfilters.cpp`, as four Rust modules:

- `host/loopfilter.rs`: the deblocking filter (`av1_loopfilter_gpu`: the two shaders that list the edges,
  the vertical edges in two passes, the horizontal ones in one, or split by `-lfhsplit`).
- `host/postfilter.rs`: CDEF (`-cdefv1` or the default shader), superres, loop restoration, and
  `av1_cdef_looprestoration`, which orders them and moves the frame between the slot's frame buffer, the
  frame's own buffer and the back buffer.
- `host/film_grain.rs`: the grain parameters, the scaling functions and the random offsets of the grain
  blocks, and the three dispatches of the synthesis.
- `host/output.rs`: the output image and the copy into it (`av1_postprocess_copy_output`).

What is kept, call for call: the order of the dispatches and of the barriers, the fields written into
every record of constants (and no others), the lists the CPU writes for CDEF and loop restoration, the
layouts `-dumpstages` writes. In particular:

- **The unit fields of loop restoration** (25c85ca): a plane without restoration still gets a unit grid
  of one unit and the offset 0 in its record, so that no reader computes an address from what an earlier
  frame left there. The comment that says why went along.
- **Film grain copies its parameters from one record into the next** by reading them back from the upload
  memory, bytes the stage did not write included, as the C++ did (`data->params = gen_data->params`). The
  second record is part of what `-dumpstages` compares, so it has to hold the same bytes.
- **The output of a frame is two Rust functions with the glue between them.** When the application gives
  no output image the C++ raised the decoder's error with `aom_internal_error`, which leaves the decode
  call with a `longjmp` when the caller has set one up. A `longjmp` must not pass a Rust function, so the
  glue raises the error, between the call that asks for the image and the call that records the copy.
  When the error returns, the frame goes on without an image, as before.
- Unit tests hold the scaling function and the random register of film grain and the layout of the output
  image against the C++ arithmetic as it was written.
- The backend boundary got state transitions, copies between buffers, and the copy of a plane from a
  buffer into a texture of the application (`TextureRef`, `TextureSource`).

Checked before the push on the local decoder (wine, vkd3d, lavapipe): the 244 vectors of AOM plain, with
`-poisonbuf`, with `-cdefv1`, with `-lfhsplit 1` and `-lfhsplit 2`; and two synthetic streams with frame
size changes and with superres, 64 frames each, md5 equal to the C++ build of CI.

Module 5 on CI (run 43298091378, commit 3592709): the same gates, all met (251 of 251, md5 of 1005 frames
and the stage dumps of 7 streams equal to `warp_decoder`, the constants and lists of the stages among
them); `D3D12 DEVICE REMOVALS: 0` in every pass.

Module 6, everything of `prediction.cpp`, as three Rust modules:

- `host/blocks.rs`: `av1_mi_push_block`, which the tile threads call for every block the entropy decoder
  has decoded: the prediction blocks of the block counted into the lists of its tile, their places in the
  lists written behind the block's index base, the wavefront iteration of every intra transform block, the
  work counters of `-gputimedetail`. It is an `extern "C"` function of the entropy decoder's interface, so
  the C calls the Rust without glue.
- `host/prediction.rs`: the shader that writes the block records (`av1_prediction_gen_blocks`: the counts
  of the lists turned into where each list starts, the tile with the most iterations moved to the front),
  the passes of inter prediction with the lists of `-interfast`, the reconstruction, intra prediction along
  its wavefront or by the schedule of `-intrav2` (`av1_prediction_run_all`), the borders
  (`av1_inter_ext_borders`), and the dispatch counters of the inter breakdown.
- `host/intra_v2.rs`: the schedule of `-intrav2` (`intra_v2_place`, `intra_v2_sort`,
  `intra_v2_next_dispatch` of `dx/intra_v2_sched.h`, which stays for the host check
  `testing/intra_check`).

What is kept: the lists and the order of every index a block writes, the dispatches with their inline
constants and the barriers between the passes, the pipeline set only when it changes, the fields of the
records. In particular:

- **The loop the MSVC miscompilation hit** (1eebffe): the width and height of a transform block in 4x4
  units are computed once per plane, in front of the loops over the transform blocks, in luma and in
  chroma. In Rust that is where they would be written anyway; the comment says what happened to the C++.
- The wavefront step that luma and chroma share (the highest iteration of the units a block reads, plus
  one; the block's iteration written along its right and bottom edge) is one function, inlined into both.
- The inline constants of the reconstruction have five words the shader does not read. The C++ passed
  what its stack held there; Rust passes zeros.
- Inline functions of the C headers that the hot path needs are ported, not called through a wrapper
  (`is_interintra_pred`, `scale_chroma_bsize`, `av1_get_tx_size` with `av1_get_max_uv_txsize`,
  `av1_extract_interp_filter`, `get_ref_frame_buf`); `has_top_right` and `has_bottom_left`, which are
  functions of the entropy decoder, are called as before.

Tests that came with it (21 unit tests in `rav1g` now, run by `cargo test` on Windows with MSVC):

- **Function probes.** The C layout probe has the ported inline functions as real functions
  (`rav1g_sys::probe`), and the tests call both sides over all block sizes, modes and references.
- **The schedule of `-intrav2` against the C++ header.** The C++ probe has the three functions of
  `intra_v2_sched.h`; a test places the same few thousand blocks through the Rust port and through the
  header on frames of one and of several regions and compares the cell maps, the region lists, the sorted
  lists and every dispatch cut from them.
- **Tables.** 16 more tables that are `static` in the C headers (block and transform size tables, the
  subpel filters, the distance weights, the smooth weights) and the five of `av1_compute.h` (the list
  place of a block size, the overlapped prediction mask, the intra mode parameters) are Rust copies, each
  compared byte for byte with what the C or C++ compiler of the build has. The copies were written by a
  program that includes the headers, not by hand.
- The distance weights of compound prediction and the relative distance of order hints against the C++
  arithmetic as it was written.

Checked before the push on the local decoder: the 244 vectors of AOM plain, with `-interfast`,
`-interfastlists`, `-intrav2`, `-poisonbuf`, `-gputimedetail` and with four threads. `-intraopt` is no
check there: with the optimised intra shader that D3D12 implementation gives other frames in every run,
with the C++ build as with the Rust one (three runs of each on one stream, six different results).

Module 6 on CI (run 43304735545, commit 63a9e5f, with the merge of `cpp` 3b469c0 under it): the same gates,
all met: `rav1g-dec` 251 of 251 on WARP, the order check, 137 of 137 with `-poisonmi` and with
`-poisonbuf`, md5 of 1005 frames and the stage dumps of 7 streams equal to `warp_decoder` of the commit,
251 of 251 with the program of the Linux build, the timing smoke test with its counter checks, the test
player builds; `D3D12 DEVICE REMOVALS: 0` in every pass of the three decoders. `-interfast` runs with
`warp_decoder` in CI (251 of 251) and ran with `rav1g-dec` on the local decoder.

**CPU time of the recording, Rust against C++** (run 43307000147 of a throwaway branch: three programs
built from one checkout on one Windows runner, each stream decoded five times by each in turn with
`-gputime -t 4` on WARP; microseconds per frame, the median frame of a run, the lowest of the five runs).
`warp_decoder` is the MSBuild build with whole program optimisation; "C++ host" is `rav1g-dec` of the
commit before step 3 (a3719fc), the cargo build with the host in C++; "Rust host" is `rav1g-dec` of
63a9e5f.

| Timer | 2160p 8-bit, 20 frames | | | 1080p 10-bit, 16 frames | | | 720p 8-bit, 40 frames | | |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| | `warp_decoder` | C++ host | Rust host | `warp_decoder` | C++ host | Rust host | `warp_decoder` | C++ host | Rust host |
| record (all stages) | 43 | 44 | 42 | 264 | 265 | 219 | 160 | 157 | 134 |
| rec_gen_blocks | 29 | 30 | 28 | 30 | 30 | 29 | 27 | 28 | 28 |
| rec_idct | 6 | 5 | 6 | 21 | 20 | 21 | 15 | 13 | 14 |
| rec_pred | 8 | 8 | 7 | 37 | 36 | 37 | 33 | 32 | 31 |
| rec_loopfilter | 9 | 9 | 9 | 9 | 9 | 9 | 9 | 9 | 8 |
| rec_cdef_lr | 1 | 1 | 1 | 163 | 163 | 119 | 80 | 79 | 55 |
| rec_borders | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| rec_copy | 7 | 6 | 7 | 19 | 18 | 13 | 6 | 5 | 6 |
| entropy_sum (tile threads, with `av1_mi_push_block`) | 2922 | 3295 | 3310 | 6865 | 7327 | 7378 | 4264 | 4580 | 4571 |

- The recording in Rust costs what it cost in C++, stage by stage, and less where the CPU writes lists
  for a stage (CDEF and loop restoration: 119 against 163 microseconds at 1080p). The tile threads, which
  call `av1_mi_push_block` for every block, take the same time with the Rust function as with the C++ one
  (+0.5 %, +0.7 %, -0.2 %; the runs of one program differ by more).
- The frame of the 2160p stream is intra coded with few blocks, which is why its recording is short; a
  first run with 8 frames of it and means instead of medians (run 43304936153) had a single frame of
  several milliseconds in most runs of all three programs and said nothing.
- **Not from this step, but it shows here:** the cargo build of the entropy decoder is 7 to 13 % slower in
  the tile threads than the MSBuild build (`entropy_sum`: 3295 against 2922, 7327 against 6865, 4580
  against 4264), with the C++ host as with the Rust host. The C is compiled without whole program
  optimisation (`/GL`), which the MSBuild projects switch on and cargo cannot use. The test player links
  the MSBuild library today; when it links the Rust static library (step 5) this is what the console
  timing against `v0.3.0` will see, unless the C gets link time optimisation another way (clang-cl with
  LTO, or the entropy decoder stays an MSBuild library). Done since:
  [The entropy decoder with link time optimisation](#the-entropy-decoder-with-link-time-optimisation-2026-11-12).
- The console timing of the recording counters is the main session's.

Step 3 in numbers: 4,344 lines of C++ in 7 files left the cargo build (`av1_core.cpp`, 2,453 lines, is
what it still compiles of the host); the Rust host is 7,092 lines in 15 files of `crates/rav1g/src/host`
and 895 in `crates/rav1g/src/gpu`, tests and comments included; 516 lines of glue in 7 files stand
between the two until step 4.

**What step 4 inherits.**

- `av1_core.cpp` is the frame pipeline and everything that owns something: the creation of a decoder
  with the budget guard and the plan of its buffers, the pipeline slots and the pool of frame buffers,
  `av1_setup_frame` and the other functions the entropy decoder calls (`dx/av1_core.h`), the command buffer
  of a frame from prepare to commit, the completion thread and its fences, the output images and
  show-existing copies, the timestamps and CPU timers of `-gputime`, the stage dumps, `-asyncclear`.
- With it go the glue (`crates/rav1g-sys/glue`), the C++ bindings (`rav1g_sys::host`, `host.hpp`) and the
  C++ probes; the structs the stages read through pointers today (`Av1Core`, `av1_frame_thread_data`,
  `ComputeCommandBuffer`, the allocator, the queues, the shader library) become Rust types that own their
  buffers, pipelines and locks, and the `unsafe` of the stage functions shrinks to the calls of the C
  entropy decoder and of D3D12. `av1_tile_data` and the mode info stay C structs: the entropy decoder
  writes them.
- The backend boundary still lacks what `av1_core.cpp` does with D3D12 itself: the device and queues,
  command allocators and lists, fences, timestamp queries, readback and the copies of the stage dumps.
- The functions that write a record of constants field by field can take a zeroed record once both
  decoders are no longer compared stage dump by stage dump, or once the comparison leaves the unwritten
  bytes out.
- `aom_internal_error` leaves the decode call with a `longjmp`. The functions of step 4 that the entropy
  decoder calls and that raise it (`av1_setup_frame`: a frame the decoder was not created for, no frame
  buffer) need the same care as the output copy got in module 5: the error is raised from C, not from
  under a Rust frame.

### The entropy decoder with link time optimisation (2026-11-12)

The cargo build compiles the C of the entropy decoder (and the C++ that is left) for link time
optimisation, as the MSBuild projects do with `/GL` and `/LTCG` (commit 1face0c):

- **MSVC** (the build on Windows): `/GL` for every file, `/LTCG` at the link of `rav1g-dec` and of the
  tests (`/LTCG:incremental` since 2e890b2, which is what the MSBuild projects link with and makes faster
  code of the entropy decoder: [tile decode time at the end of step 4](#tile-decode-time-at-the-end-of-step-4)).
- **clang-cl** (the build on Linux, `cargo xwin`): `-flto` (full LTO; `lld-link` compiles the bitcode).
- `RAV1G_C_LTO=off` builds without, `thin` with ThinLTO under clang-cl: for measurements.

Why the first attempt with `/GL` ended in LNK1120 (unresolved externals): cargo copies a static library
of a build script into the `.rlib` of its crate, and rustc writes the symbol table of that archive
itself. It cannot read an object of `/GL`, which is the compiler's intermediate language and not COFF, so
the table had none of the C functions and the linker did not find them; `/LTCG` at the link changes
nothing about that. The libraries now go to the linker as `lib.exe` (or `llvm-lib`) wrote them
(`cargo:rustc-link-lib=static:-bundle=...` with the output directory as a search path), for both
compilers: what rustc reads of clang's bitcode would depend on its LLVM being as new as clang's.
`link.exe` restarts with `/LTCG` when it meets a `/GL` object, so a link without the flag works too (the
tests of the `rav1g` crate link that way); the two build scripts pass it.

The source fix for the MSVC miscompilation of `av1_mi_push_block` (1eebffe) is not touched by this: that
function is Rust in the cargo build, and the set decodes right with `/GL` (251 of 251, below).

**Tile decode time** (`entropy_sum` of `-gputime -t 4` on WARP, run 43312178597 of a throwaway branch: six
programs from one checkout on one Windows runner, the three clang-cl programs built on a Linux runner of
the same run; each stream decoded five times by each program in turn; microseconds per frame, the median
frame of a run; first number the lowest of the five runs, in brackets the mean of the five; the percentage
is of the lowest against `warp_decoder`):

| Program | 2160p 8-bit, 20 frames | 1080p 10-bit, 16 frames | 720p 8-bit, 40 frames |
| --- | ---: | ---: | ---: |
| `warp_decoder` (MSBuild, `/GL`) | 3068 (3169) | 6855 (6986) | 4308 (4366) |
| cargo, MSVC, no LTO (before) | 3436 (3508), +12.0 % | 7360 (7528), +7.4 % | 4664 (4724), +8.3 % |
| cargo, MSVC, `/GL` and `/LTCG` | 3138 (3229), +2.3 % | 7091 (7154), +3.4 % | 4484 (4497), +4.1 % |
| cargo, clang-cl, no LTO (before) | 3250 (3334), +5.9 % | 6860 (7024), +0.1 % | 4337 (4390), +0.7 % |
| cargo, clang-cl, ThinLTO | 3157 (3204), +2.9 % | 6893 (6987), +0.6 % | 4321 (4383), +0.3 % |
| cargo, clang-cl, full LTO | 3006 (3067), -2.0 % | 6607 (6763), -3.6 % | 4128 (4194), -4.2 % |

- **MSVC:** `/GL` takes the gap from 7 to 12 % down to 2 to 4 % (2 to 3 % by the means). The rest has the
  same sign on all three streams, so it is not noise, though the runs of one program differ by 2 to 3 %.
  What the MSBuild program has and this one cannot: its whole program optimisation covers the host too,
  where the function the tile threads call for every block (`av1_mi_push_block`) was C++ and is Rust
  here, behind a boundary the optimiser does not cross. Not measured: how much of the rest that is.
- **clang-cl:** without LTO it was already at the MSBuild program's speed on two of the three streams; with
  full LTO it is 2 to 4 % faster than the MSBuild program on all three. ThinLTO gains little over none.
  Full LTO is the default there: the C is 71 files and the link of the program takes a few seconds.
- Conformance with the `/GL` program and with the ThinLTO program in that run: 251 of 251 each; the
  layout and table tests pass against the `/GL` build of the C. The full LTO program runs the set in the
  `rust-cross-run` job of every push.
- For step 5 (the player links the Rust library and the C): objects of `/GL` are what `link.exe` of the
  player's build takes (with the 2 to 4 % above); bitcode of clang needs `lld-link` for the link of the
  player; a parser library built by MSBuild stays possible. The console timing decides.

### Step 4: frame pipeline (2026-11-12)

`av1_core.cpp` (2,453 lines) is one file, so the cargo build cannot leave it out part by part the way
step 3 left out files. For the parts of this step it compiles `crates/rav1g-sys/glue/av1_core_rest.cpp`
instead: the text of `av1_core.cpp` without what Rust has, with the Rust called through `extern "C"`
functions. `libav1/dx/av1_core.cpp` itself stays whole and unchanged for the MSBuild projects. The rest
file shrinks with every part and goes with the last; so do the glue, the C++ bindings and the C++ probes.

The parts, each green on CI before the next:

| Part | Out of the C++ rest | Rust | Notes |
| --- | --- | --- | --- |
| 1. Creation and release | the buffer plan (`av1_allocate_buffers`), the memory query, the budget guard and `av1_create_gpu_decoder`, the "MEMORY" summary, `av1_allocate_pbi`, the pool of frame buffers, `av1_destroy_gpu_decoder` (826 lines) | `host/create.rs` (1,424 with tests); `gpu/d3d12.rs` grows to 937 | see below |
| 2. Timers and stage dumps | the timers, timestamps and the record of a frame of the timing run (`-gputime`, `-gputimedetail`), the switches `av1_set_perf_callback`, `av1_set_perf_detail`, `av1_set_inter_fast`; the stage dumps (`-dumpstages`) (505 lines) | `host/perf.rs` (507), `host/stage_dump.rs` (536), `host/rest_calls.rs` (126, until part 4); `host/core.rs` 242; `gpu/d3d12.rs` grows to 1,054 | see below |
| 3. A frame on the decode thread | `av1_setup_frame` with the slot, the clears of `-asyncclear`, the back buffer, the references, the palette buffer and the coefficient layout; the other functions the entropy decoder calls (`av1_setup_macroblockd`, `av1_setup_sec_data`, `av1_setup_ext_coef_buffer`, `av1_intra_palette`, `av1_reallocate_frame_buffer`, `av1_release_fb_callback`, `av1_sequence_fits_decoder`, `av1_setup_context_buffers`); the command buffer from prepare to commit; `av1_decode_frame_gpu`; the switches `av1_set_async_clear`, `av1_set_intra_v2`, `av1_shader_variant` (825 lines) | `host/frame.rs` (1,341 with tests), `crates/rav1g-sys/host_entry.c` (47); `gpu/d3d12.rs` grows to 1,106 | see below |
| 4. Behind the submission | the completion thread with the fences, the return of frame buffers and slots, the clear of `-asyncclear`, the output images and their queue (`get_output_frame`), `av1_drain_gpu_decoder`, `av1_decode_sef` (show-existing copies) (297 lines) | `host/completion.rs` (341); `host_entry.c` grows to 58 | see below |
| 5. The structs | the glue file with the shader bytecode (222 lines of C++), the C++ bindings (`host.hpp`), the C++ layout, table and function probes | `host/types.rs` (509), `crates/rav1g-sys/shader_table.c` (217), `parser.h` and the module `rav1g_sys::parser` | see below |

Part 1, creation and release:

- **Same plan, same decisions, same lines.** The requests of the buffer plan are made in the order of
  the C++ with its integer arithmetic, once for the plan that only counts and once for the allocator
  (`CHECK_RESULT` is the macro `request!`). The budget guard asks the application's callback, waits for
  memory the system may still be returning in steps of 20 ms for as long as the answer grows (2 s at
  most, 300 ms without a gain of 1 MiB), and refuses or accepts with the text of the C++. The lines that
  are put together in a buffer of 640 bytes in the C++ (`snprintf`) are cut where it cut them.
- **The struct is still the C++ one** (`Av1Core` in the host pool of the application, zeroed). The C++
  constructor of the allocator is called through the rest file as long as C++ calls its virtual functions;
  what the destructor of `Av1Core` released (the interfaces of the slots and of the output contexts, the
  command list, the queue and the device, in that order) is released by hand.
- **The backend boundary** got what the pipeline owns: the device of a decoder that is given none (the
  first hardware adapter), the direct queue, command allocators, the command list, fences and their
  events.
- **A check of the lines in CI.** `testing/conformance/memory_lines.py` decodes a few streams in ten decoder
  configurations (sizes, bit depths, slots, low memory parts, the buffers of `-intrav2` and `-interfast`)
  with `-v` and keeps the decoder's "MEMORY" lines; the `conformance` job does it with `warp_decoder`, the
  `rust` job with `rav1g-dec`, and `rust-equal` wants the two sets equal line for line. Left out are the
  lines about the adapter's video memory, which differ from run to run, and one number: the size of the
  host pool, which counts from the first address of the application's memory that is a multiple of 256
  and so depends on where the program's allocator put that memory (32 bytes less in `rav1g-dec` than in
  `warp_decoder`; what is in use of the pool is the same and is compared).
- **`rav1g-dec -budget <MiB>`** gives the decoder a budget callback with a fixed answer, so the guard can be
  run on a desktop (`warp_decoder` has no such flag: its decoder is created without a callback).
- Tests: the cut of a line as `snprintf` cuts it, the slots and frame buffer counts of a configuration,
  the guard margin against the header.

Part 1 on CI (run 43319362547, commit 3358766): `rav1g-dec` 251 of 251 on WARP, the order check, 137 of
137 with `-poisonmi` and with `-poisonbuf`, md5 of 1005 frames and the stage dumps of 7 streams equal to
`warp_decoder` of the commit, 251 of 251 with the program of the Linux build; `D3D12 DEVICE REMOVALS: 0` in
every pass of the three decoders. The run is red for one thing, the new comparison of the "MEMORY" lines:
all ten cases were equal but for the size of the host pool named above, which the comparison leaves out
since the next commit.

**The budget guard against the C++** (run 43319488920 of a throwaway branch, where `warp_decoder` got the
same `-budget` flag): four decoder configurations (1080p 10-bit with 2 and with 4 slots, 2160p 8-bit, 16x16)
with eleven budgets each from 0 to 4000 MiB, 44 creations by each program. Every decision is the same,
with the same lines: refused after the wait of 300 ms in which the answer did not grow ("no longer
growing"), with the `MEMORY REFUSED` line and the task ending in an error; accepted with the same sum of
device buffers and margin; and the decoder that was then created has the same buffers.

Part 2, timers and stage dumps:

- **Ahead of the frame setup and the completion thread**, which use both: with these two in Rust the
  later parts call Rust, and the C++ rest calls them through `host/rest_calls.rs`, a module of `extern "C"`
  names that goes with the rest file. The calls of the stages into `av1_core.cpp`
  (`glue/av1_core_calls.cpp`) are gone: `host/core.rs` calls the Rust.
- **Timers.** The CPU timers of the frame that is being built are counters the tile threads add to as
  well; Rust uses them as atomics in place. The callback and the two switches are atomics. A frame's
  record is put together as before: the snapshot of the CPU timers taken at submit, the timestamps read
  back behind the frame's fence, each difference counted to its stage, and to its section and dispatch of
  the inter breakdown.
- **Stage dumps.** The callback and the frame list are behind a lock (the C++ had four globals that the
  decode thread and the completion thread read without one). The text of a layout is a `CString`.
- **The backend boundary** got timestamp queries (the heap, a timestamp at a point of the list, the
  resolve into a readback buffer, the frequency of a queue) and readback buffers (creation, the copy
  destination, a read of the first bytes).
- Checked before the push on the local decoder: the 244 vectors; the stage dumps of the first four frames
  of seven vectors (key and inter frames, 10-bit, film grain, two layers, a size change, intra-only
  frames, a show-existing frame) written by the build before this part and by this one, equal by
  `stage_dump.py compare`; the lines of `-gputime`, `-gputimedetail` and `-gputimeisolate -interfast`, the
  same lines with other numbers.

Part 2 on CI (run 43321473278, commit 07e4930): all jobs green: `rav1g-dec` 251 of 251, the order check,
137 of 137 with `-poisonmi` and with `-poisonbuf`, md5 of 1005 frames and the stage dumps of 7 streams
equal to `warp_decoder`, the "MEMORY" lines of the ten configurations equal, the timing smoke test with
its counter checks on the Rust timers, 251 of 251 with the program of the Linux build; `D3D12 DEVICE
REMOVALS: 0` in every pass.

Part 3, a frame on the decode thread:

- **`aom_internal_error` is raised by C.** Two functions of this part can end with the decoder's error,
  which leaves the decode call with a `longjmp` when the caller has set one up: `av1_setup_frame` (no
  palette buffer; no room for the coefficients of a frame with very small tiles) and `av1_decode_frame_gpu`
  (no output image). A `longjmp` must not pass a Rust function, so both are C functions now
  (`crates/rav1g-sys/host_entry.c`) that call the Rust in parts (`rav1g_setup_frame_begin`, `_coefs`, `_end`;
  `rav1g_decode_frame_record`, `_submit`) and raise the error between two parts. When the error returns
  (no jump set up) the next part runs, as the C++ went on behind the call. The glue that did this for the
  output copy alone is only used by `show_existing_frame` now, which is C++ until part 4.
- **The functions the tile threads call** (`av1_setup_macroblockd` once per tile, `av1_intra_palette` per
  palette block, the two that switch a tile to the share of a second slot) are `extern "C"` Rust
  functions the C calls directly, with the integer arithmetic of the C++ for every cursor a tile gets.
- **The frame setup** keeps its order: the slot, the clears (or the reason to skip one with `-asyncclear`),
  the schedule of `-intrav2`, the back buffer, the counts of the frame's buffer and of its references
  under the lock of the pool, then the palette buffer with its budget check and its "MEMORY" lines.
- **The command buffer**: reset of the slot's allocator and of the list, the constants from the start of
  the slot's upload memory; at commit the resolve of the timestamps, close, execute, the signal of the
  slot's fence and its event, and the snapshot of the CPU timers. The backend boundary got those calls
  (`CommandAllocator::reset`, `CommandList::reset` and `close_quietly`, `Queue::execute` and `signal`,
  `Fence::set_event`).
- The allocator no longer needs its C++ constructor: no C++ calls its virtual functions any more.
- Tests: the stamp of the intra schedule, a scaled reference, and the constants that the bindings do not
  carry over against the headers (`MAX_MB_PLANE`, `REF_INVALID_SCALE`, `TRANSLATION`,
  `YV12_FLAG_HIGHBITDEPTH`, the guard margin of a buffer).
- Checked before the push on the local decoder: the 244 vectors plain, with `-poisonbuf -t 4` and with
  `-intrav2 -interfast -asyncclear -t 4`; the stage dumps of seven vectors equal to those of the build
  before part 2; the unit tests of the crate under wine.

Part 3 on CI (run 43323944247, commit 76d8862): all jobs green, the same gates as part 2; `D3D12 DEVICE
REMOVALS: 0` in every pass.

Part 4, behind the submission:

- **The completion thread** is a `std::thread` (the C++ had the `pthread_create` of `aom_thread.h`, a
  `CreateThread`); its handle is kept in the decoder, and the drain pushes the item that ends the thread
  and joins it. What the thread does per work item is unchanged, in its order: the wait for the frame's
  fence, the stage dumps, the record of the timing run, the frame buffers back to the pool under its lock
  with one wake of a waiting decode thread, the second slot and then the slot back to their pools (the
  clear of `-asyncclear` in between), the output image to the application's notification or to the queue
  that `get_output_frame` reads, the timing callback, the work item back to its pool.
- **`av1_decode_sef`** is C that calls two Rust parts, like `av1_decode_frame_gpu`: the application is
  asked for the output image in the first, the error for a missing image is raised by the C, and the copy
  is recorded and submitted in the second.
- **No C++ is left of the host in the cargo build** but one file of glue, `glue/av1_compute.cpp`, which is
  the table of the shader bytecode (the FXC headers are C++ only in that they need a typedef). The rest
  file, `host/rest_calls.rs` and four glue files are gone.
- Checked before the push on the local decoder: the 244 vectors plain, with `-poisonbuf -t 4`, with
  `-intrav2 -interfast -asyncclear -t 4 -slots 4` and with `-gputimedetail -slots 3 -poisonmi`; the stage
  dumps of seven vectors (a show-existing frame among them) equal to those of the build before part 2; the
  lines of the three timing modes; a refused creation (`-budget 5`), which releases a decoder that never
  had a completion thread; the unit tests of both crates under wine.

Part 4 on CI (run 43325739898, commit a53221e): all jobs green, the same gates; `D3D12 DEVICE REMOVALS:
0` in every pass.

Part 5, the structs:

- **The structs of the host are Rust's own** (`host/types.rs`): the decoder, a pipeline slot, the command
  buffer of a slot, the allocator with its buffer descriptors, the shader library, the queues, the
  schedule of `-intrav2`, the records of the timing run and of the stage dumps, and the numbers of the
  block lists. They keep the names and the fields they had, so the stages read them as before. A field
  that was a COM pointer is the interface or the backend type that wraps it, or none (`Option`); the
  locks and conditions are those of Windows that `aom_thread.h` made of pthread ones.
- **Same layouts, on purpose** (`repr(C)`, and an assertion on the sizes of the decoder and of the
  allocator, 67,384 and 7,296 bytes). Both lie in the host pool of the application, whose size the memory
  query reports: with the sizes of the C++ structs a configuration takes the same host memory from both
  decoders, an application's pool fits either, and the "MEMORY" lines of the two stay equal (CI holds
  them against each other). And a decoder is created zeroed, which is a valid value of every field.
- **What the entropy decoder shares with the host stays C and is bound as C**: `parser.h` and the module
  `rav1g_sys::parser` (bindgen in C mode) have `AV1Decoder`, `AV1_COMMON`, `MACROBLOCKD`, the state of a
  tile thread, the frame buffer descriptions (`YV12_BUFFER_CONFIG`, `HwFrameBuffer`, `HwOutputImage`), the
  tile's cursors, the enums and limits of the bitstream and the tables the host reads. The layout probe of
  step 2 covers them: 11 more structs, 34 in all with 619 fields, each size, alignment and offset as the
  C compiler of the build has it. The decoder is a pointer the entropy decoder never looks behind
  (`struct Av1Core *`).
- **No C++ in the library.** The table of the shader bytecode is C (`shader_table.c`: the FXC headers need
  one typedef). The build script compiles C only; the C++ files of `libav1/dx` are skipped by name, and a
  C++ file of the project that is not one of them stops the build.
- **Gone with the C++:** `rav1g_sys::host` and `host.hpp`, the C++ layout probe of 13 structs, the probe of
  the `-intrav2` scheduler and its test against `dx/intra_v2_sched.h` (the header is C++; the Rust port is
  held by the conformance runs with `-intrav2`), the C++ table probe. The five tables of `dx/av1_compute.h`
  are still tested against the header, by reading its text, as the Gaussian sequence is.
- **Left as it was:** the queues are still the ring of 100 pointers behind a critical section, and the
  inter breakdown and the tile counters of the timing run are still memory of the C runtime. `extern "C"`
  functions named `rav1g_*` that only Rust calls now (the allocator, the queues, the stages) keep that
  form. All three are for the clean-up that comes with the Rust API of step 5, when `Decoder` owns the
  state.
- Checked before the push on the local decoder: the unit tests of the three crates under wine (29, 4 and
  6 tests; the layout test with 34 structs and 619 fields), the 244 vectors plain and with the three
  flag sets of part 4, the stage dumps of seven vectors equal to those of the build before part 2, the
  "MEMORY" lines of five streams as before.

Part 5 on CI (run 43328398581, commit 97722ee): all jobs green: `rav1g-dec` 251 of 251 on WARP, the order
check, 137 of 137 with `-poisonmi` and with `-poisonbuf`, md5 of 1005 frames and the stage dumps of 7
streams equal to `warp_decoder`, the "MEMORY" lines of the ten configurations equal, the timing smoke
test, 251 of 251 with the program of the Linux build, the unit tests (29, 4 and 6; the layout test with
MSVC as the C compiler); `D3D12 DEVICE REMOVALS: 0` in every pass of the three decoders. The same commit
with the workflow's `experiments` input (run 43328491590): the whole set again with `rav1g-dec` and
`-interfast`, with `-poisonbuf` and with both, 251 of 251 each, 0 device removals.

Step 4 in numbers: `av1_core.cpp` (2,453 lines) is `host/create.rs`, `frame.rs`, `completion.rs`,
`perf.rs`, `stage_dump.rs` and `types.rs` (4,676 lines with tests and comments) and 59 lines of C
(`host_entry.c`). The Rust host is 11,590 lines in 21 files of `crates/rav1g/src/host` and 1,267 in
`crates/rav1g/src/gpu`. The 516 lines of glue, the C++ bindings and the C++ probes of step 3 are gone.

#### CPU time of the recording at the end of step 4

Run 43328488394 of a throwaway branch: `warp_decoder` (MSBuild) and `rav1g-dec` of 97722ee from one
checkout on one Windows runner, `rav1g-dec` of the Linux build (clang-cl) of the same run; each stream
decoded five times by each program in turn with `-gputime -t 4` on WARP; microseconds per frame, the
median frame of a run, the lowest of the five runs. 2160p first: it is the size that is not fast enough
yet, and what speed is judged by.

| Timer | 2160p 8-bit, 20 frames | | | 1080p 10-bit, 16 frames | | | 720p 8-bit, 40 frames | | |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| | `warp_decoder` | Rust, MSVC | Rust, clang-cl | `warp_decoder` | Rust, MSVC | Rust, clang-cl | `warp_decoder` | Rust, MSVC | Rust, clang-cl |
| record (all stages) | 44 | 42 | 42 | 272 | 228 | 230 | 162 | 142 | 140 |
| rec_gen_blocks | 28 | 28 | 28 | 30 | 31 | 31 | 30 | 30 | 30 |
| rec_idct | 6 | 6 | 6 | 21 | 22 | 23 | 16 | 16 | 17 |
| rec_pred | 8 | 7 | 7 | 37 | 38 | 37 | 33 | 32 | 32 |
| rec_loopfilter | 9 | 9 | 8 | 9 | 9 | 10 | 9 | 9 | 9 |
| rec_cdef_lr | 1 | 1 | 1 | 165 | 120 | 128 | 81 | 61 | 58 |
| rec_borders | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| rec_copy | 6 | 7 | 7 | 20 | 17 | 17 | 7 | 7 | 7 |
| submit | 270 | 271 | 262 | 979 | 984 | 985 | 864 | 875 | 864 |
| setup (with the wait for a slot) | 259,057 | 259,125 | 263,938 | 807,431 | 807,312 | 811,144 | 64 | 64 | 66 |
| memset (the clears of a frame's setup) | 396 | 393 | 401 | 118 | 118 | 117 | 61 | 60 | 61 |
| frame (decode thread, setup to submit) | 269,249 | 270,037 | 269,594 | 810,810 | 810,820 | 814,628 | 2,785 | 2,930 | 2,793 |

- The frame pipeline in Rust costs what it cost in C++: the recording as after step 3 (less where the CPU
  writes lists), the submit, the clears and the setup the same. On WARP the setup of a 2160p or 1080p
  frame is the wait for a pipeline slot, that is for the GPU work of the frame two before it, in all
  three programs.
- The decode thread of the 720p stream takes 5 % longer in the MSVC program (2,930 against 2,785): it
  decodes tiles itself, and that is the tile decode time of the next section.

#### Tile decode time at the end of step 4

`entropy_sum` (the time of the tile threads, with the block lists) is where the two builds of the Rust
decoder differ from each other and, the MSVC one, from `warp_decoder`. Three runs of throwaway branches,
each with all its programs on one Windows runner and seven rounds; microseconds per frame, the median
frame of a run, the lowest of the seven runs, in brackets the mean of the seven; the 2160p stream was
decoded twice per run, at the start and at the end.

**1. Step 3 against step 4** (run 43330704655; `rav1g-dec` of 1face0c, the build before step 4 with the
same link time optimisation, and of 97722ee):

| Stream | `warp_decoder` | Rust, MSVC, before step 4 | Rust, MSVC, step 4 | Rust, clang-cl, step 4 |
| --- | ---: | ---: | ---: | ---: |
| 2160p, first pass | 1710 (1790) | 1726 (1818), +0.9 % | 1932 (2022), +13.0 % | 1752 (1830), +2.5 % |
| 2160p, second pass | 1651 (1690) | 1685 (1721), +2.1 % | 1841 (1915), +11.5 % | 1635 (1682), -1.0 % |
| 1080p 10-bit | 4132 (4718) | 4337 (4699), +5.0 % | 4483 (4846), +8.5 % | 4130 (4468), 0.0 % |
| 720p | 2623 (2724) | 2671 (2815), +1.8 % | 2832 (2948), +8.0 % | 2549 (2643), -2.8 % |

The MSVC program of step 4 was 9 to 12 % slower on the 2160p stream than the one before step 4 on that
runner (3 % at 1080p, 6 % at 720p), and the clang-cl program was not.

**2. Part by part** (run 43333372366, another runner, a slower one; `rav1g-dec` of each part of step 4,
MSVC):

| Stream | `warp_decoder` | before step 4 | part 1 | part 2 | part 3 | part 4 | part 5 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2160p, first pass | 3149 | 3302 | 3357 | 3284 | 3425 | 3265 | 3417 |
| 2160p, second pass | 3174 | 3312 | 3433 | 3247 | 3376 | 3325 | 3413 |
| 720p | 4353 | 4578 | 4647 | 4520 | 4495 | 4493 | 4519 |

On this runner step 4 is 3 % slower than the build before it on the 2160p stream and 1 % faster at
720p, and no part stands out: the parts lie within 5 % of each other without an order (part 2 and part 4
are faster than the build before step 4, part 1 and part 3 slower). So the difference of the first run
is not the cost of one piece of Rust: it comes and goes with the machine and with small changes of the
program.

**3. What it is, as far as it is known** (the analysis is GPT 6.1 Sol's, from the two programs of run
43328491590 and from its own runs 43332697462 and 43334799404):

- It is not a call that got slower. `warp_decoder` calls `av1_mi_push_block` out of line as well; the
  function the tile threads call for every block is the same instruction stream in the build before step
  4 and in step 4; all 60 C files of the entropy decoder are compiled at the link in both; both clear with
  the `memset` of the C runtime. The large clear of a tile's block map (7.5 MB at 2160p) happens once, in
  the first decoded frame, not per frame.
- 85 to 90 % of the gap at 2160p is in the C of the entropy decoder itself (parsing and tile setup 2229
  against 2100 microseconds with timers added for the purpose; the loop over the block lists 143 against
  131). And MSVC made other code of that C in the cargo link than in the MSBuild link: `decode_partition`
  5144 bytes against 2532, the symbol decoder `od_ec_decode_cdf_q15` 694 against 590, its standard variant
  520 against 300.
- **The link mode.** The MSBuild projects link with `/LTCG:incremental`, cargo linked with `/LTCG`; the
  code generator of the link is not the same in the two modes. With `/LTCG:incremental` the three
  functions have the sizes and the instructions of `warp_decoder`.

**4. The link variants side by side** (run 43337492684; `head` is 97722ee):

| Stream | `warp_decoder` | Rust, MSVC, `/LTCG` (head) | Rust, MSVC, `/LTCG:incremental` | the same with Rust `lto = "fat"`, one codegen unit | Rust, clang-cl |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2160p, first pass | 2957 (3039) | 3198 (3254), +8.2 % | 3065 (3133), +3.7 % | 3184 (3298), +7.7 % | 2956 (3080), 0.0 % |
| 2160p, second pass | 2954 (3004) | 3163 (3229), +7.1 % | 3064 (3146), +3.7 % | 3167 (3208), +7.2 % | 2925 (3004), -1.0 % |
| 1080p 10-bit | 6830 (6882) | 7057 (7173), +3.3 % | 6967 (7071), +2.0 % | 6939 (6989), +1.6 % | 6662 (6745), -2.5 % |
| 720p | 4257 (4311) | 4403 (4450), +3.4 % | 4346 (4383), +2.1 % | 4330 (4382), +1.7 % | 4161 (4226), -2.3 % |

- **`/LTCG:incremental` is adopted** (commit 2e890b2; on CI in run 43341601469 with every gate of part
  5 met and 0 device removals): 3 to 4 % less tile time at 2160p (3065 against 3198, 3064 against 3163),
  1 % less at 1080p and 720p. The link leaves `rav1g_dec.iobj` (7.4 MB) and
  `rav1g_dec.ipdb` (1.2 MB) in the output directory of the program's build script under `target`; a build
  after a change of the program's Rust relinks in 3 s. `RAV1G_C_LTO=off` still builds without.
- **Rust `lto = "fat"` with one codegen unit is not adopted.** On top of the incremental link it takes
  the gain at 2160p back (3184 and 3167: where the plain link was), gives 0.4 % at 1080p and 720p, and
  makes the recording slower (all stages 242 against 218 microseconds at 1080p, CDEF and loop restoration
  lists 134 against 113). In run 43332697462, on the plain link and with timers added, it had been the
  fastest at 2160p; next to the incremental link it is not.
- **clang-cl** (the Linux build, full LTO of the C) is at the speed of `warp_decoder` at 2160p and 2 %
  faster at the other two sizes, in every run. It is the build to look at for the player's library in
  step 5.

**5. What is left, and not explained.** With the incremental link the MSVC program is still 3.7 % slower
than `warp_decoder` in the tile threads at 2160p and 2 % at the other two sizes. The same C, compiled
at the link by the same compiler in the same mode, next to a host that is Rust instead of C++: the
program around the C is another one, and so is what the optimiser sees of the functions the C calls
(in `warp_decoder` they are C++ it compiles too; here they are calls out of the code it knows). How much
of the 3.7 % that is, and how much is where the code landed, is not known. The 12 % of run 43330704655
was this plus the link mode plus that runner; the run that measured the parts on another runner shows
how much of it moves with the machine. Not tried, for later: `/O2` for `entdec.c`, `decodeframe.c` and
`decodetxb.c`, profile guided optimisation, and the Rust and the C in one LTO unit under clang
(`-Clinker-plugin-lto`).

#### What step 5 needs

- **The interface.** `rav1g::Decoder` exists as a thin owner of the C interface (`aom_codec_*`); step 5
  makes it the safe API of the design (a device and a queue or none, a temporal unit in, frames out as
  resources with fences, the settings) and generates `rav1g.h` with `cbindgen`. `Config::memory_available`
  is the budget callback of this step in raw form.
- **Which toolchain builds the library the player links.** The tile threads are the CPU limit at 4K, and
  the three choices differ there: C objects of MSVC `/GL`, linked by the player's `link.exe` with
  `/LTCG:incremental` (3.7 % behind the MSBuild library at 2160p in the last run); the clang-cl build with
  full LTO (level with it or ahead), which needs `lld-link` for the link of the player, or objects of
  clang without LTO (at 2160p 6 % behind in run 43312178597); or the entropy decoder as the MSBuild
  library it is today, with the Rust host as a second library. The console timing decides.
- **The C runtime.** The workspace links the static runtime (`+crt-static`); a UWP app links the DLL
  runtime of the VCLibs framework package. The player needs the C and the Rust built for that runtime.
- **The app container.** The standard library of `x86_64-pc-windows-msvc` imports Win32 functions without
  regard to what an app container offers; `x86_64-uwp-windows-msvc` is tier 3 and needs a nightly compiler
  to build the standard library. What the host uses of `std` is threads (the completion thread, the
  pipeline threads of the shader library), a lock, formatting and the allocator; an import the console
  does not have shows only on the console.
- **The host pool.** The decoder still lies in memory the application hands over, sized by the memory
  query, with the layouts of the C++ structs. When `Decoder` owns its state that can go (the queues as
  Rust queues, the inter breakdown and tile counters as Rust allocations, the `rav1g_*` functions that
  only Rust calls as plain functions); the "MEMORY" lines about the host pool then change, and the
  comparison of the lines in CI with them.
- **The C++ sources.** `libav1/dx/*.cpp` leave the tree when the player links the Rust library; with them
  go `warp_decoder`'s copy of the host, the `rust-equal` job in its present form, and the tests that read
  `av1_compute.cpp` and `av1_compute.h`.
- **Console tests for the parent:** the md5 set (8-bit and 10-bit, hidden frames, film grain, global
  motion, layers, size changes, scaled references, superres), `-gputime` against `v0.3.0` with the tile
  threads and the decode thread first, creation and release of 15 decoders in a row with the budget guard
  (the lines of the guard and of the release are those of the C++), a refused creation, a decode error
  between setup and submit (the slot is taken back), `-asyncclear`, `-slots 3` and `4`, `-lowmem`.

Seen on the way, not part of the port: on the local decoder (wine, vkd3d, lavapipe) a decoder without
the shared back buffer (`-lowmemparts 0`) gives wrong frames for 147 of the 244 vectors, with the C++
frame pipeline as with the Rust one; on WARP the same configuration decodes right (the case
`1080p-10bit-nolowmem` of the "MEMORY" lines passes its md5 check with both programs). Not looked into.

### Step 5: Rust API and C ABI (2026-11-12)

Three parts: what an application in an app container needs of the library (the C runtime, the imports),
the two interfaces, and the UWP test player on the Rust library.

#### Part 1: the C runtime and the app container

**The C runtime.** The programs of the workspace link the static runtime (`+crt-static` in
`.cargo/config.toml`); a UWP app links the DLL runtime of the VCLibs framework package. The library for
such an application is built with the target feature switched off, in a target directory of its own so
that the two builds do not replace each other:

```
CARGO_ENCODED_RUSTFLAGS=-Ctarget-feature=-crt-static<0x1f>--cfg<0x1f>windows_slim_errors
RAV1G_C_LIB_DIR=<directory>
cargo build --release -p rav1g --lib --features capi,app-container --target-dir target/app-container
```

- `cc` compiles the C with `/MD` when the target feature is off (it reads `CARGO_CFG_TARGET_FEATURE`), so
  the build script needs no switch of its own. Nothing changes for `rav1g-dec`.
- `CARGO_ENCODED_RUSTFLAGS` (flags separated by the byte 0x1f) and not `RUSTFLAGS`: with plain cargo
  either replaces the flags of `.cargo/config.toml`, but `cargo xwin` 0.23.1 adds the flags of that file
  behind those of `RUSTFLAGS`, and the static runtime wins. The encoded variable is the same everywhere.
- `--features app-container`: the C is compiled with `WINAPI_FAMILY=WINAPI_FAMILY_APP`, which is what
  MSBuild defines for `libav1.vcxproj`: the headers of the Windows SDK then declare only what an app
  container application may call, so the C cannot use anything else.
- The crate is a `staticlib` as well as an `rlib`: `rav1g.lib`. The two libraries of the C
  (`rav1g_c.lib`, `rav1g_c_speed.lib`) are not in it (link time optimisation: see "The entropy decoder
  with link time optimisation"); `RAV1G_C_LIB_DIR` names a directory the build copies them to. An
  application links the three.
- Which runtime DLL the program then imports from is decided by its link: with the import libraries of
  the player's MSBuild environment (the `store` directory of the toolset) it is `vcruntime140_app.dll`.

**The app container.** The question was whether the standard library of `x86_64-pc-windows-msvc`, which
does not care about app containers, can be in a UWP app, or whether the tier-3 target
`x86_64-uwp-windows-msvc` (nightly, `-Zbuild-std`) is needed. Answer: the stable target is used. The
analysis of the imports is GPT 6.1 Sol's (which component brings which import, what the linker does with
them, what is known about the console), from a DLL that has the interface of the library linked with
`lld-link /appcontainer`; the numbers below are of the same link with the functions of `rav1g.h`.

- **Linked the way cargo links (the Rust library first): 48 functions from `kernel32.dll`**, two from
  `ntdll.dll`, one from `oleaut32.dll`. The C++ player imports from none of the three: its Win32
  functions come through the API sets that `WindowsApp.lib` names (`api-ms-win-core-*.dll`).
- **Where the 48 come from.** Not from the standard library: it declares its functions by name and takes
  them from whatever import library the link has. They come from the `windows` crates
  (`windows-link` 0.2.1), which always declare their functions as raw imports (`raw-dylib`) of a named
  DLL, for every function of every module that is enabled, used or not: the `rav1g.lib` that was looked
  at has 1,032 such import members, 365 of them for `kernel32.dll`. The linker looks for a function first in the library whose
  object asked for it, so a call of the standard library, of the C or of the C runtime that is in
  `rav1g.lib` finds the `kernel32.dll` member there before it looks at `WindowsApp.lib`.
  `/NODEFAULTLIB:kernel32.lib` changes nothing: the members are in `rav1g.lib` itself.
- **`WindowsApp.lib` in front of the libraries of the decoder removes all 48.** The same functions then
  come from the API sets. This is how the player links: `WindowsApp.lib`, `rav1g.lib`, `rav1g_c.lib`,
  `rav1g_c_speed.lib`, and `ntdll.lib` and `userenv.lib` behind them for functions of the standard library
  that are in the objects the linker loads and not in the program it writes (file and pipe calls of
  `ntdll.dll`, the home directory of `userenv.dll`).
- **`--cfg windows_slim_errors`** takes the one import from `oleaut32.dll` out (`GetErrorInfo`, the
  extended error text of `windows-result`; it is in `WindowsApp.lib`, so it was no risk, only one DLL
  more).
- **What is left: two functions of `ntdll.dll`, `NtWriteFile` and `RtlNtStatusToDosError`.** The standard
  library writes the message of a panic to the standard error handle with them. `panic = "abort"` does
  not remove them. `ntdll.dll` is in every process, and Microsoft's dependency checker for console titles
  lists it among the DLLs of the core system; whether a program in the console's app container may
  import from it by name is not documented, and is what the console test of the packages answers.
- **Four API sets the C++ player does not import from** come with the standard library's panic output and
  backtrace: `api-ms-win-core-console-l1-1-0` (`GetConsoleMode`, `GetConsoleOutputCP`, `WriteConsoleW`),
  `api-ms-win-core-string-l1-1-0` (`MultiByteToWideChar`, `WideCharToMultiByte`),
  `api-ms-win-core-libraryloader-l1-2-1` (`LoadLibraryA`) and `api-ms-win-core-string-obsolete-l1-1-0`
  (`lstrlenW`). All four are names of `WindowsApp.lib`.
- Not in the program although the library has objects that name them: `userenv.dll`, the other file and
  pipe functions of `ntdll.dll`, `bcryptprimitives.dll` (the random seed of hash maps).
- **Why `kernel32.dll` matters.** It is not only a rule of the store's certification: a native component
  whose imports named `kernel32.dll` did not load on the console in a published case of 2016, and loaded
  once the imports went through the API sets. Nothing public says that the app system of the console has
  a `kernel32.dll` with the exports of the desktop.

**The check** (`testing/uwp_imports`; the checker and the first list are Sol's): `check_imports.py` reads
the normal and the delayed imports of a linked program and holds the DLLs against
`player-dlls-allow.txt`: the DLLs of the C++ player, the four API sets above, and of `ntdll.dll` the two
functions and nothing else. `kernel32.dll` and the desktop runtime (`vcruntime140.dll`) are not in the
list. The `rust-cross` job builds the library as above with clang-cl, links it with
`testing/uwp_imports/link_probe.sh` into a DLL with the 32 functions of `rav1g.h`, `WindowsApp.lib` first,
and checks that DLL (99 functions of 30 DLLs; the desktop runtime is allowed there, the Linux build has no
other). The player job checks the programs of its packages.

**The fallback, should the console refuse the two imports from `ntdll.dll`** (approved for the console
build of the player; not built now): the UWP target on a pinned nightly compiler, with the standard
library built from source and panics that abort without a message:

```
rustup toolchain install nightly-2026-11-13 --profile minimal --component rust-src --component rustfmt
RUSTFLAGS="-Ctarget-feature=-crt-static -Zunstable-options -Cpanic=immediate-abort --cfg windows_slim_errors"
cargo +nightly-2026-11-13 build --release -p rav1g --lib --features capi,app-container \
  -Zbuild-std=std,panic_abort --target x86_64-uwp-windows-msvc
```

Sol linked that library the same way: 81 functions of 25 DLLs, none from `kernel32.dll`, `ntdll.dll`,
`userenv.dll`, `oleaut32.dll` or `bcryptprimitives.dll`. It costs the panic message and the backtrace (a
panic ends the program at once), and a compiler that is not stable; it is for the UWP build alone. The
`rav1g` crate itself keeps building on stable Rust for `x86_64-pc-windows-msvc`: nothing in it may come
to need the nightly compiler.

#### Part 2: the Rust interface and the interface for C

**`rav1g`, the Rust interface** (`crates/rav1g/src/lib.rs` and `device.rs`, `decoder.rs`, `output.rs`,
`switches.rs`, `error.rs`):

| Type | What |
| --- | --- |
| `Device` | a D3D12 device, the direct queue the decoders submit to, and the compute pipeline states of the decoder on that device. `Device::new` makes its own (the first hardware adapter), `Device::with_d3d12` takes the application's device, with its queue or without. A decoder keeps a handle of its device, so the three live as long as a decoder needs them |
| `Settings` | the frame size and bit depth, tile threads, pipeline slots, `LowMemory` (the memory reductions as a set), the buffers of `intra_v2` and `inter_fast`, packed 10-bit output, the operating point, the output of all layers, and `memory_available`, the budget callback as a closure. `Settings::memory_requirements` is the memory query |
| `Output` | the application's side of the output, a trait: `image` gives an `OutputImage` for a frame that will be shown (a buffer with its mapping, one texture per plane, or both), `frame` gets the `Frame` when the GPU has written it, `unused` gets an image back that was not written, `end_of_frames` follows a drain |
| `Frame` | the image back, the size, bit depth and kind of the frame, the value passed with its temporal unit, `plane` (the rows of a plane in the mapped buffer) and `d3d12_fence` (the fence of the command list that wrote the frame, and its value) |
| `Decoder` | `new` (a device, settings, an output), `decode` (a temporal unit and a value for its frames), `drain`, the two layer controls; dropped, it waits for its frames and releases |
| `switches` | the process-wide choices of the test programs (CDEF and intra shaders, the fast inter path, the clears, the experiments, the fill pattern of the debugging runs), and the timing records, the stage dumps and the log as closures |
| `Error`, `ErrorCode` | the kind of an error, its text and what the decoder said about it; `ErrorCode::NeedsReconfigure` for a sequence the decoder does not fit |

- **Nothing unsafe is asked of the caller** but one promise: `OutputImage::buffer` is `unsafe` because the
  mapping it is given must stay valid while the decoder has the image. The decoder holds a reference of
  every resource of an image from `image` to `frame` (the C interface took pointers and trusted the
  application). The switches are safe functions: they are atomic values, and what a change in the middle
  of a frame costs is a wrong frame, not memory.
- **Frames with fences.** A frame is handed over when its command list has completed, as before: the
  completion thread waits for the fence of the frame's slot and then calls the output. `Frame::d3d12_fence`
  gives that fence and the value it reached. Nothing has to wait on it today; it is part of the frame so
  that the interface stays when a later step hands frames over ahead of their completion.
- **Underneath** the decoder is still created and driven through the entry points of the entropy decoder
  (`aom_codec_dec_init`, `aom_codec_decode`: C, part of the parser), and lies in a host pool that
  `Decoder` allocates; both are private to the crate now (`rav1g::sys` is hidden from the documentation).
  The GPU backend boundary is as it was: the interface names D3D12 in the methods that are about D3D12
  (`with_d3d12`, `d3d12_device`, `d3d12_fence`, the resources of an image), and `crates/rav1g/src/gpu`
  got one accessor (the fence of a slot as the interface of the backend).

**`rav1g.h`, the interface for C** (`crates/rav1g/src/capi.rs`, feature `capi`): the Rust interface
behind C types, 32 functions. `rav1g_device_create` (the application's `ID3D12Device *` and queue, or
null for the decoder's own), `rav1g_peek_stream_info`, `Rav1gSettings` with the output callbacks
(`get_image`, `frame`, `unused`, `end_of_frames`) and the budget callback, `rav1g_decoder_create`,
`_decode`, `_drain`, `_destroy`, the layer controls, `rav1g_memory_requirements`, the switches
(`rav1g_set_*`), `rav1g_last_error` and the decoder's error texts. `Rav1gStatus` has the numbers of
`ErrorCode`.

- `crates/rav1g/include/rav1g.h` is written by cbindgen from `capi.rs`
  (`cargo run -p rav1g-header`), is in the tree, and CI fails when it is not what the source gives
  (`cargo run -p rav1g-header -- --check` in the `rust-cross` job). The tool builds for the machine it
  runs on; it is not one of the default members of the workspace.
- The records of the timing run and of the stage dumps (`av1_perf_frame`, `av1_stage_dump`) are named
  in `rav1g.h` and defined in `dx/av1_perf.h`, as before: they are the instrumentation of the test
  programs, and the entropy decoder writes into them.

**`rav1g-dec` uses the interfaces and nothing else** (no `aom_codec_*`, no `rav1g::sys`): its task is
written once (`tools/rav1g-dec/src/library.rs`) against either interface. By default it uses the Rust
interface; with **`-capi`** it uses the functions of `rav1g.h` (device, settings, the output callbacks,
frames, switches, log), which is what the player will call. CI decodes the whole conformance set both
ways.

Checked before the push on the local decoder (wine, vkd3d, lavapipe): the 244 vectors through the Rust
interface and through `rav1g.h`, each plain and with flag sets (`-intrav2 -interfast -asyncclear -t 4
-slots 4`; `-gputimedetail -slots 3 -poisonbuf -t 4`, with `-capi` also `-interfast -asyncclear`), 244
of 244 every time; a refused creation (`-budget 5`).

## Estimate

Steps 1-2: about a day. Step 3: two to three days. Step 4: one to two days. Step 5: one to two days.
About a working week to a Rust decoder that is md5-identical on WARP and the console, assuming no large
surprises in step 4.

## Decisions

1. The UWP test player stays C++ in phase 1 and links `rav1g.h`; it is test tooling.
2. `rav1g-dec` keeps the `-gputime` and `-dumpstages` diagnostics from step 1, through the same C entry
   points until step 4 moves them.
3. Minimum supported Rust version: the latest stable when step 1 lands, raised freely.
