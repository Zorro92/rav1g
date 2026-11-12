# Test player

The test player is upstream's `uwp_dx12_player` (`testing/uwp_dx12_player`), built with
this repository's decoder changes. It decodes AV1 files listed in a plain-text args file,
logs timings, and can write or check one md5 per frame. It has been run on Xbox Series
consoles in UWP app mode; nothing in it is console-specific beyond the UWP packaging.

## What this repository changes in upstream

Forked from <https://aomedia.googlesource.com/av1-xbox-one> at
`b5c61563b48625c261a8d627cfa928896fd10b13`; upstream's history is kept. The changes since,
one commit each:

- **LocalState paths.** Only the UWP branch of `Decoders::GetAvailablePaths()` in
  `testing/player_common/decoder_creator.cpp` changes. Read and write paths both become
  `Windows::Storage::ApplicationData::Current->LocalFolder->Path`, converted with
  upstream's existing `pstos()` narrow-string helper. The x64 `uwp_common` project
  already compiles this file with `/ZW`, so no entry-point shim is needed.
- **`-gputime` and `-nopresent`**, two player flags for the timing run described under
  [Timing run](#timing-run--gputime-and--nopresent). They touch the test player
  (`testing/common`, `testing/player_common`) and the libav1 D3D12 host (`libav1/dx`,
  plus two timers in `libav1/av1/decoder/decodeframe.c`). No shader, no shader maths
  and no decoded output changes; without the flags the decoder runs as before.
- **CDEF v2**: a second CDEF compute shader, `libav1/dx/shaders/cdef_filter_v2.hlsl`, and
  see [CDEF v2 run](#cdef-v2-run--cdefv2). It is the default since package 0.2.8.0; the
  player flag `-cdefv1` selects the upstream shader `cdef_filter.hlsl`, which is not
  modified. The new shader produces the same bytes as the upstream one, including where
  upstream differs from the AV1 specification. A host-side comparison of the two shaders
  lives in `testing/cdef_check`, which is not part of the player build.
- **Intra v2**: a second intra prediction path: the shader `libav1/dx/shaders/intra_v2.hlsl`
  (with `intra_v2_hbd.hlsl` for high bit depth), its scheduler `libav1/dx/intra_v2_sched.h`,
  and the player flag `-intrav2` that selects it; see [Intra v2 run](#intra-v2-run--intrav2).
  The upstream shaders `intra_main.hlsl` and `intra_filter.hlsl` are not modified and stay
  the default. It also adds intra dispatch counters to the `-gputime` output and a
  host-side comparison of the two paths (`testing/intra_check`), which is not part of the
  player build. It is bit-exact but slower than the original path.
- **Intra optimisation A/B**: upstream builds `intra_main.hlsl` and `intra_main_hbd.hlsl`
  with FXC optimisation disabled. Two wrapper files, `intra_main_opt.hlsl` and
  `intra_main_hbd_opt.hlsl`, only `#include` those sources and are compiled with the
  project's default (optimised) settings, and the player flag `-intraopt` selects them; see
  [Intra optimisation A/B](#intra-optimisation-ab--intraopt). The upstream sources and
  their `/Od` builds are not modified and stay the default. **The optimised build decodes
  wrongly and is not faster**; the flag is kept only to investigate why.
- **Pipeline slots and clears**: the number of frames the decoder keeps between CPU decode
  and GPU completion becomes a decoder creation parameter
  (`aom_codec_dec_cfg_t::frame_slots`, player flag `-slots`), and the player flag
  `-asyncclear` moves the per-frame clears of a slot from the decode thread to the GPU
  completion thread; see
  [Pipeline slots and clears](#pipeline-slots-and-clears--slots--asyncclear). The changes
  are in `libav1/dx` and the test player. No shader changes. Without the flags the decoder
  has two slots and clears on the decode thread, as upstream does. The same section covers
  the `MEMORY` log lines of a decoder creation (what was allocated, and which allocation
  failed) and the decoder's size, which the player takes from the stream (`-nofitsize`
  for the fixed 4096x2176 10-bit).
- **Decoder memory**: the decoder took 754.7 MiB of D3D12 buffers at 4096x2176 10-bit,
  most of a console app's 1 GiB. Two buffers per slot are now only created when they are
  needed (the palette buffer on the first frame that allows screen content tools, the
  intra v2 buffer for a decoder created for `-intrav2`), there are three more reductions
  (the shared back buffer, on by default, and two that the player flag `-lowmem` adds),
  the sequence header peek behind the decoder's size and `aom_codec_peek_stream_info`
  reads the header correctly (and the bit depth), and a decoder creation that does not
  fit the app's memory limit is refused
  (`MEMORY REFUSED`) instead of losing the D3D12 device. See
  [What the decoder takes](#what-the-decoder-takes), [`-lowmem`](#smaller-decoder--lowmem)
  and [Budget guard](#budget-guard-memory-refused). A host-side check of the peek lives in
  `testing/seq_header_check`.
- **Memory after a decoder is destroyed**: on the console about 350 MiB stayed charged to
  the app after a decoder that had decoded frames was destroyed, and the budget guard then
  refused every later decoder of the launch. The player and the decoder now log the app's
  memory at every step of a task and of the release (`MEMORY app <where>`,
  `MEMORY destroy <step>`), the player waits for the memory to come back before it creates
  the next decoder, and the guard does the same before it refuses; the guard's margin was
  also wrong (it refused the default decoder in a fresh launch) and is re-derived. See
  [Memory after a decoder is destroyed](#memory-after-a-decoder-is-destroyed).
- **Defaults** (package 0.2.8.0): what the console runs proved is what a task without
  flags runs: CDEF v2, a decoder sized for the stream, the shared back buffer. The old
  behaviour has opt-out flags. See [Defaults and opt-out flags](#defaults-and-opt-out-flags).
- **md5 references from dav1d**: `testing/md5ref/make_md5_ref.sh` writes a reference in
  the player's format with ffmpeg and libdav1d; see
  [md5 references from dav1d](#md5-references-from-dav1d).
- **Hidden frames** (package 0.2.9.0): a stream with frames that are decoded but not shown
  at once (alt-refs, shown later with `show_existing_frame`: what most encoders produce)
  stopped after its first frames, because every hidden frame held one of the player's
  output buffers. An output buffer is now taken only for a frame that is output, at the
  moment it is output; see [Hidden frames](#hidden-frames-show_existing_frame). The change
  is in `libav1/dx`; no shader changes, and nothing changes for a stream in which every
  frame is shown.
- **Global motion**: frames with global motion decoded wrongly on the console in upstream.
  The shader for `GLOBAL_GLOBALMV` blocks took the position of the reference that is not
  warped through `scale_value()` of `libav1/dx/shaders/inter_common.h`, which used double
  precision; the console returned 0 from it. `scale_value()` is integer arithmetic now,
  with the same results. A host-side check of the warp shaders against libaom's C code
  lives in `testing/warp_check`. See [Global motion](#global-motion). Only the twenty
  `inter_scale_*` shaders change: those for scaled references, and the one that predicts
  `GLOBAL_GLOBALMV` blocks, which runs only in frames with global motion.
- **Super-resolution**: a frame with superres and CDEF and without loop restoration was
  upscaled from the wrong width in upstream (the visible width of a buffer description that
  nothing had set for the frame). One assignment block in `av1_setup_frame()`; the shader
  is unchanged and has a host-side check against libaom, `testing/superres_check`. See
  [Super-resolution](#super-resolution).
- **Stage dumps**: `-dumpstages` writes the buffer behind every GPU stage of a frame, in the
  player and in `warp_decoder`, and `testing/stage_dump/stage_dump.py` compares two runs. See
  [Stage dumps](#stage-dumps--dumpstages). The decoded output does not change.
- **Experiments**: `-lfhsplit`, `-intraedgefix` and a second package whose shaders sync all
  memory at every group sync, for the bugs that show on the console only. See
  [Experiments](#experiments). Off by default; the output does not change.
- **Layers, new sequences, references of different sizes**: of a temporal unit with spatial
  layers the frame of the highest layer is output (`-alllayers` for every layer's frame,
  `-oppoint <n>` for another operating point), a sequence that does not fit the decoder is
  reported with `AOM_CODEC_NEEDS_RECONFIGURE` and the player replaces the decoder, a warped
  block is clamped to its own reference and a scaled reference is never warped. See
  [Layers and new sequences](#layers-and-new-sequences) and
  [the warp check](#host-side-check-of-the-warp-shaders).
- **Shader cache of the workflow**: the CI builds take the bytecode headers of the shaders
  from one FXC call per shader, kept by content, instead of compiling every shader in every
  build. See [Shader cache of the workflow](#shader-cache-of-the-workflow). A build without
  `/p:Rav1gPrebuiltShaders` is unchanged.
- **Conformance run**: `testing/warp_decoder` is the decoder as a desktop console program
  that decodes on the D3D12 WARP software adapter and writes or checks one md5 per output
  frame, and `testing/conformance` runs it in CI over AOM's decoder test vectors, cuts of
  open content and streams encoded in the job, against a list of the streams that are known
  to fail. See [warp-decoder.md](warp-decoder.md). Nothing in the decoder or the player
  changes; `testing/md5ref/make_md5_ref.sh` now hashes every frame at its own size.

## Build

`.github/workflows/build.yml` runs on every push to `main` that touches more than docs, and
on demand. It builds `av1xbox.sln` (the player and its `uwp_common`/`libav1` dependencies)
as Release x64 with MSBuild, the `v145` toolset and the `10.0.26100.0` Windows SDK. The
minimum Windows version is overridden to `10.0.17763.0`. Upstream FXC/Shader Model 5.0
compute shader settings are unchanged, and with `-cdefv1` and without `-intrav2` and
`-intraopt` the decoder runs exactly the upstream shaders. An appended compiler option `/wd4996` suppresses the
newer SDK's deprecated `GetDesktopDpi` diagnostic (promoted to an error by upstream's
`/sdl`), without changing the player source.

Before building, the workflow stamps the package identity: name `Av1GpuTestPlayer`,
publisher `CN=Bale Development` (kept from the builds made before this repository existed,
so updates keep the same package family and its LocalState), display name
**rav1g Test Player**, version `0.2.<run number>.0`. A throwaway certificate is created per
run; the upstream committed temporary PFX is not used. Only the public `.cer` is exported.

```sh
gh workflow run build.yml --ref main
gh run list --workflow build.yml --limit 5
gh run watch <run-id> --exit-status
gh run download <run-id> --name rav1g-testplayer-x64-<run-id> --dir rav1g-testplayer
```

The artifact `rav1g-testplayer-x64-<run-id>` contains:

- `packages/`: the signed x64 sideload package (MSIX requested; legacy tooling may
  emit APPX), any emitted `Dependencies/x64/` framework packages and sideload helpers.
- `Av1GpuTestPlayer.cer`: this run's public signing certificate.
- `msbuild.log`: normal-verbosity build/package diagnostics, also uploaded on failure.
- `cdef-shaders/`: FXC listings of both CDEF shaders (`cdef_filter.asm`,
  `cdef_filter_v2.asm`) and `stats.txt`, a side-by-side count of what the listings
  declare; see [CDEF v2 run](#cdef-v2-run--cdefv2).
- `intra-shaders/`: the same for the intra shaders: the four upstream ones
  (`intra_main`, `intra_filter` and their `_hbd` builds), `intra_main_opt` and
  `intra_main_hbd_opt` (the optimised builds of the `intra_main` sources), and `intra_v2`,
  `intra_v2_hbd`; see [Intra v2 run](#intra-v2-run--intrav2) and
  [Intra optimisation A/B](#intra-optimisation-ab--intraopt).
- `shader-survey/`: `survey.md` (and `survey.csv`), one line per shader of the project
  with the same counts, and `asm/` with the FXC listing of every shader; see
  [Shader survey](#shader-survey).
- `SOURCES.txt`: repository commit, fork point, run URL, toolset/SDK and the commits since
  the fork point.
- `licenses/`: the upstream license and patent notices.

A failed run can still upload diagnostics; only use a package from a successful run.

The run has more jobs. On Linux, `cdef-check`, `warp-check` and `intra-check` run the
host-side comparisons of the CDEF shaders, of the warp shaders with libaom and of the intra
paths, and `sync-check` runs every shader with group memory under an adversarial thread
schedule ([below](#host-side-check-of-the-thread-groups)); `intra-check` also runs the self-test of the timing summary and the sequence header
check, and uploads its log as `rav1g-intra-check-<run-id>`. `conformance-streams` (Linux)
and `conformance` (Windows) are the conformance run: the decoder as a desktop program
decodes AOM's test vectors and other streams on the WARP software adapter of the runner and
compares the md5 of every frame; see [warp-decoder.md](warp-decoder.md). The run is green
only if all jobs pass.

### Host-side check of the thread groups

Upstream's shaders counted on the 64 threads of a thread group running in lockstep, which is
true on the console and nowhere else
([gpu-av1-decoder.md](gpu-av1-decoder.md#lockstep-reliance-portability-2026-11-11)). The
host checks therefore run a thread group the way D3D12 allows it to run
(`testing/common/group_sched.h`): between two group syncs every thread runs alone, from the
one to the other, in an order drawn anew every time; a plain `GroupMemoryBarrier()` does
nothing and is counted; and the group memory records, element by element, who read and wrote
it since the last group sync. A thread that reads what another thread wrote since then, or
changes what another read or wrote since then, is a race, reported with the shader and the
source line of both accesses, whatever order the threads were run in. A group sync that not
all threads of the group reach is an error, and so is a group that does not end.

`cdef_check`, `intra_check` and `warp_check` run their shaders this way and fail on a race, a
plain barrier or a sync error as they do on a wrong frame. `testing/sync_check` does it for
every shader with group memory that has no reference check (the transforms, inter prediction,
deblocking, loop restoration, film grain): the shader sources are turned into C++ by
`testing/sync_check/to_cpp.py` (line numbers are kept), and run on generated input (constants,
block records and buffers shaped so that every path is taken and two groups never write the
same words). Every case runs twice with two thread orders and must write the same bytes. It
has no reference output: whether a picture is right is the business of the conformance run.

```sh
sh testing/sync_check/run.sh             # the check
sh testing/sync_check/run.sh --mutate    # and, for every group sync, a copy of its shader with
                                         # that sync turned into a plain barrier: all must be
                                         # rejected for a race
sh testing/sync_check/run.sh --shaders <dir>   # the shaders of another directory
```

On the shaders before the fix (`--shaders` with a checkout of rav1g 7ff22d5) 53 of 58 fail.
The mutation run ends with

```text
sync_check: 58 shaders, 15660 thread groups: no race in group memory, every group sync reached by all threads,
no plain group memory barrier, output independent of the thread order
sync_check: 113 mutants, each with one group sync turned into a plain barrier: 111 rejected for a race, 0 because they did not end, 2 pass as listed (group syncs of upstream that order nothing)
```

No run can hang: the scheduler ends a thread group that makes more group syncs, or a thread
that makes more group memory accesses between two, than any shader comes near (a shader with
a race can take a loop bound from garbage), every run of a check has a timeout, and an
interrupted script ends its processes and removes its files.

## Install with Device Portal

1. Put the console in Developer Mode, open its Windows Device Portal, and use the
   Apps installation page to add the downloaded `.msix` (or `.appx`/bundle if emitted).
2. Supply `Av1GpuTestPlayer.cer` as the certificate and every required x64 framework
   package from the emitted `Dependencies/x64/` directory. Do not select another
   architecture's dependencies. If no dependencies were emitted, inspect the package
   dependencies/build log rather than assuming all required frameworks are installed.
3. Install **rav1g Test Player**, keep its Dev Home app type set to **App**, not Game,
   and launch it once to initialize LocalState.
4. Stop the player before editing its inputs. A new workflow run uses a new throwaway
   certificate; if an update is rejected because signing trust changed, remove the old
   package and install the new package/certificate. Back up its LocalState files before
   uninstalling.

The workflow never deploys to, launches on, or benchmarks a console.

## Inputs and logs in LocalState

Use Device Portal's file explorer, choose **LocalAppData** as the known folder, and
select the installed `Av1GpuTestPlayer` package. Under its **LocalState** folder,
create `args/` and `streams/`, upload your local test streams, and write a plain-text
`LocalState\args\args_av1.txt`. No stream or server location is embedded in the build.

Each non-comment line is one upstream task, for example:

```text
-i <stream path> -t 6 -frate -1 -limit 600 -progress
```

For a file uploaded as `LocalState\streams\sample.ivf`, use this concrete line:

```text
-i /streams/sample.ivf -t 6 -frate -1 -limit 600 -progress
```

`CParameters::Parse()` in `testing/player_common/cmd_parser.cpp` prefixes **both**
`-i` and `-dir` values with `read_path`; it concatenates the strings without adding
a separator. Therefore start the stream/directory value with one `/` or `\`, as in
the example. A plain `streams/sample.ivf` would be concatenated incorrectly, and an
absolute drive path would also be prefixed incorrectly. Quote values that contain
spaces. For multiple files, a line such as `-dir /streams -t 6 -frate -1 -limit 600 -progress`
uses the same LocalState root. The parser itself is unchanged.

`-t 6` selects six decoder threads, `-frate -1` requests maximum speed rather than
source-rate pacing, `-limit 600` bounds the task, and `-progress` logs per-frame timings.
Use the same streams, bit depth, resolution, frame count and comparable pacing/thread
settings for any dav1d comparison; this is a test player, not a playback pipeline.

Relaunch the player after uploading the args file. Upstream reads it at startup,
processes the task lines, and normally exits after completing them. Logs are written
to **`LocalState\logs`**; download them through the same Device Portal file explorer.
The log directory is created by upstream's existing logging initialization. An absent
or invalid args file produces diagnostics there, not a file-picker UI. App-mode GPU
availability and performance remain to be measured; upstream's old error text about
Game mode is not evidence that app mode was tested or a reason to change the app type
for these tests.

## Defaults and opt-out flags

Since package 0.2.8.0 a task line without any of the flags below runs what the console
runs proved (4K 10-bit, md5-exact on both 4K test clips in every case). The log lines at
the start of every task say what is in effect.

| Setting | Default | Its log line | Other values |
| --- | --- | --- | --- |
| CDEF shader | `cdef_filter_v2.hlsl`, 7 to 9 times faster CDEF | `CDEF shader: cdef_filter_v2 (default)` | `-cdefv1`: upstream's `cdef_filter.hlsl`, `CDEF shader: cdef_filter (original, -cdefv1)` |
| Decoder size | the stream's largest frame size, rounded up to 128, and 8-bit buffers for an 8-bit stream; see [Decoder size](#decoder-size--nofitsize) | `Decoder size: 3840x2176 10-bit (fit to the stream, default)` | `-nofitsize`: 4096x2176 10-bit for every stream, `Decoder size: 4096x2176 10-bit (fixed, -nofitsize)` |
| Memory reductions | part 1, one back buffer shared by all frames (frame buffer pool 12 → 10, no measurable time cost); see [Smaller decoder](#smaller-decoder--lowmem) | `Low memory parts: 1 (default: 1 shared back buffer)` | `-lowmemparts 0`: none, every frame takes its back buffer from the pool (`Low memory parts: 0 (none, -lowmemparts 0)`); `-lowmemparts N`: exactly the parts in `N`; `-lowmem`: all three (`-lowmemparts 7`) |
| Pipeline slots | 2 | `Frame slots: 2` | `-slots 3`, `-slots 4` |
| Clears | on the decode thread | `Clears: decode thread (original)` | `-asyncclear` |
| Intra prediction | upstream's shaders, `/Od` build | `Intra shaders: intra_main + intra_filter (original)`, `Intra main shader: intra_main (/Od, original)` | `-intrav2`, `-intraopt` |

- **`-cdefv2` and `-fitsize` are still accepted** and change nothing: old task lines keep
  working. `-lowmem` is still all three parts. Parts 2 and 4 stay opt-in because they can
  turn a pathological stream into a decode error (see the table under
  [Smaller decoder](#smaller-decoder--lowmem)); part 1 covers every stream.
- A flag together with its opposite is refused and the task is skipped: `Invalid arguments
  string. You can't use following args together: "cdefv2" "cdefv1"` (the same for
  `-fitsize` with `-nofitsize`).
- **The behaviour before package 0.2.8.0** (upstream's CDEF shader, the fixed size, a back
  buffer per frame from a pool of 12) is `-cdefv1 -nofitsize -lowmemparts 0`. Use it for
  A/B runs and to rule the defaults out when a stream decodes wrongly.
- The three settings are decoder creation parameters or process-wide switches that the
  player sets at the start of every task, so tasks with different values can follow each
  other in one args file; the player creates a new decoder where it has to.

**The library's own defaults are the same.** For an application that embeds the decoder:
a zero-initialised `aom_codec_dec_cfg_t` (with the fields an application has to fill:
size, bit depth, host memory, device, pipeline cache, output callbacks) gives two pipeline
slots (`frame_slots` 0), the shared back buffer (`low_memory` 0 is
`AV1_LOW_MEMORY_DEFAULT`; `AV1_LOW_MEMORY_NONE` is how to ask for none, any other value is
taken as the sum of the parts), no intra v2 buffers and no budget guard. CDEF runs through
`cdef_filter_v2.hlsl` unless the application calls `av1_set_cdef_v2(0)`, intra prediction
through upstream's shaders (`libav1/dx/av1_perf.h`). The library has no size of its own:
it is created for the `width`, `height` and `bitdepth` of the configuration, and
`aom_codec_peek_stream_info` gives those of a stream, which is what the player now passes.

**Budget guard with the default decoder**, a 3840x2160 10-bit stream, `-queue_size 4`
(computed from the allocation code; the guard's numbers are under
[Budget guard](#budget-guard-memory-refused)): the default decoder is 3840x2176 10-bit
with two slots and the shared back buffer, **615.7 MiB of device buffers** (639.9 for the
stream's size less one frame buffer of 24.2 MiB; up to 48.3 MiB more for palette buffers
on first use). With the margin of 48 MiB the creation needs 663.7 MiB. In a fresh launch
the app has about 762 MiB left for the decoder (1024.0 - 160.8 in use - 100.9 for the 4
output images), so it is **accepted with about 99 MiB to spare**; before, the default was
681.9 MiB and had 32 MiB to spare. Three slots are 766.1 MiB by default and still
refused (814.1 needed); `-lowmem -slots 3` is 652.6 MiB and accepted. (Package 0.2.8.0
logs 0.1 MiB less for each: the two output contexts of
[Hidden frames](#hidden-frames-show_existing_frame) came with 0.2.9.0.) Not changed: with
the player's default `-queue_size 16` the output pool of a 4K 10-bit stream is estimated
at 403.4 MiB, which leaves about 460 MiB for the decoder, so a 4K task without
`-queue_size` is refused, as it was before. 4K task lines need `-queue_size 4` (or
another small value).

## Timing run (`-gputime` and `-nopresent`)

The question for this run: **where does the time per 4K frame go**, and can the GPU
side alone ever fit in 41.67 ms (24 fps)?

| Flag | Effect |
| --- | --- |
| `-gputime` | Per-stage GPU timestamps and CPU timers for every decoded frame, a summary at the end of the task, and a live log line each time the decoder blocks on the player's output buffers. |
| `-nopresent` | Decoded frames go straight back to the buffer pool. Nothing is queued for display and nothing is presented, so the picture stays blank. The render loop sleeps 10 ms per pass instead of spinning. |

Both are off by default, and the player then behaves as before. `-progress` keeps its
existing per-frame lines.

Put both lines in `args_av1.txt`, the `-nopresent` one first (it cannot stall on the
display, so its summary is written even if the second task hangs):

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime
```

For a single timing run with presentation, use the second line alone.

### What is measured

GPU: a D3D12 timestamp query after every stage of the frame's command list, on the
queue the decoder submits to (the player passes no queue, so the decoder creates its
own direct queue; the frequency comes from that queue). The queries are resolved into
a readback buffer as the last command of the list and read only after the frame's
existing fence has completed, so the run adds no wait. A stage's time is its timestamp
minus the previous one, which means **the stage times add up to the first-to-last
span exactly**, and any gap between two stages is counted in the later one.

| GPU stage | What it covers |
| --- | --- |
| `gen_blocks` | prediction block generation |
| `residual_clear` | residual buffer fill (the whole buffer, every frame) |
| `idct_sort`, `idct` | transform block sort, inverse transform (all types) |
| `inter`, `recon` | inter prediction (all passes), inter residual reconstruction |
| `intra` | intra prediction, the whole wavefront loop |
| `lf_gen`, `lf_vert`, `lf_horz` | loop filter: block generation, vertical edges (two passes), horizontal edges |
| `cdef`, `superres`, `loop_rest` | CDEF, super-resolution, loop restoration |
| `ext_borders` | reference border extension |
| `film_grain` | film grain |
| `copy_plane`, `output_copy` | copy shader into the staging area, then the copy into the output buffer or textures |

A timestamp records when the GPU reaches that point of the list. Stages that end in a
UAV barrier are cleanly separated. `residual_clear` has no barrier before `idct_sort`,
so read those two together. Stages a clip never uses are shown as `(not dispatched)`.

CPU timers (QueryPerformanceCounter, all in ms per frame):

| Timer | Meaning |
| --- | --- |
| `entropy_wall`, `entropy_sum` | tile (entropy) decode: wall time on the decode thread, and the sum over the tile threads. `sum / wall` is the parallelism actually reached. |
| `record` | recording the frame's command list, waits excluded. `rec_gen_blocks`, `rec_idct`, `rec_pred`, `rec_loopfilter`, `rec_cdef_lr`, `rec_borders`, `rec_copy` split it by host function (`rec_copy` still contains `wait_output` and `wait_image`). |
| `submit` | `Close`, `ExecuteCommandLists`, `Signal` |
| `memset`, `memset_tile` | the per-frame and per-tile memsets on the decode and tile threads; the summary also gives the MB cleared per frame |
| `setup` | `av1_setup_frame`, including its waits and memsets |
| `frame` | decode thread, frame setup to submit done |
| `wait_slot`, `wait_sec_slot` | blocked waiting for a free pipeline slot (two by default, see `-slots`): for the frame, and for the second slot a frame takes when its own coefficient or mode info share is too small. High values mean the GPU is behind the CPU. With `-asyncclear` a slot is free only after its clears, so that time is part of the wait. |
| `wait_output` | blocked in the player's output buffer callback (pool of `-queue_size`) |
| `wait_fb`, `wait_image`, `wait_item` | blocked on the reference frame pool, the output image pool, the GPU work item pool |
| `fence_wait`, `gpu_latency` | completion thread: time blocked on the frame's fence, and submit-to-fence time |
| `notify` | time inside the frame-ready callback. With `-progress` this includes its two log writes per frame. |
| `async_clear` | completion thread, `-asyncclear` only: the slot's clears after the frame's fence. Zero without the flag. |

### Log lines

All new lines start with `GPUTIME`, in the usual `LocalState\logs` file.

Live, during decoding:

```text
GPUTIME output-buffer wait #3 BLOCKED: pool 4, free 0, held by decoder 3, queued or on display 1
GPUTIME output-buffer wait #3 resumed after 41.7 ms
```

The number is a running count. "Held by decoder" are buffers rented for frames that are
output and still in flight: a frame takes its buffer when its output copy is recorded and
hands it to the player when its GPU work is done. A frame that is not shown holds none
(up to package 0.2.8.0 it did, which is how a stream with hidden frames hung; see
[Hidden frames](#hidden-frames-show_existing_frame)). So the number is at most the
pipeline slots plus two (the `show_existing_frame` copies in flight), less one for the
frame that is waiting, and every buffer in it comes back without the decoder needing
another one. "Queued or on display" is the rest of the pool, which the player returns as
it presents (at once with `-nopresent`). A `BLOCKED` line with no `resumed` line at the
end of a log means the task hung right there.

After the last frame, the summary (the first 10 records and the first 10 output frames
are left out as warm-up when the task has more than 20). A record is a decoded frame or
the output copy of a `show_existing_frame`; the copies are counted and averaged in a line
of their own, so that the per-frame lines stay per decoded frame, and they are part of
the "per OUTPUT frame" line. A stream without `show_existing_frame` has no such line:

```text
GPUTIME ==== summary: 300 frames decoded, 300 output; warm-up 10 skipped, 290 measured (232 shown, 58 hidden, 58 show-existing copies, 12 intra), GPU timestamps on 290 ====
GPUTIME show-existing copies: 60 (58 measured): gpu busy mean N.NNN ms, cpu decode thread mean N.NNN ms, blocked mean N.NNN ms; not in the per-frame lines below
GPUTIME target: 24 fps needs <= 41.67 ms per output frame
GPUTIME -- per frame, ms --                          n     mean      p50      p95      max
GPUTIME wall between output frames                ...                                        NN.N fps output
GPUTIME wall between decoded frames               ...                                        NN.N fps decoded
GPUTIME gpu busy (first to last timestamp)        ...                                        NN.N % of wall
GPUTIME gpu stage sum                             ...
GPUTIME gpu idle gap before the frame             ...
GPUTIME cpu decode thread, setup to submit        ...                                        NN.N % of wall
GPUTIME cpu entropy/tile decode, wall             ...
GPUTIME cpu memsets (frame + tile)                ...                                        N.N MB cleared per frame
GPUTIME cpu clears, completion thread             ...                                        N.N MB cleared per frame
GPUTIME blocked, decode thread, all waits         ...                                        NN.N % of wall
GPUTIME per OUTPUT frame (1.00 decoded frames each): wall .. ms, gpu busy .. ms, cpu decode thread .. ms, ...
GPUTIME stage                    frames     mean      p50      p95      max      avg   share
GPUTIME gpu intra                   290    ...
GPUTIME cpu wait_output             290    ...
GPUTIME output-buffer waits: 3, total 120.0 ms, longest 60.0 ms (whole run, warm-up included)
GPUTIME ==== end of summary ====
```

In the stage table, `mean`/`p50`/`p95`/`max` are over the frames that ran the stage,
`avg` spreads the stage over all measured frames (the `avg` column adds up to the
"gpu busy" mean), and `share` is the stage's part of all GPU time.

With `-progress` as well, one line per decoded frame follows the summary. They are
kept in memory during the run and written at the end, so they add no log I/O to the
decode loop:

```text
GPUTIME frame 57 no=57 show=1 sef=0 intra=0 iters=62 iv2=0 idisp=71 ibarr=63 igroups=412 ipasses=412 tiles=4 sec=0 wall=61.20 busy=38.10 gap=22.40 | gen_blocks=0.31 ... | entropy_wall=18.20 ...
```

`wall` is the time since the previous frame completed, `busy` the first-to-last GPU
span, `gap` the time the decoder's queue sat idle before this frame, `iters` the last
intra wavefront iteration (so the frame ran `iters + 1` of them; -1 without intra),
`sec` whether the frame also took a second pipeline slot. `iv2`, `idisp`, `ibarr`,
`igroups` and `ipasses` describe the intra stage, see
[Intra timing fields](#intra-timing-fields). Timers at zero are left out.

`-nopresent` adds one line of its own: frames released and the fps between the first
and the last. If the queue refuses timestamp queries the summary says
`NO GPU TIMESTAMPS` and the CPU timers are still valid.

### How to read it

- **"per OUTPUT frame" is the line to compare with 41.67 ms.** Hidden frames are
  decoded but not output, so an output frame costs more than a decoded one.
- **GPU busy per output frame near or above 41.67 ms**: the shaders themselves are too
  slow; the stage table says which. No host change gets a port to 24 fps.
- **GPU busy well under the wall time, large idle gap, `wait_slot` near zero**: the GPU
  is waiting for the CPU. Look at `entropy_wall`, `record`, `memset` and the blocked
  waits.
- **`wait_slot` high**: the CPU is waiting for the GPU (only two frames can be in the
  pipeline unless `-slots` raises it).
- **`wait_output` high, or output-buffer wait lines**: the player's output pool is the
  limit, which is what `-queue_size 4` in app mode makes likely.
- **`-nopresent` faster than the presenting task**: the difference is presentation
  coupling (buffers held by the display queue, GPU time taken by the swap chain), since
  the decoder does not share a command queue with the swap chain in this player.

### Host-side check of the summary maths

The summary code is a plain header (`testing/player_common/perf_stats.h`). A small test,
not part of the player build, checks it:

```sh
g++ -std=c++14 -Wall -Wextra -I libav1 -o perf_stats_test testing/player_common/perf_stats_test.cpp
./perf_stats_test
```

## CDEF v2 run (`-cdefv2`)

In the first timing run, upstream's CDEF shader was the largest GPU stage on 4K
10-bit streams. CDEF v2 is its replacement: on the console it takes 5.5 to 7.2 ms per 4K
10-bit frame where upstream's takes about 52 ms, and the output is md5-exact.

| Flag | Effect |
| --- | --- |
| none | CDEF runs through `cdef_filter_v2.hlsl`. **The default since package 0.2.8.0.** |
| `-cdefv1` | CDEF runs through upstream's `cdef_filter.hlsl`: the same pipeline state and the same dispatch as upstream. For A/B runs. |
| `-cdefv2` | Accepted, changes nothing (it was the opt-in flag up to package 0.2.7.0). Refused together with `-cdefv1`. |

The task's log says which shader is in use:

```text
CDEF shader: cdef_filter_v2 (default)
CDEF shader: cdef_filter (original, -cdefv1)
```

The task lines in the rest of this section are the ones the comparison was run with, when
`-cdefv2` was the opt-in flag. With the current player the line without a flag runs CDEF
v2: add `-cdefv1` to the line that is to run upstream's shader.

### What changed in the shader

| | `cdef_filter.hlsl` (upstream) | `cdef_filter_v2.hlsl` |
| --- | --- | --- |
| Thread group | 4 x 4 x 8, one group per 32x32 luma | 8 x 8 x 1, one group per 64x64 luma (one CDEF strength index) |
| A thread's work | one 8-sample row of an 8x8 luma block, then one 4-sample row of a 4x4 chroma block (U or V) | one 8x8 luma block and its two 4x4 chroma blocks |
| 64x64 with zero strengths or no index | full load, direction search, filter with zero strengths (a copy) and store | the group copies its samples and does nothing else |
| 8x8 block with zero primary strengths | direction search still runs | no direction search |
| Direction search | on 16 of the 128 threads, a loop over 64 samples into an indexed array, while the other threads wait at a barrier | on every thread for its own block, fully unrolled |
| Group shared memory | 32-bit per sample, an input window and an output array; results are written to shared memory, read back and then stored | two 16-bit samples per word, input window only; each thread stores its own result |
| Barriers per group | six in the source, five after FXC | one |
| Primary tap weights | constant table (immediate constant buffer) | computed |

Bindings, the constant buffer and the frame buffers are the same, so the host change
is the choice of pipeline state and the dispatch size (`(w + 63) >> 6` by
`(h + 63) >> 6` groups instead of `(w + 31) >> 5` by `(h + 31) >> 5`).

### Timing: old against new

Both lines in `args_av1.txt`, same clip, same settings; the only difference is the flag:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2
```

Compare the `gpu cdef` row of the two summaries' stage tables, then `gpu busy` and the
"per OUTPUT frame" line. Every other GPU stage should be unchanged between the two tasks.

### Same output: md5 of every frame

Upstream's player can write one md5 per decoded frame and check a later run against
that file. First task: the original shader writes the reference. Second task: the new
shader is checked against it. A reference that does not come from this decoder at all is
better where one can be made: see [md5 references from dav1d](#md5-references-from-dav1d).

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -save_md5 /streams/<clip>.ivf.md5
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -cdefv2 -md5_frame_check
```

- `-save_md5` takes a path below LocalState, like `-i`. `-md5_frame_check` takes no
  value and always reads `<stream path>.md5`, so the saved file must have exactly the
  stream's name plus `.md5`, next to the stream.
- Use the same `-limit` in both tasks. The checker does not guard against a reference
  file with fewer frames than it decodes.
- `-save_wrong_yuv <dir>` (with `-md5_frame_check`) writes every frame whose md5 differs
  to `<dir>/<stream>_<frame>.yuv` below LocalState (planar, 16 bits per sample for
  high bit depth, no header), for a pixel diff against the reference decoder. Create the
  directory first.
- **Save and check in separate launches.** Up to package 0.2.6.0 the saver's file was not
  closed when its task ended, so a check task in the same `args_av1.txt` read only the
  first ~8 KB (about 210 frames). Past that point the checker either reported errors with
  an empty expected value (`Avaiting  In real ...`) or crashed without a log line. The
  file is now closed at the end of its task (the task's md5 state is released there, see
  [Memory after a decoder is destroyed](#memory-after-a-decoder-is-destroyed)); a save and
  a check in one launch have not been run on the console yet, so until one has: run the
  save task, let the app exit, then launch again with only the check task.
- The result is in the log when the second task ends: `MD5 is OK`, or
  `MD5 checksum errors in N frames` with one `MD5 checksum error in <n> frame` line
  per bad frame. Later frames are predicted from earlier ones, so one wrong CDEF
  output shows up as errors from that frame on.
- Both flags read every frame back from the GPU. Do not combine them with `-gputime`
  for timing numbers.
- Do not add `-try10x3`: the saver and the checker hash that output format differently.

The md5 covers the visible picture after all later stages (loop restoration, film
grain). Run it on at least one 8-bit and one 10-bit clip.

### md5 references from dav1d

The reference of an md5 check no longer has to come from a `-save_md5` run of the
original shaders. `ffmpeg -c:v libdav1d -i <clip>.ivf -f framemd5 -` prints one line per
output frame, and the md5 in it (the sixth comma-separated field) is the player's md5 of
that frame: both hash the visible samples of the luma plane and then of the two chroma
planes, row by row, two bytes a sample (low byte first) above 8 bits. Verified for 10-bit
4:2:0: the dav1d reference of a 300-frame 4K 10-bit clip is byte for byte the file the
player saved with the original shaders.

`testing/md5ref/make_md5_ref.sh` writes the file in the player's format:

```sh
sh testing/md5ref/make_md5_ref.sh <clip>.ivf        # every frame: <clip>.ivf.md5
sh testing/md5ref/make_md5_ref.sh <clip>.ivf 300    # the first 300, for -limit 300
```

It needs an ffmpeg with libdav1d and nothing else; no media is in the repository. The
format is what `-save_md5` writes: one line per output frame in output order, the 32 hex
digits, three spaces, the frame number counted from 0, CRLF. Upload the file next to the
stream as `<stream name>.md5` and run only the check task:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -md5_frame_check
```

- **Prefer it to a `-save_md5` reference.** It is independent of this decoder (a
  `-save_md5` reference only shows that two configurations of it agree), it needs no
  console run and no separate launch for the save task, and it exists for streams the
  original path cannot decode, such as the streams with hidden frames that hung the
  player (see [Hidden frames](#hidden-frames-show_existing_frame)).
- It is one line per frame the stream outputs, in output order: a frame that is decoded
  but not shown has no line, a frame shown with `show_existing_frame` has one.
- dav1d applies film grain, as the decoder does at output.
- Not for a task with `-start` (the reference has to begin at that frame) and not with
  `-try10x3`.
- Checked here for 8-bit and 10-bit 4:2:0 against a hash of ffmpeg's raw output. Only the
  10-bit case was compared with a file the player saved.
- **A stream whose frames change size** (reference scaling, or a new size at a key frame) is
  hashed frame by frame at the size each frame has: the script passes `-noautoscale` to
  ffmpeg. Without it ffmpeg scales every frame to the size of the first one, and such a
  reference reports every frame of another size as wrong whatever the decoder does. A
  reference made by the script before it had `-noautoscale` is wrong for such a stream, and
  only for such a stream: make it again.

### Host-side comparison of the two shaders

`testing/cdef_check` compiles both `.hlsl` files as C++ (attributes and semantics
stripped by `sed`, nothing else changed) and runs them the way a dispatch does, with
all threads of a group stopping at each barrier. It builds frame buffers with the
decoder's layout, fills them and every input with random and edge-case data, runs both
shaders and compares the **whole buffer**, padding included. It covers 8-bit and high
bit depth, frame sizes that are not multiples of 8 or 64, missing strength indices,
skipped blocks and all strength and damping values. A second pass changes one tap
weight in a copy of the new shader and requires the comparison to fail.

From the repository root (needs `g++`, no GPU):

```sh
sh testing/cdef_check/run.sh --mutate
```

It ends with `cdef_check: OK, cdef_filter_v2.hlsl output is byte-identical to
cdef_filter.hlsl`. This proves the HLSL logic; the md5 run above is the proof for the
compiled shader on the console.

### Shader listings in the artifact

The workflow runs both shaders through the same FXC, profile and flags as the Release
build (`/T cs_5_0 /E main`) and stores the listings under `cdef-shaders/`. After the
build it checks that each listing's bytecode is the one compiled into the package.
`stats.txt` gives, for each shader: instruction slots, temp registers, indexable
temps (arrays the compiler could not keep in registers), immediate constant buffer
use, group shared memory size, barriers, and load/store counts. The counts are static
(instructions in the listing), not instructions executed, and are not a timing.

With the pinned upstream and FXC from Windows SDK `10.0.26100.0`:

| | `cdef_filter.hlsl` | `cdef_filter_v2.hlsl` |
| --- | --- | --- |
| Thread group | 4, 4, 8 | 8, 8, 1 |
| Instruction slots | 1160 | 3223 |
| Temp registers | 10 | 29 |
| Indexable temps | 1: `x0[120]` (`partial[8][15]` of the direction search), 227 instructions use it | none |
| Immediate constant buffer | yes, read at 4 places | no |
| Group shared memory | 12,224 bytes per 32x32 | 15,552 bytes per 64x64 |
| Barriers | 5 | 1 |
| Loops | 22 | 9 |

The new shader is longer because its direction search and its four-sample filter are
unrolled; what a thread executes is shorter. In the upstream listing the direction
search loop reads and writes the indexable temp 16 times per sample with a computed
index, 64 samples per block.

## Intra v2 run (`-intrav2`)

In the timing runs with `-cdefv2`, intra prediction took 14 to 21 ms per frame on average
on 4K 10-bit streams and 59 ms on a key frame, while the per-frame lines showed 80 to
190 wavefront iterations on inter frames and 863 on the key frame. Intra v2 is a
replacement to measure against it.

| Flag | Effect |
| --- | --- |
| `-intrav2` | Intra prediction runs through `intra_v2.hlsl` (`intra_v2_hbd.hlsl` for high bit depth) instead of `intra_main.hlsl` + `intra_filter.hlsl`. Decoded output is meant to be identical. |

Off by default: without the flag the decoder takes the upstream code path, shaders and
dispatches. The flag is independent of `-cdefv2`; the two can be combined freely. A
frame that allows intra block copy (screen content) keeps the upstream shaders even
with the flag; `iv2` in the per-frame line says which path a frame took. The task's log
says which path is selected:

```text
Intra shaders: intra_main + intra_filter (original)
Intra shaders: intra_v2 (-intrav2; frames with intra block copy keep the original shaders)
```

### What changed

Upstream gives every intra transform block a wavefront iteration of the **whole frame**
(one more than the highest iteration among the neighbours it reads) and runs, per
iteration, one dispatch of `intra_main`, one of `intra_filter` if the iteration has
filter-intra blocks, and a UAV barrier. A key frame of small blocks needs hundreds of
such rounds, each of them a few small dispatches that the GPU has to finish before the
next one may start.

| | upstream (`intra_main` + `intra_filter`) | `intra_v2` |
| --- | --- | --- |
| Schedule | one wavefront iteration of the frame per round | a block gets a **step** (round of dispatches) and a **pass** (round inside a thread group). The step only rises where a block reads from another 64x64 region; neighbours inside a region are ordered by passes |
| Per round | 1 or 2 dispatches (two pipeline states) and a UAV barrier | 1 dispatch (more only when the regions of a step differ a lot in pass count) and a UAV barrier |
| Thread group | `intra_main`: 256 threads, a thread is one row of 4 pixels, blocks packed by size. `intra_filter`: 64 threads per block | 384 threads: one 64x64 luma region and its two chroma regions, a thread is one 4x4 cell (16 pixels) |
| Waiting | 12 group syncs per group in `intra_main`; none across groups | 2 group syncs per pass; none across groups. No thread ever waits for another group |
| Edge pixels | loaded, corner-filtered, edge-filtered and upsampled in stages through group memory, a sync after each stage | each pixel evaluates the edge samples it needs as a function of the frame buffer |
| DC and CFL sums | every thread sums the whole edge; CFL average by a tree reduction that relies on the threads of a wave running in lockstep | one partial sum per cell in group memory, added up after a sync |
| Filter intra | separate shader, relies on 64 threads running in lockstep | cells of the same group, one anti-diagonal of cells per pass |
| FXC | the project compiles `intra_main` with optimization disabled (`/Od`) | default optimization |
| Host, per frame | clears the iteration grid (5 bytes per 4x4 of the frame) and scans it for every block | scans a cell map for the same neighbours; the map is not cleared per frame (entries carry a frame stamp); one sort of the region list |

The block records are the ones `gen_pred_blocks.hlsl` writes; that shader is unchanged
(with `-intrav2` every block is "iteration 0" for it). The new path binds two more
buffers: the cell map with the region list, and the block map. The scheduler is a
header (`libav1/dx/intra_v2_sched.h`) shared by the decoder and the host check.

Group syncs inside a loop are only legal when the loop count is the same for all
threads, so all groups of a dispatch run the pass count of the busiest region; regions
with far fewer passes are dispatched separately within the step.

### Intra timing fields

`-gputime` reports what drives the cost of the intra stage, for both paths. In the
per-frame lines (with `-progress`):

| Field | Meaning |
| --- | --- |
| `iters` | last wavefront iteration (upstream) or last step (`-intrav2`); the count is `iters + 1`, -1 means no intra block |
| `iv2` | 1 if the frame went through `intra_v2` |
| `idisp` | `Dispatch` calls of the intra stage |
| `ibarr` | UAV barriers of the intra stage (one per iteration or step) |
| `igroups` | thread groups dispatched |
| `ipasses` | thread groups times the passes they run. `ipasses / igroups` is the average pass count; a pass costs two group syncs. For the upstream path it equals `igroups` |

and one line in the summary, over the measured frames that have intra blocks:

```text
GPUTIME intra stage per frame with intra: dispatches mean 14.2 max 131, UAV barriers mean 12.8 max 127, thread groups mean 310 max 6120, passes per group 9.4 (intra_v2 on 288 of 288 frames)
```

The counters are plain integers kept by the host; without `-gputime` nothing is reported.

### Timing: old against new

All tasks in one `args_av1.txt`, same clip and settings, only the shader flags differ:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -intrav2
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -intrav2
```

Compare the `gpu intra` row of the stage tables, the "intra stage per frame" line, then
`gpu busy` and the "per OUTPUT frame" line. The first two tasks isolate the intra change,
the last two show it on top of the CDEF change. Check `cpu setup`, `cpu entropy_wall` and
`cpu memsets` too: the new path moves a little work on the decode threads (it writes
8 bytes per 4x4 cell of every intra block, and no longer clears the iteration grid).

### Same output: md5 of every frame

As for [CDEF](#same-output-md5-of-every-frame): the original shaders write the
reference, the new path is checked against it, **in separate launches**.

First launch, reference from the upstream shaders (no `-intrav2`, no `-cdefv2`):

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -save_md5 /streams/<clip>.ivf.md5
```

Let the app exit, then a second launch with only check tasks (they only read the file,
so both can be in the same `args_av1.txt`):

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -intrav2 -md5_frame_check
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -cdefv2 -intrav2 -md5_frame_check
```

The rules of the CDEF section apply unchanged (same `-limit`, the `.md5` file next to
the stream, no `-gputime`, no `-try10x3`). A reference file saved for the CDEF check of
the same clip and `-limit` can be reused. Each task ends with `MD5 is OK` or a list of
bad frames. Run it on at least one 8-bit and one 10-bit clip, and include a clip with a
key frame of small blocks. Every later frame is predicted from earlier ones, so one
wrong intra block shows up as errors from that frame on.

### Host-side comparison of the two paths

`testing/intra_check` compiles the four upstream intra shaders and the two `intra_v2`
builds as C++ (attributes, semantics and register bindings stripped by `sed`) and runs
both paths on generated frames:

- frames are built from random AV1 partition trees (64 and 128 superblocks, all
  partition types the generator knows: none, split, horizontal, vertical and the two
  4-way ones; forced splits at the frame edge), up to 2 x 2 tiles, frame sizes that are
  not multiples of 64, with intra, inter and inter-intra blocks mixed;
- every luma and chroma mode: DC, the eight directional modes with all angle deltas,
  the smooth modes, Paeth, CFL with all alpha values, filter intra with all five filters,
  palette blocks (which are skipped), with and without residual, edge filter strengths,
  edge upsampling, top-right and bottom-left availability, transform splits;
- the records are a port of the intra part of `gen_pred_blocks.hlsl`; the iterations of
  the original path come from a port of the grid code in `av1_mi_push_block`; the new
  path uses the decoder's own `intra_v2_sched.h`; the lookup tables are pulled from the
  decoder sources by the script;
- a thread group is emulated with one fiber per thread. Group syncs block until all
  threads arrive. A plain `GroupMemoryBarrier()`, which the upstream shaders use where
  they rely on a wave running in lockstep, lets all other threads reach their next
  barrier first. Group memory starts as garbage, different in every run;
- the **whole frame buffer** is compared, 8-bit and 10-bit. The original path is run
  twice to show its result does not depend on the garbage;
- the frame buffer is watched for reads that depend on GPU scheduling: a word read by
  one thread group that another group wrote since the last UAV barrier, or (new path)
  read by one thread that another thread wrote since the last group sync. Any such read,
  an access outside a buffer or a group sync not reached by all threads fails the check;
- the frames of a layout share the cell map like consecutive frames of the decoder, so
  entries of earlier frames with an older stamp are present;
- a second run limits a thread group to 6 passes instead of 250, which forces regions
  to continue in later steps all the time;
- `--mutate` then breaks one detail at a time in a copy (rounding of the corner filter,
  an upsampling tap, the edge filter strength, a filter-intra neighbour, the CFL average,
  the Paeth tie, the inter-intra blend, the pass order in the shader, the step and pass
  order in the scheduler) and requires every such copy to be rejected.

From the repository root (needs `g++`, no GPU):

```sh
sh testing/intra_check/run.sh --mutate
```

It ends with `intra_check: intra_v2 output is identical to the original path on every
frame` and one `mutant rejected as expected` line per mutation.

Not covered by this check: the compiled shaders and the real GPU (that is the md5 run);
`gen_pred_blocks.hlsl` itself and the decoder's `has_top_right`/`has_bottom_left` tables
(the generator treats a neighbour as available whenever it was decoded earlier, which
includes every case of the tables); intra block copy (those frames keep the upstream
shaders); frames larger than about 380 pixels a side.

### Shader listings in the artifact

`intra-shaders/` holds the FXC listings of the four upstream intra shaders and of
`intra_v2`, `intra_v2_hbd`, compiled with the flags the Release build uses for each
(`intra_main` and `intra_main_hbd` with `/Od`, as the project asks), and `stats.txt`
with the same counts as for CDEF. After the build the workflow checks that each listing's
bytecode is the one compiled into the package. The directory also has the listings of
`intra_main_opt` and `intra_main_hbd_opt`, see
[Intra optimisation A/B](#intra-optimisation-ab--intraopt).

## Intra optimisation A/B (`-intraopt`)

Upstream's project compiles `intra_main.hlsl` and `intra_main_hbd.hlsl` with FXC
optimisation disabled (`DisableOptimizations` for these two items in
`libav1/build/libav1.vcxproj`, FXC `/Od`). They are the only shaders built that way, and
neither a comment nor upstream's history says why. This flag puts both builds of the same
source into one install, so a console run can compare their timing and their output.

| Flag | Effect |
| --- | --- |
| `-intraopt` | Where the decoder runs `intra_main` (`intra_main_hbd` for high bit depth), it uses the pipeline state built with FXC optimisation instead of the `/Od` one. The shader source is the same. |

Off by default: without the flag the decoder uses the same pipeline states and issues
the same dispatches as before. With the flag only the pipeline state of the `intra_main`
dispatches changes; root signature, bindings, constants, dispatch sizes and
`intra_filter` (which upstream already builds optimised) stay as they are.

The flag is independent of `-cdefv2` and `-intrav2` and combines freely with both. With
`-intrav2` it applies to the frames that keep the upstream path (frames that allow intra
block copy); the other frames do not run `intra_main` at all.

The task's log says which build is selected:

```text
Intra main shader: intra_main (/Od, original)
Intra main shader: intra_main_opt (FXC optimised, -intraopt)
```

### How it is built

`intra_main_opt.hlsl` and `intra_main_hbd_opt.hlsl` consist of the licence header and one
`#include` of the upstream file. They are separate `FxCompile` items without the
`DisableOptimizations` override, so the Release build compiles them with the project's
default settings, like every other shader. The workflow stops if any shader other than
`intra_main` and `intra_main_hbd` is built with `/Od`, or if the two `_opt` items are
missing.

Whether the optimised bytecode behaves like the `/Od` one on the GPU is not known from the
build; that is what the md5 run below is for. The host-side check (`testing/intra_check`)
runs the HLSL source as C++ and cannot tell the two builds apart.

### Timing: `/Od` against optimised

Both lines in `args_av1.txt`, same clip, same settings, both with `-cdefv2`; the only
difference is the flag:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -intraopt
```

Compare the `gpu intra` row of the two summaries' stage tables, then `gpu busy` and the
"per OUTPUT frame" line. The "intra stage per frame" line (dispatches, UAV barriers,
thread groups) must be the same in both tasks, and every other GPU stage should be
unchanged.

### Same output: md5 of every frame

As for [CDEF](#same-output-md5-of-every-frame), **in separate launches**. First launch,
the reference from the `/Od` build (no `-intraopt`):

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -save_md5 /streams/<clip>.ivf.md5
```

Let the app exit, then a second launch with only the check task:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -cdefv2 -intraopt -md5_frame_check
```

The rules of the CDEF section apply unchanged: the same `-limit` in both tasks, the
`.md5` file next to the stream, no `-gputime`, no `-try10x3`. A reference file saved for
the CDEF or intra v2 check of the same clip and `-limit` can be reused, since `-cdefv2`
is md5-exact. The task ends with `MD5 is OK` or a list of bad frames. Run it on at least
one 8-bit and one 10-bit clip (they use different shaders), and include a clip with
chroma-from-luma blocks and a key frame of small blocks. A timing result of the
optimised build counts only if its md5 check passes.

### Console result (2026-11-11): wrong output, no gain

Package 0.2.3.0 (run 43008653829), 4K 10-bit, 1 tile, 300 frames, all tasks with `-cdefv2`:

| Clip | `gpu intra` `/Od` ms | `gpu intra` `-intraopt` ms | md5 with `-intraopt` |
| --- | --- | --- | --- |
| SVT low-delay 40 Mbps | 13.68 | 13.95 | wrong on all 300 frames |
| Jellyfin 40 Mbps | 21.21 | 21.30 | wrong on all 300 frames |

- **No speed gain.** The intra stage time is the same within noise, as are dispatches,
  barriers and thread groups. The driver compiles the DXBC to GPU code with its own
  optimiser either way, so FXC's `/Od` costs nothing here. The stage is bound by the
  latency of its wavefront steps (about 0.11–0.16 ms per step), not by instructions.
- **Wrong output, and not deterministic.** Both clips fail from the first (key) frame. Two
  check runs of the same clip agree on none of the 300 frame md5s, so the optimised
  bytecode has a timing-dependent race rather than a fixed miscompile. A control task with
  the same build and no `-intraopt` gives `MD5 is OK` on both clips.
- The likely reason upstream disabled optimisation for this shader is this broken output.
  The listings show two candidates: FXC drops the group sync at `intra_main.hlsl:300`
  (between the left edge-filter stores and the above upsampling loads), and the CFL
  reduction relies on 64 threads running in lockstep with only `GroupMemoryBarrier()`.
  Neither is confirmed.
- Only `intra_main_hbd_opt` ran on the console (both clips are 10-bit); the 8-bit
  `intra_main_opt` was not tested.

### Shader listings in the artifact

`intra-shaders/` has `intra_main_opt.asm` and `intra_main_hbd_opt.asm` next to the `/Od`
listings `intra_main.asm` and `intra_main_hbd.asm`, and `stats.txt` gives the counts and
the FXC compile time of all four. After the build the workflow checks that each of the
four listings has the bytecode compiled into the package.

## Pipeline slots and clears (`-slots`, `-asyncclear`)

With CDEF v2 the CPU decode thread and the GPU need about the same time per 4K frame, and
both vary from frame to frame. The decoder has two pipeline slots, so only two frames can
be between the start of their CPU decode and the end of their GPU work: whenever one side
is slower for a frame, the other waits (`wait_slot` on the CPU side, the idle gap on the
GPU side). The decode thread also clears about 7.5 MB of the slot it has just taken before
it can start a frame. These two flags test whether a deeper pipeline and clearing
elsewhere shorten the wall time per frame. Neither changes the decoded output.

| Flag | Effect |
| --- | --- |
| `-slots N` | The decoder is created with `N` pipeline slots, `N` = 2, 3 or 4. Default 2. Any other value is refused: the task is skipped with `Invalid arguments string. -slots N is not allowed: the decoder takes 2 to 4 slots.` |
| `-asyncclear` | The two large per-frame clears of a slot are done by the GPU completion thread after the fence of the frame that used the slot, before the slot goes back to the pool. The decode thread then finds the slot already cleared. |
| `-nofitsize` | The decoder is created for 4096x2176 10-bit whatever the stream is, as it was up to package 0.2.7.0. Without it (the default) the decoder is created for the stream's largest frame size and bit depth, which makes every buffer smaller; see [Decoder size](#decoder-size--nofitsize). `-fitsize` is still accepted and changes nothing. |
| `-lowmem` | All three memory reductions: one back buffer shared by all frames (which is the default on its own), and smaller per-slot shares of the mode info pool and of the coefficient buffer; see [Smaller decoder](#smaller-decoder--lowmem). |
| `-lowmemparts N` | Exactly the reductions in `N`, the sum of 1 (shared back buffer), 2 (mode info shares) and 4 (coefficient shares). Default 1. `-lowmemparts 0` is none of them, `-lowmem` is `-lowmemparts 7`. |
| `-noguard` | The decoder of this task is created without the [budget guard](#budget-guard-memory-refused). Only for the test described under [Memory after a decoder is destroyed](#memory-after-a-decoder-is-destroyed): a creation that does not fit loses the D3D12 device for the rest of the launch. Has no effect on a task that reuses the decoder of the task before. |

Without any of them the decoder has two slots, clears on the decode thread, is sized for
the stream and shares one back buffer (see
[Defaults and opt-out flags](#defaults-and-opt-out-flags)). They are independent of each
other and of `-cdefv1`, `-intrav2`, `-intraopt`, `-gputime`, `-nopresent` and the md5
flags.

**Console results so far (2026-11-11, package 0.2.5.0, app mode, both 4K 10-bit clips):**

- A console app has a memory limit of **1024.0 MiB** (`Windows::System::MemoryManager`),
  and the decoder's D3D12 buffers count against it. The app used 77 MiB before the
  decoder and **882.7 MiB after the default two-slot decoder** (40 device buffers,
  754.7 MiB; host pool 23.2 MiB), and the output pool of 4 images took another 100.5 MiB:
  about 40 MiB were left. DXGI reported a local budget of 1209.6 MiB, with usage going
  from 37 to 810 MiB.
- **`-slots 3` did not fit and took the D3D12 device with it.** The buffers were created
  (GPU usage 1006.9 MiB, the DXGI budget fell to 1053.9 MiB), then the creation failed at
  `upload of the lookup tables` with `hr 0x887A0005` (`DXGI_ERROR_DEVICE_REMOVED`). The
  failed decoder was released correctly (usage back at 53.6 MiB), but a removed device
  stays removed for the life of the process: every later task of the launch failed at
  `D3D12 device or command queue (hr 0x887A0005)`, the default ones too. The
  [budget guard](#budget-guard-memory-refused) now refuses such a creation before it
  allocates.
- **`-asyncclear` is md5-exact on both clips and gives no gain with two slots.** The
  decode thread's clears (3.3 to 4.3 ms per frame) go away, and `wait_slot` grows by 1.5
  to 3.7 ms: with two slots the slot comes back just when the decode thread needs it, so
  the decode thread waits for the clear instead of doing it. The completion thread spends
  3.6 to 3.9 ms per frame on the clears. Two runs with the same settings differ by about
  1 fps on the SVT-AV1 clip, which is more than what is left of the difference.
- **`-fitsize` was wrong in that package**: it logged `the stream's largest frame is
  32x128` for a 3840x2160 stream and created a 128x128 decoder. Fixed, see
  [Decoder size](#decoder-size--nofitsize).

Every task's log says what is in effect:

```text
Frame slots: 2
Clears: decode thread (original)
```

```text
Frame slots: 3
Clears: off the decode thread (-asyncclear)
```

### Slots

A slot (`av1_frame_thread_data` in `libav1/dx/types.h`) is everything one frame needs
from the start of its entropy decode until its GPU work is done: a command allocator and
fence, the block and mode info buffers the CPU fills and the GPU reads, and a share of
the coefficient buffer and of the mode info pool. A frame whose own share is too small
takes a second slot for the overflow (`sec=1` in the frame lines, `wait_sec_slot`).

The slot count is a decoder creation parameter (`aom_codec_dec_cfg_t::frame_slots`; 0
means the default 2, the library clamps other values to 2..4). The player keeps one
decoder across the tasks of an args file and creates a new one when `-slots` differs
from the task before (`Destroy decoder`, `(Re)create decoder` in the log), as it already
does for `-t` and `-queue_size`.

What else follows the slot count:

- **Frame buffers in the pool**: by default 10 with two slots, one more per added slot
  (11, 12), and two back buffers beside the pool (the one the frames share and the one
  the filters share); the derivation is at the end of this item. With `-lowmemparts 0`,
  as up to package 0.2.7.0: 12 with two slots, one more per added slot (13, 14), and one
  back buffer beside the pool. Where the 12 come from: the reference map holds at
  most 8 different buffers; the frame being decoded takes one for itself and, when it is
  filtered after reconstruction (CDEF, loop restoration, superres), one back buffer; a
  frame in flight keeps its back buffer, and whatever the map has dropped since it started
  (a reference it reads that a later frame replaced, or its own buffer if it is not a
  reference), until its GPU work is done. 8 + 2 for the frame being decoded + 2 for the one
  frame in flight next to it is 12: no wait in the common case of one map entry replaced
  per frame. Taking a buffer waits until a frame in flight returns one, so a smaller pool
  costs `wait_fb` time, not output, down to the count that must be there with nothing in
  flight: 11, because a frame that failed after its setup keeps its buffer and its back
  buffer until the next frame is set up, which is after that frame took its own (8 + 2 +
  1). Below 11 the decode thread could wait for a buffer nobody returns. With the shared
  back buffer (the default, part 1 of [`-lowmem`](#smaller-decoder--lowmem)) no frame
  takes a back buffer from the pool, and the same counts are 8 + 1 + 1 = 10 for the
  common case and 8 + 1 + 1 = 10 with nothing in flight: the pool has 10 (and 11, 12).
- **Unchanged**: the GPU work item pool (8), the output image descriptors (16), and the
  player's output buffer pool (`-queue_size`). A frame that is shown holds one output
  buffer while it is in flight, so `-queue_size` still limits how many shown frames can
  be in the pipeline: with `-queue_size 4` and `-slots 4` the output pool, not the slot
  count, is the limit (`wait_output`). A frame that is not shown holds none, and the copy
  of a frame shown with `show_existing_frame` takes an output context instead of a slot;
  see [Hidden frames](#hidden-frames-show_existing_frame).

Memory per slot. The player creates the decoder for 4096x2176, 10-bit with `-nofitsize`,
and for a 3840x2160 10-bit stream as the other column says, which is what an added slot
costs in the test player by default. All of it is D3D12 buffer memory (on the
console that is the same memory as the host's); the host-heap part of a slot is about
0.5 MB. The numbers are computed from the allocation code.

| Per slot, MiB | 4096x2176 (`-nofitsize`) | 3840x2160 (default) |
| --- | --- | --- |
| mode info pool share | 59.5 | 55.8 |
| coefficient buffer share | 36.1 | 33.9 |
| transform block list | 12.8 | 12.0 |
| prediction block map and indexes | 14.1 | 13.2 |
| mode info grid, intra iteration grid | 8.7 | 8.2 |
| CDEF, loop restoration, film grain, command constants | 3.4 | 3.3 |
| **slot** | **134.6** | **126.2** |
| one more frame buffer (with its motion vectors and segment map) | 27.4 | 25.7 |
| **each slot above two** | **162** | **152** |
| palette buffer, from the slot's first frame that allows screen content tools | +25.8 | +24.2 |
| intra v2 schedule buffer, only in a decoder created for `-intrav2` | +10.6 | +10.0 |

A slot used to be 171.0 MiB (198 MiB with its frame buffer): the palette buffer and the
intra v2 buffer were created with every decoder. With [`-lowmem`](#smaller-decoder--lowmem)
a slot is 94.2 MiB at 4096x2176 (mode info share 29.7, coefficient share 25.5) and each
slot above two costs about 120 MiB.

If the creation does not fit the app's memory it is refused before anything is allocated
([budget guard](#budget-guard-memory-refused)); if an allocation fails all the same, the
task ends with `Failed to initialize decoder`, and the log says which allocation it was;
see [Memory lines](#memory-lines-in-the-log-memory).

**On the console in app mode, `-slots 3` and `-slots 4` did not fit** with packages
0.2.4.0 and 0.2.5.0 (4096x2176 decoder, 951 MiB of buffers for three slots); the second
package's log is quoted at the top of this section. Whether three slots fit with the
smaller decoder was open then: computed, a three-slot decoder was 842 MiB at 4096x2176,
790 MiB sized for a 3840x2160 stream and 652 MiB with `-lowmem` on top, and the guard
accepts only the `-lowmem` ones next to a 4-image output pool (see the table under
[What the decoder takes](#what-the-decoder-takes) and the
[budget guard](#budget-guard-memory-refused)). On the console (package 0.2.6.0)
`-lowmem -fitsize -slots 3` was accepted and md5-exact. With the current defaults a
three-slot decoder for a 3840x2160 10-bit stream is 766 MiB and refused; `-lowmem -slots
3` is the 652 MiB one (652.6 now).

### What the decoder takes

D3D12 buffers of the decoder in MiB, computed from the allocation code
(`av1_allocate_buffers` in `libav1/dx/av1_core.cpp`); the first row is what package 0.2.5.0
logged on the console for two slots, and the same formulas for three and four. The host
pool (23.2 MiB at 4096x2176 with two slots) and the player's output pool come on top.

| Decoder | 2 slots | 3 slots | 4 slots |
| --- | --- | --- | --- |
| package 0.2.5.0, 4096x2176 10-bit | 754.7 | 951.4 | 1148.1 |
| **default now, 3840x2160 10-bit stream** | **615.7** | **766.1** | **916.6** |
| default now, 1920x1080 8-bit stream | 128.1 | 165.8 | 203.6 |
| `-lowmem`, 3840x2160 10-bit stream | 540.0 | 652.6 | 765.2 |
| `-lowmemparts 0`, 3840x2160 10-bit stream (`-fitsize` of package 0.2.6.0 and 0.2.7.0) | 639.9 | 790.3 | 940.7 |
| `-lowmemparts 0`, 1920x1080 8-bit stream | 131.3 | 169.1 | 206.8 |
| `-nofitsize`, 4096x2176 10-bit | 656.3 | 816.6 | 977.0 |
| `-nofitsize -lowmemparts 0` (the default of package 0.2.6.0 and 0.2.7.0) | 682.0 | 842.4 | 1002.7 |
| `-nofitsize -lowmemparts 0 -intrav2` | 703.3 | 874.3 | 1045.2 |
| `-nofitsize -lowmem` | 575.5 | 695.5 | 815.5 |

The parts of `-lowmem` alone, two slots at 4096x2176: `-lowmemparts 1` 656.3,
`-lowmemparts 2` 622.5, `-lowmemparts 4` 660.8.

All rows but the first are for the current code (package 0.2.9.0), which has two output
contexts for `show_existing_frame` copies in every decoder: four small buffers, 0.13 MiB
together (see [Hidden frames](#hidden-frames-show_existing_frame)). Packages 0.2.6.0 to
0.2.8.0 logged 0.1 to 0.2 MiB less for the same decoder (681.9 where the table says
682.0, in 36 buffers instead of 40), and those are the numbers in the measurements and
log lines quoted in this document.

The rest of this section and the next ones describe the decoder at 4096x2176 without the
shared back buffer (`-nofitsize -lowmemparts 0`), which is what was measured on the
console when they were written.

What changed against package 0.2.5.0 without any flag (72.8 MiB with two slots):

- **Palette buffer, 25.8 MiB per slot**: it holds the pixels of the frame's palette
  blocks, which the entropy decoder writes in the layout of a frame buffer and
  `reconstruct_block.hlsl` copies into the frame. Its worst case is a whole frame (every
  block of a screen content frame can be a palette block), so it cannot be smaller, and
  the frames in flight write and read their own, so the slots cannot share one. But a
  block can only use a palette in a frame that allows screen content tools
  (`allow_screen_content_tools`, which encoders set for screen content), so the buffer is
  now created when the first such frame gets the slot, and kept. The log says so:

  ```text
  MEMORY palette buffer of slot 0 created for frame 12, the first on the slot that allows screen content tools: 25.8 MiB; device buffers now 707.7 MiB in 37 buffers
  ```

  A stream without screen content never creates them. If the buffer cannot be created
  (the budget guard refuses it, or the allocation fails) the frame is a decode error, not
  a lost device.
- **Intra v2 buffer, 10.6 MiB per slot**: only the `-intrav2` path reads or writes it.
  It is created when the decoder is created for that path
  (`aom_codec_dec_cfg_t::intra_v2`; the player sets it for a task with `-intrav2` and
  creates a new decoder when the flag differs from the task before). In a decoder without
  it, `av1_set_intra_v2(1)` keeps the original intra shaders.

What was looked at and left as it is:

- **Frame buffers** (335.0 MiB: 12 in the pool and one back buffer of 25.8 MiB): the count
  is derived under [Slots](#slots); 12 is what two slots need to not wait in the common
  case. The buffers themselves are the pixels (16 bits a sample at 10-bit, a border of 16
  on each side).
- **Mode info pool** (59.5 MiB per slot): one entry is `MB_MODE_INFO`, 224 bytes, and the
  GPU reads the entries as they are, so the entry cannot shrink without changing the
  shaders. A frame can have one entry per 4x4 unit; a slot's share holds half of that and
  a frame that needs more takes a second slot, so two shares are exactly the worst case
  of one frame. Smaller shares are in `-lowmem` because they no longer cover every
  stream.
- **Coefficient buffer** (36.1 MiB per slot): 24 coefficients per 4x4 unit in the worst
  case (16 luma, 8 chroma), four bytes each; a share holds 17/24 of that and a frame that
  needs more continues in a second slot's share at a superblock boundary, which is why a
  share is more than half. Smaller shares are in `-lowmem`.
- **Residuals, prediction blocks, transform block lists, block maps** (26.0, 25.5 + 9.6,
  12.8 + 12.8 per slot, 14.1 per slot): each is sized for the case that every block of
  the frame is 4x4, which a legal frame can be.

Two overruns were found on the way and are now decode errors or fixed, without changing
what a stream that decoded before decodes to:

- A frame with a tile of at most two superblocks lays its coefficients out for the worst
  case, from its own slot's share on into the next share. When the frame's second slot was
  the one in front of its own, the layout still started in its own share and ran over the
  share behind it (another slot's, or the end of the buffer) for a frame larger than 71%
  of the decoder's size. It now starts in the second slot's share then. With three or four
  slots the two shares may not be neighbours; such a frame is then a decode error
  (`MEMORY FAILED frame N: a tile of at most 2 superblocks ...`). On the default
  4096x2176 decoder a frame has to be larger than 71% of that for either case, which
  `-fitsize` makes the normal case for small streams.
- The mode info entries and the coefficients of a tile are checked against the end of the
  tile's part of the buffer as they are written (`MEMORY FAILED frame: the blocks of a
  tile do not fit ...`, `... the coefficients of a tile do not fit ...`). With the default
  shares neither can happen for a tile of more than two superblocks.

### Smaller decoder (`-lowmem`)

Three reductions. The decoded output is the same with all of them; what is given up is
listed for each. **Part 1, the shared back buffer, is the default** since package
0.2.8.0: it covers every stream, and on the console it was md5-exact on both 4K clips
with no measurable time cost (21.9 / 16.4 fps with `-lowmem -fitsize` against the same
baseline without). Parts 2 and 4 change sizes a stream may depend on and stay opt-in.
`-lowmem` switches on all three, `-lowmemparts N` exactly the parts in `N` (for finding
out which one a result comes from; `-lowmemparts 0` is none, the decoder of package
0.2.7.0). They are a decoder creation parameter (`aom_codec_dec_cfg_t::low_memory`,
`AV1_LOW_MEMORY_*` in `libav1/aom/aom_decoder.h`; there 0 selects the default and
`AV1_LOW_MEMORY_NONE` none), so the player creates a new decoder when the value differs
from the task before.

```text
Low memory parts: 1 (default: 1 shared back buffer)
Low memory parts: 7 (1 shared back buffer, 2 mode info shares, 4 coefficient shares)
Low memory parts: 0 (none, -lowmemparts 0)
```

| Part | Saves, 4096x2176 10-bit, 2 / 3 slots | What it does | Not covered |
| --- | --- | --- | --- |
| 1, shared back buffer | 25.8 / 25.8 MiB | A frame that is filtered after its reconstruction is reconstructed into a back buffer. Instead of a buffer of its own from the pool, every frame uses the same one, beside the pool, and the pool has two buffers less (10 with two slots). | Nothing: only the frame's own command list reads or writes its back buffer, and the command lists of the frames run on the queue one after the other, as they already do for the second back buffer the filters share. Verified on the console (md5-exact, both 4K clips, two and three slots) and the default. A stream without CDEF, loop restoration and superres whose frames replace several references at once may wait for a frame buffer (`wait_fb`) where it did not before. |
| 2, mode info shares | 59.5 / 89.2 MiB | A slot's share of the mode info pool holds a quarter of the worst case instead of half. | A frame takes a second slot (and the pipeline loses a frame of depth for it) as soon as a tile has more blocks than a quarter of its 4x4 units, an average block smaller than 8x8. A tile with more blocks than half of its 4x4 units, an average block smaller than 8x4, is a decode error: `MEMORY FAILED frame: the blocks of a tile do not fit its part of the mode info pool`. |
| 4, coefficient shares | 21.2 / 31.9 MiB | A slot's share of the coefficient buffer holds 12/24 of the worst case instead of 17/24. | A frame takes a second slot once a tile has used half of its worst case. Two shares are exactly the worst case, less what is left over in the first at the superblock boundary where the frame switches: a tile whose coefficients come within one superblock of the worst case (every coefficient of every transform block coded, as in lossless coding of noise) is a decode error: `MEMORY FAILED frame: the coefficients of a tile do not fit its part of the coefficient buffer`. |

A decode error ends the task in the player (`aom_codec_decode error`), like any other.

### Budget guard (`MEMORY REFUSED`)

An app that goes over its memory limit on the console does not get a failed allocation: it
loses its D3D12 device, for the rest of the process (see the `-slots 3` result above). So
the decoder asks before it allocates. The application gives it a callback
(`aom_codec_dec_cfg_t::memory_available_cb`) that returns the bytes the decoder may still
take; the player's returns `AppMemoryUsageLimit - AppMemoryUsage` of
`Windows::System::MemoryManager`, less the textures of its output pool for as long as the
pool has not created them (it creates them at the first frame). Without a callback, or
when the callback does not know (it returns a negative value, as the player's does outside
an app container), nothing is checked.

- **At the creation**: the decoder adds up the device buffers it is about to create, the
  same way it creates them, and is refused when they would leave less than
  `AV1_MEMORY_GUARD_MARGIN`, 48 MiB: headroom for what decoding takes outside the
  decoder's buffers and the output images. Nothing is added for the creation itself; see
  [the margin](#the-margin) below for the numbers. A creation that does not fit is not
  refused at once: the decoder asks the callback again every 20 ms for as long as the
  answer keeps growing (the system returns the memory of a decoder that was just
  destroyed some time after its release), at most 2 s, and stops after 300 ms without a
  gain of 1 MiB (`AV1_MEMORY_GUARD_WAIT_MS`, `AV1_MEMORY_GUARD_SETTLE_MS`).
- **For a palette buffer** created later: refused when it would leave less than
  `AV1_MEMORY_GUARD_MARGIN_BUFFER`, 16 MiB. The frame is then a decode error.

A refusal allocates nothing and leaves the device as it was, so the next task of the
launch runs:

```text
MEMORY decoder size: 4096x2176 10-bit (-nofitsize, the stream's largest frame is 3840x2160 10-bit), slots 3, intra_v2 buffers no, low memory parts 0; device buffers 842.2 MiB and up to 77.3 MiB for palette buffers on first use, host pool N.N MiB
MEMORY output pool estimate: textures of 4 images of 3840x2160 10-bit, 100.9 MiB; held back from what the decoder may take until the pool has them. The decoder's budget guard keeps a margin of 48.0 MiB at its creation
MEMORY app before the decoder: usage N.N MiB of limit 1024.0 MiB, N.N MiB left
MEMORY budget: 890.2 MiB needed and N.N MiB left for the decoder, waited 300 ms for memory the system may still be returning: N.N MiB left now (no longer growing)
MEMORY REFUSED decoder 4096x2176 10-bit, slots 3: its device buffers take 883162112 bytes (842.2 MiB) and the creation must leave a margin of 48.0 MiB, 890.2 MiB together; the application has N.N MiB left for the decoder. Nothing was allocated; the D3D12 device is intact
MEMORY dxgi REFUSED, nothing allocated: local budget N.N MiB, usage N.N MiB, ...
Failed to initialize decoder: Memory allocation error: MEMORY REFUSED decoder 4096x2176 10-bit, slots 3: ...
MEMORY app after the failed decoder creation: usage N.N MiB of limit 1024.0 MiB, N.N MiB left
Unable to create decoder. Error 2
```

```text
MEMORY REFUSED palette buffer of slot 1 (frame 12 allows screen content tools): 27017216 bytes (25.8 MiB) and the margin of 16.0 MiB do not fit the N.N MiB the application has left for the decoder. The frame is a decode error; nothing was allocated and the D3D12 device is intact
```

A creation that is accepted says what was compared:

```text
MEMORY budget: device buffers 681.9 MiB + margin 48.0 MiB = 729.9 MiB, the application has N.N MiB left for the decoder: accepted; palette buffers on first use would add up to 51.5 MiB
```

or `MEMORY budget: not checked (...)` with the reason (also with `-noguard`).

#### The margin

Console, package 0.2.6.0, limit 1024.0 MiB, 4 output images of a 3840x2160 10-bit stream
(estimate 100.9 MiB):

- **The pipeline states take about 84 MiB of the app's memory, once per launch**, and
  they were being created while the first task started. The first task of a fresh launch
  logged `app before the decoder: usage 77.0 MiB`, the creation then waited for the
  pipeline states, and the guard was asked with 160.8 MiB in use (it reported 762.3 MiB
  left for the decoder: 1024.0 - 160.8 - 100.9). The next task of that launch, with no
  decoder created in between, logged `app before the decoder: usage 160.8 MiB`. About 17
  MiB of the 84 are video memory (`dxgi` usage 37.3 MiB before the first creation, 54.2
  MiB with no decoder later in the launch). That the 84 MiB are the pipeline states is
  derived: between the player's line and the guard's question the creation does nothing
  else of any size (it creates a command queue and waits for them). The lines `MEMORY app
  before the pipeline states` and `after the pipeline states` now measure it.
- **The creation itself takes less than its device buffers.** In the launch where the
  default decoder (681.9 MiB of device buffers) was created, the app's memory was 812.7
  MiB after it: about 652 MiB above the 160.8, 30 MiB less than the buffers. `dxgi`
  usage rose by 699.7 MiB in that creation, the 681.9 MiB of buffers and the pipeline
  states' part.
- The margin used to be 84 MiB, from the first line read as "the creation takes 51 MiB
  more than its buffers" (882.7 - 77 - 754.7 in package 0.2.5.0) plus 32 MiB of headroom.
  That counted the pipeline states twice: they are in what the guard sees when it asks,
  and again in the margin. With it **the default decoder was refused in a fresh launch**:
  681.9 + 84.0 = 765.9 MiB against 762.3 MiB left, 3.6 MiB short. An earlier launch had
  accepted the same decoder.
- The margin is now **48 MiB**, all of it headroom. With the numbers above the guard
  accepts device buffers up to 714.3 MiB (762.3 - 48.0):

| Decoder (device buffers, MiB) | 2 slots | 3 slots | 4 slots |
| --- | --- | --- | --- |
| **default now**, 3840x2160 10-bit stream | **615.7 accepted, 98.6 to spare** | 766.1 refused | 916.6 refused |
| `-lowmem`, 3840x2160 10-bit stream | 540.0 accepted | 652.6 accepted | 765.2 refused |
| `-lowmemparts 0`, 3840x2160 10-bit stream | 639.9 accepted | 790.3 refused | 940.7 refused |
| `-nofitsize -lowmemparts 0` (the default when this was measured, 681.9 then) | 682.0 accepted, 32.3 to spare | 842.4 refused | 1002.7 refused |
| `-nofitsize -lowmemparts 0 -intrav2` | 703.3 accepted | 874.3 refused | refused |
| `-nofitsize -lowmem` | 575.5 accepted | 695.5 accepted | 815.5 refused |

  The 762.3 MiB were measured with the host pool of a 4096x2176 decoder in the app's
  usage; a smaller decoder has a smaller host pool, so the rows for a 3840x2160 stream
  have a little more to spare than the table says. With the default `-queue_size 16` a 4K
  10-bit stream leaves about 460 MiB for the decoder and every row is refused; see
  [Defaults and opt-out flags](#defaults-and-opt-out-flags).

  What it protects: with the measured 30 MiB the creation stays under its buffers, a
  decoder at the edge (714.3 MiB) ends at about 946 MiB with its 4 output images, 78 MiB
  under the limit; if a creation took all of its buffers, 48 MiB. The three-slot decoder
  with `-fitsize` (790.2) would end at about 1021 MiB and is refused; so is everything
  larger. The 754.7 MiB decoder of package 0.2.5.0, which ran with about 40 MiB left, would
  be refused too. On the console `-fitsize`, `-lowmem -fitsize` and `-lowmem -fitsize
  -slots 3` were accepted (with the old margin, so with this one as well) and md5-exact.
- The player now waits for the pipeline states before the first task, so `app before the
  decoder` is about 161 MiB in a fresh launch, not 77, and the difference to `app after
  the decoder` is the creation alone.

One launch with three decoders (flags of package 0.2.7.0: `default`, then `-fitsize`, then
`-lowmem -fitsize -slots 3`; now `-nofitsize -lowmemparts 0`, `-lowmemparts 0` and
`-lowmem -slots 3`), computed for the case that the memory of a destroyed decoder comes back (see
[Memory after a decoder is destroyed](#memory-after-a-decoder-is-destroyed)): each
creation starts at about 160 MiB (pipeline states and its own host pool), has about 763
MiB for the decoder next to the 4-image pool, and needs 729.9, 687.7 and 700.4 MiB with
the margin: all three accepted, with 32, 76 and 62 MiB to spare. After the creation and
the pool's textures the app would be at about 913, 870 and 884 MiB (812.7 + 100.5 was
measured for the first), and an md5 run adds 94.9 MiB of readback buffers to each. With
the 350 MiB that stayed charged in package 0.2.6.0 the second and third had about 415 MiB
and were refused, correctly for those numbers.

What the guard does not see: the readback buffers of an md5 or file run are not in the
player's estimate (the measured runs with them added up to more than the limit and still
ran, so counting them would refuse runs that work; the default decoder with a 4-image md5
pool comes to 812.7 + 100.5 + 94.9 = 1008.1 MiB), and a decoder that is reused for a
stream with larger output images does not ask again. The output pool estimate is a
sixteenth above the samples of the images; the console reported 100.5 MiB for the 100.9
estimated.

### Memory lines in the log (`MEMORY`)

Every decoder creation writes lines that start with `MEMORY ` (`grep MEMORY`). The decoder
writes them through `av1_set_log_callback` (`libav1/dx/av1_perf.h`), the player adds the
ones about the app and its output pool. The numbers below are what the allocation code
gives for 4096x2176, 10-bit, two slots and no memory reduction (`-nofitsize -lowmemparts
0`, the default when the lines were recorded); `N` stands for a value that depends on the
run. The same lines of the current default decoder for a 3840x2160 10-bit stream follow
at the end of this block.

Before the decoder is created (player, then the decoder's budget line):

```text
MEMORY decoder size: 4096x2176 10-bit (-nofitsize, the stream's largest frame is 3840x2160 10-bit), slots 2, intra_v2 buffers no, low memory parts 0; device buffers 681.9 MiB and up to 51.5 MiB for palette buffers on first use, host pool N.N MiB
MEMORY output pool estimate: textures of 4 images of 3840x2160 10-bit, 100.9 MiB; held back from what the decoder may take until the pool has them. The decoder's budget guard keeps a margin of 48.0 MiB at its creation
MEMORY app before the decoder: usage N.N MiB of limit N.N MiB, N.N MiB left
MEMORY budget: device buffers 681.9 MiB + margin 48.0 MiB = 729.9 MiB, the application has N.N MiB left for the decoder: accepted; palette buffers on first use would add up to 51.5 MiB
```

The default decoder, the same stream:

```text
MEMORY decoder size: 3840x2176 10-bit (fit to the stream, the stream's largest frame is 3840x2160 10-bit), slots 2, intra_v2 buffers no, low memory parts 1; device buffers 615.7 MiB and up to 48.3 MiB for palette buffers on first use, host pool N.N MiB
MEMORY budget: device buffers 615.7 MiB + margin 48.0 MiB = 663.7 MiB, the application has N.N MiB left for the decoder: accepted; palette buffers on first use would add up to 48.3 MiB
MEMORY decoder created: 3840x2176 10-bit, slots 2, frame buffers 10 + 2 back buffers of 25346048 bytes (24.2 MiB) each; 40 device buffers, 645612288 bytes (615.7 MiB); host pool N of N bytes (N.N MiB) in use
MEMORY decoder created options: intra_v2 buffers no (the decoder was not created for -intrav2); low memory 1 (shared back buffer yes, mode info share 1/2 of the worst case, coefficient share 17/24); palette buffers 0 of 2 created, 24.2 MiB each, a slot's on its first frame with screen content tools
```

`app` is `Windows::System::MemoryManager`: the memory of the whole app and the limit the
system gives it. On the console the D3D12 buffers are part of it. The host pool is
allocated before the `app before the decoder` line, so it is in that usage, and so are
the pipeline states (the player waits for them before the first task). The player
now reads on to the stream's first key frame before it creates the decoder, with or
without `-fitsize`: the sequence header there gives the size and bit depth of the output
images for the estimate. `budget` is the [budget guard](#budget-guard-memory-refused).

After a creation that worked (decoder, then player):

```text
MEMORY decoder created: 4096x2176 10-bit, slots 2, frame buffers 12 + 1 back buffer of 27017216 bytes (25.8 MiB) each; 36 device buffers, 715036928 bytes (681.9 MiB); host pool N of N bytes (N.N MiB) in use
MEMORY decoder created options: intra_v2 buffers no (the decoder was not created for -intrav2); low memory 0 (shared back buffer no, mode info share 1/2 of the worst case, coefficient share 17/24); palette buffers 0 of 2 created, 25.8 MiB each, a slot's on its first frame with screen content tools
MEMORY decoder created by type: DeviceOnly 7 buffers 412.0 MiB; DeviceUpload 14 buffers 32.3 MiB; HostRW 9 buffers 164.6 MiB; ReadBack 0 buffers 0.0 MiB; DeviceOnlyConst 5 buffers 0.8 MiB; UploadUAV 1 buffers 72.2 MiB; textures 0
MEMORY decoder created per slot: 141107968 bytes (134.6 MiB) = 11 own buffers 38.9 MiB + mode_info_pool share 59.5 MiB + idct_coefs share 36.1 MiB; a slot above 2 also adds one frame buffer, 25.8 MiB
MEMORY decoder created largest: frame_buffer_pool 335.0 MiB DeviceOnly; mode_info_pool 119.0 MiB HostRW; idct_coefs 72.2 MiB UploadUAV; idct_residuals 26.0 MiB DeviceOnly; prediction_blocks 25.5 MiB DeviceOnly; idct_blocks 12.8 MiB DeviceOnly; idct_blocks_unordered 12.8 MiB DeviceUpload (per slot); prediction_blocks_warp 9.6 MiB DeviceOnly; gen_block_map 7.7 MiB HostRW (per slot); gen_mi_block_indexes 6.4 MiB HostRW (per slot)
MEMORY dxgi before the decoder's allocations: local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available; non-local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available
MEMORY dxgi after the decoder's allocations: local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available; non-local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available
MEMORY app after the decoder: usage N.N MiB of limit N.N MiB, N.N MiB left
```

Package 0.2.5.0 logged 40 device buffers and 754.7 MiB here (per slot 171.0 MiB in 13 own
buffers); the difference is the palette and intra v2 buffers, see
[What the decoder takes](#what-the-decoder-takes).

- **options**: what the decoder was created with, and how many palette buffers exist at
  that moment (none right after a creation).

- **by type**: the decoder's memory types (`MemoryType` in `libav1/dx/av1_memory.h`).
  `DeviceOnly` and `DeviceOnlyConst` are default-heap buffers the CPU never maps;
  `DeviceUpload` is the upload heap; `HostRW` and `UploadUAV` are custom heaps the CPU
  reads and writes. The decoder creates no textures and no readback buffers; those belong
  to the player's output pool.
- **per slot**: what one pipeline slot takes, the **slot** row of the table above. The
  names are the fields in `libav1/dx/types.h`.
- **largest**: the ten largest buffers; one that every slot has is listed once.
- **dxgi**: `IDXGIAdapter3::QueryVideoMemoryInfo` for the adapter of the decoder's device,
  local and non-local segment group. If the adapter or a query is not available the line
  says so with the HRESULT and the creation goes on.
- The host pool is ordinary memory of the player (`std::vector`), not D3D12 memory.

The player's output images are allocated at the first frame of a size, all free images of
the pool at once:

```text
MEMORY output pool: allocated for 3840x2160 10-bit frames: textures of 4 images, N.N MiB; 0 readback buffers, 0.0 MiB; pool of 4 images
```

A 3840x2160 10-bit image is three textures of about 23.7 MiB together, so the default
`-queue_size 16` is about 380 MiB and `-queue_size 4` about 95 MiB; an md5 or file run
adds a readback buffer per image.

When an allocation fails, one line names it (the example is a third slot's last buffer): the buffer, the slot it belongs to, its size,
memory type and heap, the HRESULT, and what had been allocated before it. Then follow the
totals up to the failure, the video memory before the creation, at the failure and after
the failed decoder was released, and the player's lines; the first `MEMORY FAILED` line
is repeated as the detail of `Failed to initialize decoder`:

```text
MEMORY FAILED buffer filmgrain_rand_offset of slot 2: 36608 bytes (0.0 MiB), type DeviceUpload, heap UPLOAD, CreateCommittedResource hr 0x8007000E; allocated before it: N bytes (N.N MiB) in 46 buffers
MEMORY FAILED decoder creation at: decoder buffers (the MEMORY FAILED line above names the one)
MEMORY FAILED decoder, allocated up to the failure: 4096x2176 10-bit, slots 3, frame buffers 13 + 1 back buffer of 27017216 bytes (25.8 MiB) each; 46 device buffers, N bytes (N.N MiB); host pool N of N bytes (N.N MiB) in use
MEMORY FAILED decoder, allocated up to the failure options: ...
MEMORY FAILED decoder, allocated up to the failure by type: ...
MEMORY FAILED decoder, allocated up to the failure per slot: ...
MEMORY FAILED decoder, allocated up to the failure largest: ...
MEMORY dxgi FAILED, before the creation: local budget N.N MiB, usage N.N MiB, ...
MEMORY dxgi FAILED, at the failure: local budget N.N MiB, usage N.N MiB, ...
MEMORY dxgi FAILED, after releasing the failed decoder: local budget N.N MiB, usage N.N MiB, ...
Failed to initialize decoder: Memory allocation error: MEMORY FAILED buffer filmgrain_rand_offset of slot 2: 36608 bytes (0.0 MiB), type DeviceUpload, heap UPLOAD, CreateCommittedResource hr 0x8007000E; allocated before it: N bytes (N.N MiB) in 46 buffers
MEMORY app after the failed decoder creation: usage N.N MiB of limit N.N MiB, N.N MiB left
Unable to create decoder. Error 2
```

On the console the failure that was seen is not a failed allocation but a removed device:
all buffers of the three-slot decoder were created and the first GPU work after them
failed.

```text
MEMORY FAILED decoder creation at: upload of the lookup tables (hr 0x887A0005)
```

`0x887A0005` is `DXGI_ERROR_DEVICE_REMOVED`. After it every creation in the same launch
ends with `MEMORY FAILED decoder creation at: D3D12 device or command queue (hr
0x887A0005)`: the player cannot get the device back, the app has to be started again.

Other `MEMORY FAILED` lines: `host pool` (the pool the player allocated is too small for
the configuration), `compute pipeline state N` (a shader), `decoder creation at: <step>`
for the command allocators, fences, command list and the lookup table upload,
`output image texture` / `output image readback buffer` from the player's output pool,
and during decoding `buffer palette_buffer of slot N` and `frame ...` (a frame that does
not fit a buffer; see [What the decoder takes](#what-the-decoder-takes) and
[`-lowmem`](#smaller-decoder--lowmem)). A creation or a palette buffer that the
[budget guard](#budget-guard-memory-refused) turns down is `MEMORY REFUSED`.

A creation that fails releases everything it had created before it returns, so the next
task starts as it would after a fresh launch: `usage` in the `dxgi ... after releasing`
line should be back at the value of `before the creation`. The player also releases the output
pool of the decoder before, ahead of creating the new one. What the app is charged after a
decoder that was in use is destroyed is a different matter, see the next section.

### Memory after a decoder is destroyed

**Console, package 0.2.6.0, app mode (2026-11-11).** One launch ran several tasks. The
first two shared the default decoder (4096x2176 10-bit, two slots, md5 check, 300 frames
each); the third needed another decoder (`-fitsize`), so the player destroyed the first
and released its output pool:

```text
task 1  MEMORY app before the decoder: usage 76.6 MiB of limit 1024.0 MiB
        MEMORY dxgi before the decoder's allocations: usage 37.3 MiB
        MEMORY dxgi after the decoder's allocations: usage 737.0 MiB
        MEMORY app after the decoder: usage 812.7 MiB
        MEMORY output pool: textures of 4 images, 100.5 MiB; 4 readback buffers, 94.9 MiB
task 3  (Re)create decoder
        MEMORY app before the decoder: usage 507.7 MiB of limit 1024.0 MiB
        MEMORY dxgi REFUSED, nothing allocated: usage 54.2 MiB
        MEMORY REFUSED decoder ...
        MEMORY app after the failed decoder creation: usage 485.9 MiB
tasks 4 to 7: the same, app usage 486 to 508 MiB, dxgi usage 54.2 MiB
```

Package 0.2.5.0 showed the same without md5 and after only 30 frames: 503 MiB before the
second decoder. So the video memory was back (54.2 MiB, the 37.3 MiB of the start and the
pipeline states), and the app was still charged for much more than at the start. How much:
without a decoder, a host pool and a stream the app uses about 138 MiB once the pipeline
states exist (160.8 MiB with a 23.2 MiB host pool, see [the margin](#the-margin)); the
485.9 MiB above are that state. **About 348 MiB stayed charged** (not 415: the pipeline
states are 84 MiB of the difference to the 76.6 MiB of the launch's first line). The
guard's refusals were right for these numbers; the numbers are the problem. It does not
grow with the frames (30 or 600) and does not depend on md5.

**What the code says.** Everything the decoder and the player allocate was followed to
its release:

| Candidate | Finding |
| --- | --- |
| The decoder's D3D12 buffers | All are in the allocator's list and released by `av1_memory_allocator::release()` when the decoder is destroyed; `dxgi` usage confirms it. Nothing else holds a reference to them. |
| Command list, command allocators, fences, timing query heaps | `ComPtr` members of `Av1Core`, released by its destructor, which `av1_destroy_gpu_decoder` runs. The events of the slots are closed. |
| Command queue | The player gives the decoder no queue, so every decoder creates its own and releases it when it is destroyed. |
| D3D12 device | The player's, shared with its renderer by design; the decoder holds a reference and drops it. Destroying a decoder must not and does not release it. |
| Pipeline states and root signatures | One set per launch (`av1_create_pipeline_cache_handle`), shared by all decoders by design, released by `av1_destroy_pipeline_cache_handle` at the end of the launch. About 84 MiB of the app's memory; this is what a first task "kept" after a refused creation (77.0 MiB before the decoder, 160.8 MiB before the next task's), not the stream reader. It is now logged (`app before` / `after the pipeline states`). |
| Threads | The GPU completion thread is joined by the drain and again by the destroy; the tile workers are ended in `av1_decoder_remove`; the pipeline state threads are joined. |
| libaom-side memory | The fork allocates none on the heap: the decoder structure, tile data, thread data, contexts, motion vectors and segment maps are in the host pool (`host_allocate`), the frame buffers are D3D12 buffers. `aom_malloc` has no caller left on the decode path (`cb_buffer_base`, the restoration and loop filter buffers of libaom are not allocated). |
| Host pool | A `std::vector` of the player, 23.2 MiB; freed before the next decoder's is allocated. |
| Stream reader | Reads on a thread, at most 20 frames ahead, each in a buffer of its own that is reused; it does not read the whole file. The reader, its buffers and the file are locals of the task and are gone when it returns. A few MiB while a task runs. |
| Output pool | Textures and readback buffers are `ComPtr`s of the pool, released with it before the next decoder is created; `dxgi` usage confirms it. |
| md5 lists, timing records | Small (32 bytes and about 0.5 KB a frame). They were kept until the next task replaced them; they are now released at the end of their task. |
| An event handle in `upload_helper` | Never closed; the function has no caller. Closed now. |

So nothing in the code keeps 348 MiB, and the adapter agrees that the buffers are gone.
What is left is how the system charges the app:

1. **The memory comes back late.** D3D12 does not free a buffer in `Release`: the
   destruction is queued in the system's video memory manager, which finishes it later
   ("the call may return prior to the underlying surface memory being fully reclaimed",
   WDK, `D3DDDICB_DESTROYALLOCATION2FLAGS`). The player measured within milliseconds of the
   release, and the refused tasks that followed took no time either. The new
   `MEMORY app settle after the release` line says whether and after how long it comes
   back. If this is it, the wait the player and the guard now do is the fix.
2. **The system keeps it** for the process, for reuse or for good. The decoder's
   default-heap buffers, which the CPU never maps, are 412.8 MiB (the frame buffer pool
   alone 335.0); the buffers the CPU maps are 269.1 MiB. The `MEMORY destroy released
   <type>` lines say which release does not raise what the app has left. A `-noguard`
   task then tells reuse from loss: if the next decoder is created and `app after the
   decoder` is about where it was after the first one, the memory was reused and the
   guard must not count it; if the creation loses the device, it is lost.
3. **It is taken while decoding**, by something outside the decoder's buffers, and not
   given back. The `MEMORY app decoding, first frame out` and `30 frames out` lines say
   so: they should be `app after the decoder` plus the output pool (100.5 MiB of textures,
   and 94.9 MiB of readback buffers in an md5 run).

**What changed.**

- The player logs the app's memory and the video memory at every step (list below).
- The decoder logs its own release: `MEMORY destroy <step>`, one line at the start, one
  after the GPU thread is joined, one after the buffers of each memory type are released
  (the mapped ones first, the default-heap ones last), one at the end. Each has the
  `dxgi` usage and what the app's budget callback says is left.
- After a decoder and its output pool are released, and before the next host pool is
  allocated, the player waits until the app's memory is within 16 MiB of what it was at
  the first task of the launch, at most 2 s, checking every 20 ms.
- The budget guard no longer refuses at once (see [Budget guard](#budget-guard-memory-refused)).
- The pipeline states are waited for before the first task
  (`av1_wait_pipeline_cache_handle`, new in `aom/aom_decoder.h`).
- The task's md5 state and timing records are released at the end of the task; the md5
  file being saved is closed there.

None of this touches a frame: the decoded output is the same.

**The `MEMORY app <where>` lines.** Every `app` line is followed by a `MEMORY dxgi
<where>` line with the same text; `grep "MEMORY app"` gives the app's memory along a
launch. In the order they appear:

| `<where>` | When |
| --- | --- |
| `before the pipeline states`, `after the pipeline states` | Once per launch, around the creation of the compute pipeline states. |
| `task start` | Before the task opens its stream. |
| `stream reader at the first key frame` | The reader thread runs, the first key frame is read. |
| `before the decoder is destroyed`, `after the decoder was destroyed` | Around `aom_codec_destroy`, when the task needs another decoder, or at the end of a task that failed or had `-r`. The decoder's `MEMORY destroy` lines are in between. |
| `before the output pool is released`, `after the output pool was released` | The textures and readback buffers of the decoder before. |
| `after the host pool was released` | The player's host pool of the decoder before. |
| `settle after the release` | A line of its own, see below; then `after the settle wait`. |
| `before the decoder`, `after the decoder`, `after the failed decoder creation` | Around the creation, as before (no `dxgi` twin: the decoder's own `dxgi` lines are there). |
| `decoding, first frame out`, `decoding, 30 frames out` | From the decode loop, after the decode call that follows the first and the 30th output frame. The output pool has its images at the first. |
| `before the drain`, `after the drain` | Around the flush that waits for the last frames. |
| `after the last frame left the output queue` | Nothing of the task is in flight or queued. |
| `after the task's md5 and timing records were released` | |
| `task end, stream reader closed` | The reader, its buffers and the file are gone. |

```text
MEMORY app before the decoder is destroyed: usage N.N MiB of limit 1024.0 MiB, N.N MiB left
MEMORY dxgi before the decoder is destroyed: local budget N.N MiB, usage N.N MiB, ...
MEMORY destroy start: dxgi local usage N.N MiB; the application has N.N MiB left for a decoder
MEMORY destroy GPU thread joined, buffers not yet released: dxgi local usage N.N MiB; the application has N.N MiB left for a decoder
MEMORY destroy released DeviceUpload, 14 buffers, 32.3 MiB: dxgi local usage N.N MiB; the application has N.N MiB left for a decoder
MEMORY destroy released HostRW, 9 buffers, 164.6 MiB: ...
MEMORY destroy released UploadUAV, 1 buffers, 72.2 MiB: ...
MEMORY destroy released DeviceOnlyConst, 5 buffers, 0.8 MiB: ...
MEMORY destroy released DeviceOnly, 7 buffers, 412.0 MiB: ...
MEMORY destroy end, command queue, list and allocators released: dxgi local usage N.N MiB; the application has N.N MiB left for a decoder
MEMORY app after the decoder was destroyed: usage N.N MiB of limit 1024.0 MiB, N.N MiB left
MEMORY app before the output pool is released: ...
MEMORY app after the output pool was released: ...
MEMORY app after the host pool was released: ...
MEMORY app settle after the release: usage N.N MiB -> N.N MiB in N ms; without a decoder the app used N.N MiB at the first task of the launch, N.N MiB more are charged now (returned)
MEMORY app after the settle wait: ...
```

The settle line ends with `(returned)` or `(not returned within the wait)`. "What the
application has left" in the `destroy` lines is the player's budget callback: the limit
less the usage, less the output pool textures that do not exist yet. Outside an app
container the `app` lines are missing and the `destroy` lines say that it is not known.

The lines cost a few milliseconds each (a file append, and a DXGI factory for the `dxgi`
one). All but two are outside the frames of a task; `decoding, first frame out` and `30
frames out` delay the decode thread once each, the first inside the warm-up frames of a
timing run.

**The test for case 2** (`-noguard`), in one launch:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 60 -queue_size 4 -nopresent -cdefv2
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 60 -queue_size 4 -nopresent -cdefv2 -fitsize -noguard
```

Only needed if the settle line of the second task says `not returned`. The second
creation is then not checked: either `MEMORY app after the decoder` shows the app about
where the first decoder left it (the memory was reused), or the creation fails with
`0x887A0005` and the launch is over (it was not).

### Decoder size (`-nofitsize`)

The player creates the decoder for what the sequence header at the stream's first key
frame announces: the largest frame size (`max_frame_width`, `max_frame_height`), rounded
up to a multiple of 128 as the decoder rounds it, and 8-bit buffers for an 8-bit stream.
That is 3840x2176 10-bit for a 3840x2160 10-bit stream, the second column of the slot
table (about 42 MiB less than 4096x2176 for two slots, 52 MiB less for three), and 128
MiB instead of 656 for a 1920x1080 8-bit stream. The decoded output is the same. This is
the default since package 0.2.8.0 (on the console: md5-exact on both 4K 10-bit clips);
before, it was the flag `-fitsize`, which is still accepted and changes nothing. With
`-nofitsize` the decoder is created for 4096x2176, 10-bit, whatever the stream is, as
upstream's player does.

```text
MEMORY decoder size: 3840x2176 10-bit (fit to the stream, the stream's largest frame is 3840x2160 10-bit), slots 3, intra_v2 buffers no, low memory parts 1; device buffers 766.1 MiB and up to 72.5 MiB for palette buffers on first use, host pool N.N MiB
Decoder size: 3840x2176 10-bit (fit to the stream, default)
```

With `-nofitsize` the first line says `(-nofitsize, the stream's ...` and the second reads
`Decoder size: 4096x2176 10-bit (fixed, -nofitsize)`.

**The flag was wrong in package 0.2.5.0**: for a 3840x2160 stream it logged `the stream's
largest frame is 32x128` and created a 128x128 decoder. The cause was the sequence header
reader behind `aom_codec_peek_stream_info` (`decoder_peek_si_internal` in
`libav1/av1/av1_dx_iface.c`, from upstream): after `reduced_still_picture_header` it read
the operating points at once and left out `timing_info_present_flag` (with the timing and
decoder model info behind it), `initial_display_delay_present_flag`, and the per-operating-
point decoder model and display delay fields. In a header without timing info that is two
bits early: the size field widths and then the sizes were read from the middle of the
level and of the fields before them, which gives exactly 32x128 for an SVT-AV1 4K header
(level 5.1, 12-bit size fields). The decoder itself never used those two values; it reads
the header with its own parser. The peek now walks the header the way that parser does
(`libav1/av1/common/seq_header_peek.h`: timing info, decoder model info, every operating
point, the frame size, and on to the bit depth), stays inside the sequence header OBU and
reports a header that ends too early. `aom_codec_stream_info_t` has a `bit_depth` field
for it.

The decoder's own parser (`read_sequence_header_obu`) is not used for the peek: it writes
into the decoder's state, reports errors with `longjmp` and needs a decoder to exist.

- An 8-bit stream gets 8-bit buffers (frame buffers of half the size); a 10-bit or 12-bit
  stream, or a header the peek cannot read, keeps 10-bit ones. An 8-bit decoder refuses a
  10-bit frame that does not fit its buffers, as a decoder refuses a frame that is too
  large.
- A stream larger than 4096x2176 in either direction gets 4096x2176 (and fails at its
  first frame, as with `-nofitsize`).
- The player creates a new decoder when the size or the bit depth differs from the one it
  has, as for `-slots`.
- Every frame of a sequence is within the announced size (frame size override, superres
  and reference scaling included), so the buffers fit. A later sequence header in the same
  file may announce a larger size; the decoder now refuses a frame that is wider or higher
  than it was created for (decode error `Failed to allocate frame buffer`) instead of
  only comparing the byte size of a frame buffer. That check is on with and without the
  flag.
- Verified on the console for 3840x2160 10-bit streams (md5-exact, packages 0.2.6.0 and
  0.2.7.0). Other sizes, and the 8-bit buffers of an 8-bit stream, have not been run
  there yet: check such a stream with `-md5_frame_check` against a
  [dav1d reference](#md5-references-from-dav1d), and with `-nofitsize` if it fails.

#### Host-side check of the peek

`testing/seq_header_check` (not part of the player build) writes sequence headers bit by
bit from the syntax tables of the AV1 specification, with known values, and checks what
the peek returns: the header of an SVT-AV1 4K 10-bit encode (for which the reader before
the fix is shown to return 32x128), timing info with and without decoder model info and
with the shortest and longest `uvlc`, one, three and 32 operating points with their own
decoder model and display delay fields, frame size fields from 1 to 16 bits wide, the
reduced still picture header, the three profiles with 8, 10 and 12 bits, and
4000 random combinations. Every header is also cut at every length (the peek must report
it and not read past the end), and wrapped into OBUs that are found again with the
decoder's OBU header reader (`libav1/av1/common/obu_util.c`), with an extension header and
with payload sizes of one to three bytes.

```sh
sh testing/seq_header_check/run.sh --mutate
```

`--mutate` also builds five copies of the peek with one read of the syntax left out
(among them the two flags the old reader did not know) and requires the check to reject
each. The workflow runs it in the `intra-check` job.

### Clears

Per frame, `av1_setup_frame` clears two buffers of the slot it has taken: the mode info
grid (one pointer per 4x4 block, set to zero; 4.0 MiB for a 3840x2160 frame) and the
intra iteration grid (set to -1; 3.5 MiB). The GPU reads both, so they cannot be cleared
before the frame that used the slot has completed. With `-asyncclear` the completion
thread does it right there: after the frame's fence and before the slot is pushed back to
the pool. The slot is not visible to the decode thread during that time, and nothing on
the GPU uses it any more.

The size of both clears depends on the frame that will use the slot next, which the
completion thread does not know. The rule:

- The completion thread clears exactly the bytes the frame's own setup cleared, and
  records the two sizes in the slot.
- The next frame skips a clear only if the recorded size is exactly the size it would
  clear itself. The grid is then byte for byte what the clear on the decode thread would
  have left.
- In every other case the decode thread clears as before: first use of a slot, a frame
  of another size, a slot that came back without a completed frame (decode error).

Not moved, because they are small or depend on the next frame:

- the per-tile clears (`memset_tile`): the tile descriptor and the head of the tile's
  block map, a few KB per tile;
- with `-intrav2`: the intra iteration grid is not cleared at all by frames on the new
  path, and the schedule's cell map is cleared once every 255 frames of a slot. The few
  frames that keep the original path (intra block copy) clear the iteration grid on the
  decode thread;
- the coefficient buffer: there is no per-frame clear to move. The entropy decoder
  writes only non-zero coefficients, and every IDCT shader stores zeros back over the
  coefficients it has read, so a slot's share is clean again when its frame completes.
  The only host-side clear is for a frame that was set up but never submitted, and it
  stays where it was.

With `-gputime` the summary shows both sides:

```text
GPUTIME cpu memsets (frame + tile)                ...                                        N.N MB cleared per frame
GPUTIME cpu clears, completion thread             ...                                        N.N MB cleared per frame
```

The first line is what is left on the decode and tile threads (`memset`, `memset_tile`),
the second the moved work (`async_clear`, measured on the completion thread and booked on
the frame whose completion triggered it). `setup` and `frame` shrink by what moved.
`wait_slot` now includes waiting for a slot that is being cleared: if the decode thread
needs a slot at the moment its frame completes, it waits for the clear instead of doing
it, and nothing is gained for that frame. The gain is in the frames where the slot came
back early.

**Console result (2026-11-11, package 0.2.5.0, two slots, both 4K clips): md5-exact, no
gain.** `cpu memsets` on the decode thread went down by the 3.3 to 4.3 ms per frame that
moved, `cpu wait_slot` went up by 1.5 to 3.7 ms, and the completion thread spent 3.6 to
3.9 ms per frame on the clears. With two slots and a decode thread that is the slower
side, the slot a frame needs is the one that has just completed, so the decode thread
waits for the clear it used to do itself. Runs with the same settings differ by about
1 fps on the SVT-AV1 clip, so the remainder cannot be told from noise. The flag stays off
by default; it can only pay with a slot to spare, which needs the third slot to fit.

### Timing: the four combinations

All four lines in `args_av1.txt`, same clip, all with `-cdefv2` (the lines as they were
run; the flag is the default now, and so are `-fitsize` and part 1 of `-lowmem` in the
lines further down):

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -slots 3
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -asyncclear
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -slots 3 -asyncclear
```

The second and fourth task recreate the decoder (the slot count changes); the third does
too, going back to two slots. Compare in the four summaries:

- `wall between decoded frames` and the "per OUTPUT frame" line: the result;
- `cpu wait_slot`, `cpu wait_sec_slot`, `cpu wait_fb` and `gpu idle gap before the frame`:
  what a deeper pipeline is meant to remove;
- `cpu memsets (frame + tile)` against `cpu clears, completion thread`, and `cpu setup`:
  what `-asyncclear` moved;
- `cpu wait_output` and the output-buffer wait lines: if these grow with `-slots`, the
  output pool has become the limit.

`gpu busy` and the GPU stage table should not change: the flags do not touch what the
GPU does.

With package 0.2.5.0 the two `-slots 3` tasks did not run (the creation removed the
device, and the tasks after them failed with it). A third slot needs the smaller decoder:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -lowmem -fitsize
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -lowmem -fitsize -slots 3
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent -cdefv2 -lowmem -fitsize -slots 3 -asyncclear
```

The first line against the plain `-cdefv2` task shows what `-lowmem` costs with two slots
(`cpu wait_fb`, `cpu wait_sec_slot` and the `sec=1` frames are where it would show). A
task whose decoder does not fit is refused with `MEMORY REFUSED` and the next one runs.

### Same output: md5 of every frame

As for [CDEF](#same-output-md5-of-every-frame), **in separate launches**, or with a
[dav1d reference](#md5-references-from-dav1d) and only the check task. First launch,
the reference with the default pipeline (no `-slots`, no `-asyncclear`):

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -save_md5 /streams/<clip>.ivf.md5
```

Let the app exit, then a second launch with only the check task:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -cdefv2 -slots 3 -asyncclear -md5_frame_check
```

The rules of the CDEF section apply unchanged: the same `-limit` in both tasks, the
`.md5` file next to the stream, no `-gputime`, no `-try10x3`. A reference file saved for
another check of the same clip and `-limit` can be reused. The task ends with `MD5 is OK`
or a list of bad frames. Check `-slots 4 -asyncclear` the same way if it is to be timed,
and include a clip that takes the second slot (`sec=1` in a `-gputime -progress` run of
the same clip) and one whose frame size changes.

The memory changes need the same check, each as its own launch against the same
reference: the default decoder, `-nofitsize`, `-lowmemparts 0`, `-lowmem`, and `-nofitsize
-lowmemparts 0` (the decoder of package 0.2.7.0); `-lowmemparts 1`, `2` and `4` tell the
parts apart if `-lowmem` fails. A clip with screen content (palette
blocks) checks the palette buffer that is created on first use; the two 4K clips have
none.

## Hidden frames (`show_existing_frame`)

Most AV1 encodes have frames that are decoded but not shown at once: alt-refs, and with a
hierarchical (random-access) structure about half of all frames. Such a frame is shown
later by a frame header with `show_existing_frame`, which decodes nothing and names the
reference to output.

**The hang (up to package 0.2.8.0).** The decoder made the output of a hidden frame while
it decoded the frame: `av1_postprocess_copy_output` (`libav1/dx/loopfilters.cpp`) took an
output buffer from the player for every frame with `show_frame` *or* `showable_frame` and
recorded the copy into it, and the buffer stayed with the frame until its
`show_existing_frame`, or until the frame left the reference map. An SVT-AV1
random-access stream decodes, after its key frame, the hidden frames 16, 8, 4 and 2 of
the first group before the first shown frame. With `-queue_size 4` those four held the
whole pool, and the next frame waited in the player's callback for a buffer that only a
later `show_existing_frame` could have handed over:

```text
Got first frame (0) 3840 x 2160 x 10
GPUTIME output-buffer wait #1 BLOCKED: pool 4, free 0, held by decoder 4, queued or on display 0
```

A stream with fewer hidden frames at a time did not hang, but the buffers they held were
missing from the pool (a 1080p stream with 207 hidden frames of 639 hit the limit 23
times, with waits up to 25 ms). A larger `-queue_size` only moves the point where it
stops.

**The fix (package 0.2.9.0).** An output buffer is taken only for a frame that is output,
when it is output:

- A frame with `show_frame`: as before, at the end of its own command list.
- A frame without `show_frame`: nothing. No output buffer, no output copy, no film
  grain. Its frame buffer is in the reference map like any other reference.
- `show_existing_frame` (`av1_decode_sef` in `libav1/dx/av1_core.cpp`): takes the output
  buffer then and submits a command list of its own that copies the frame buffer of the
  referenced frame into it, with film grain. Upstream had this path for a frame that is
  shown a second time; it is now the only one.

So the decoder never holds more output buffers than there are frames that are output and
still in flight on the GPU, and each of those is handed to the player without the decoder
needing another one. The player's pool can no longer be exhausted by the stream's
structure; `-queue_size` limits how many output frames are in flight or queued for
display, as it does for a stream without hidden frames.

Why the copy is right:

- **Order on the GPU.** The decode thread submits the command list of a frame before it
  reads the next frame header, and all lists run on one command queue in the order they
  were submitted. The copy of a `show_existing_frame` is submitted after the list that
  decoded the frame, so it reads the finished frame. Nothing writes a frame buffer after
  the frame's own list (borders included), so the copy reads what an output copy at the
  end of the frame's decode would have read.
- **Lifetime of the frame buffer.** The copy takes a reference to the frame buffer while
  the reference map still holds the frame, and the completion thread gives it back after
  the fence of the copy's list. Until then the buffer cannot return to the pool, so no
  later frame can be decoded into it, whatever the frames that follow do to the reference
  map. (A frame in flight holds its references the same way.)
- **Film grain.** The header reader loads the film grain parameters that were stored with
  the frame (`cm->film_grain_params = frame_to_show->film_grain_params`, the
  `load_grain_params` of the specification, random seed included) before it calls the
  copy, also when the frame shown is a key frame and the reference map is refreshed with
  it. The copy applies those. A hidden frame that is showable carries its parameters in
  its own header, so they are the ones an output copy at decode time would have used.
- **Output order and timestamps.** The work item of the copy is queued behind those of
  the frames before it, and the completion thread hands the images to the player in that
  order: output is in the order in which the bitstream shows frames. An output carries
  the `user_priv` of the `aom_codec_decode` call whose data contained the shown frame or
  the `show_existing_frame` header.
- **Staging buffer.** Every output copy goes through the one back buffer the filters
  share; each list writes it and reads it back before it ends, and the lists do not
  overlap on the queue.

**Output contexts.** The copy does not take a pipeline slot: it would wait behind the
frames in front of it on the queue, and for that time one frame less could be in flight,
at every second frame of a random-access stream. The decoder has two output contexts
instead (`OutputThreadDataCount` in `libav1/dx/types.h`): a command allocator and a fence
each, 32 KiB for the film grain constants and a buffer for the film grain offsets, four
buffers of 0.13 MiB together at 3840x2176. A third `show_existing_frame` while two copies
are in flight waits for the first (`wait_slot` in its record). A copy in flight keeps the
frame buffer it reads out of the frame buffer pool, like a reference of a frame in
flight; that can cost a wait for a frame buffer (`wait_fb`), not a deadlock.

**Timing lines.** A copy has a `-gputime` record of its own (`sef=1` in the frame lines,
stages `film_grain`, `copy_plane`, `output_copy`). In the summary the copies have their
own line and are not in the per-frame lines; see [Log lines](#log-lines). A hidden frame
no longer has `copy_plane` and `output_copy` stages, so its GPU time is that much
shorter and the copy's is that much longer: the work per output frame is the same.

**What does not change.** A stream in which every frame is shown never reaches
`av1_decode_sef` and its frames take the path they took: same command lists, same output
buffers, same order. The only difference for it is the 0.13 MiB of the two idle output
contexts.

**Not run on the console yet** (the change was made without one). To check, each against
a [dav1d reference](#md5-references-from-dav1d), since the original path cannot decode
these streams and so cannot write one:

```text
-i /streams/<random-access 4K clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -md5_frame_check
-i /streams/<random-access 4K clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -lowmem -slots 3 -md5_frame_check
-i /streams/<random-access 4K clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -gputime -nopresent
```

- `MD5 is OK` on the random-access 4K clip (the one that hung), on a 1080p clip with
  hidden frames, and unchanged on the two low-delay 4K clips; each with two slots and
  with `-lowmem -slots 3`. One md5 task per launch.
- A clip with film grain and hidden frames, if one is at hand: the grain of a frame shown
  with `show_existing_frame` is the part of this that no clip has exercised here.
- In the `-gputime` run: no `BLOCKED` line without its `resumed` line, "held by decoder"
  never above the slots plus one, `frames decoded` and `output` as dav1d counts them, and
  the `show-existing copies` line (their GPU time should be the `copy_plane` and
  `output_copy` time of a shown frame).

## Global motion

**The bug (upstream; in every package before this change).** A 1080p 10-bit 4:2:0 stream (4 tile
columns, many hidden frames, no film grain) decoded md5-identical to dav1d except in the
frames with global motion and in the frames predicted from them. The first one is output
frame 525: an inter frame with a rotzoom model on ALTREF (`is_global[7]`,
`is_rot_zoom[7]`), the only frame with global motion among the first 600 decoded, and the
only wrong output among them. Against dav1d, 584 luma samples differ (by up to 436 of
1023), all inside x 1852..1890, y 620..644, and about 140 samples of each chroma plane in
the same place. The same wrong md5 came out with `-cdefv1`, `-nofitsize`,
`-lowmemparts 0`, and with two and three slots.

**What the samples say.** The block there is a 32x16 `GLOBAL_GLOBALMV` block with a wedge
mask, at x 1856, y 624: LAST, which has no global motion in that frame, and ALTREF with
the rotzoom model. On one side of the wedge the decoder's output is the warped ALTREF
prediction, as in dav1d. On the other side it is not the block of LAST at x 1856, y 624,
but the top left corner of LAST, sample for sample: a run of 11 equal samples, then 4 one
lower, then 4 one lower again, which is how every row of that corner runs. So the prediction of
the reference that is not warped read `LAST[y - 624][x - 1856]`: the position of the block
was lost, the step from sample to sample was not.

**The cause.** `GLOBAL_GLOBALMV` blocks with at least one warped reference are predicted
by `inter_scale_warp_compound.hlsl` (`_hbd` for 10-bit). For a reference without a warp
that shader takes the path of the scaled-reference shaders, also when nothing is scaled:

```hlsl
int mvx = scale_value((mbx << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
...
mvx += (dx + wi) * scale.y;
```

`scale_value()` (`libav1/dx/shaders/inter_common.h`) multiplies a position by a scale, a
product that needs more than 32 bits. HLSL 5.0 has no 64-bit integers, so upstream
computed it in double precision (FXC emits `itod`, `dmul`, `dadd`, `dtoi` and marks the
shader `enableDoublePrecisionFloatOps | enable11_1DoubleExtensions`). The console creates
the pipeline state for such a shader without an error, but the result of `scale_value()`
is 0 there: exactly what the samples show. No other shader path that runs on these
streams uses double precision; all twenty `inter_scale_*` shaders do, and nineteen of them
only run for scaled references, which the test streams do not have. That this is what the
console does is concluded from its output, not measured on it: the md5 run below is the
proof.

**The fix.** `scale_value()` splits the scale into its upper bits and its lowest eight, so
that both partial products fit 32 bits, and rounds the sum the way libaom's `scaled_x()`
does (to 1/1024 sample, away from zero). The results are the same as with working double
precision, for every position and scale a frame can have, so a GPU that executed the old
code writes the same frames with the new one. Nothing else changes: the shaders of frames
without global motion and without scaled references are byte for byte the ones of the
build before.

The decoder also logs once per launch what the device says about double precision
(`GPU double precision shader operations: reported` or `not reported`), for the record;
no shader depends on it any more.

### Host-side check of the warp shaders

`testing/warp_check` (not part of the player build) compiles the shaders that predict
warped blocks as C++ (attributes, semantics and register bindings stripped with `sed`,
`testing/warp_check/hlsl_shim.h` for the rest): `inter_warp` for a block with one
reference and a local or a global warp, `inter_scale_warp_compound` for `GLOBAL_GLOBALMV`
blocks, their `_hbd` builds, and `inter_ext_borders`, which extends the border of every
reference frame. It runs them on generated frames laid out like the decoder's frame pool
(16 samples of border of which the decoder extends 12, the padding to 128, garbage
wherever the decoder defines nothing), with block records as `gen_pred_blocks.hlsl` writes
them and constants as `prediction.cpp` sets them.

The reference is libaom's C code of this tree, compiled as it is: `av1_make_inter_predictor`
with `av1_allow_warp` and `av1_warp_plane` (`av1_warp_affine_c`,
`av1_highbd_warp_affine_c`), the convolve functions, the difference weighted mask and the
mask blend, called for each block the way libaom's decoder calls them
(`testing/warp_check/ref_glue.c`; the two functions of libaom's block path that the fork
removed are repeated there). The whole pool must be byte identical afterwards: the frame,
the reference frames, the padding around them.

What the generated frames cover:

- 8-bit and 10-bit; frames from 8x8 to 1920x1080, with sizes that are multiples of 64,
  multiples of 8 only, neither, and odd;
- every block size that can be warped (8x8 to 128x128, the 1:2, 2:1, 1:4 and 4:1 shapes
  included), at the first and the last position of a row and of a column and in between,
  so also blocks that reach beyond the frame;
- global motion models of every type: rotzoom and affine as warps, translation and
  identity as the reference of a `GLOBAL_GLOBALMV` block that is not warped; local warp
  models from `find_projection()` and random ones, with models that send the block far
  outside the reference;
- `GLOBAL_GLOBALMV` with both references warped or one of them, with every compound
  type (average, distance weights, wedge, difference weights with the mask handed to
  chroma), every interpolation filter, vectors that point beyond every edge;
- with and without residual;
- frames whose references do not all have the frame's size (reference scaling, spatial
  layers): reference 0 is a frame of half, twice, or 8/16 to 16/8 of the frame's size, and the
  warped blocks have references of the frame's size, as they always do. The frames of the
  pool have layouts of their own, in buffers of the largest frame's size, as in the decoder;
- `scale_value()` on its own against libaom's `scaled_x()`, 400000 positions and scales;
- one frame with the block of the console failure: 1920x1080 10-bit, the 32x16 wedge
  block at x 1856, y 624, LAST plain, ALTREF with the model of that frame;
- one frame, 8-bit and 10-bit, with the block of the spatial layer vectors: a 1280x720 frame
  whose LAST is a 640x360 frame and a 64x64 block with a local warp from GOLDEN, 1280x720,
  at x 960, y 528.

Every frame runs twice: with double precision in the shaders working, and with every
double precision operation yielding 0, which stands for the console. The check also
counts the double precision operations the shaders execute, and fails if there is one.

```sh
sh testing/warp_check/run.sh             # the check
sh testing/warp_check/run.sh --mutate    # and fourteen mutated shaders, which must all fail
```

Further arguments go to the check: `--cases N` (frames per common size, default 24), `--seed S`,
`-v` (show every mismatch). It ends with

```text
warp_check: scale_value() against libaom's scaled_x(), 400000 values: identical with double precision, identical without
warp_check: console case (1920x1080 10-bit, 32x16 GLOBAL_GLOBALMV wedge block at x 1856 y 624, LAST plain and ALTREF rotzoom): identical
warp_check: spatial layer case (1280x720 8-bit, LAST a 640x360 frame, 64x64 block with a local warp from GOLDEN, 1280x720, at x 960 y 528): identical
warp_check: spatial layer case (1280x720 10-bit, LAST a 640x360 frame, 64x64 block with a local warp from GOLDEN, 1280x720, at x 960 y 528): identical
warp_check: 627 frames, 8-bit and 10-bit, 4399 blocks: 1449 GLOBALMV with a global warp, 1472 with a local warp, 1478 GLOBAL_GLOBALMV (866 of them with one reference not warped)
warp_check: 246 of the frames, with 1831 of the blocks, have a reference 0 of another size than the frame
warp_check: frames that differ from libaom: 0 with double precision, 0 without
warp_check: double precision operations executed by the shaders: 0
warp_check: the warp shaders and libaom write identical frames
```

and exit code 0. On the shaders before the fix it ended with

```text
MISMATCH console case, 1920x1080 10-bit, without double precision
  769 bytes differ; first in the frame, plane 0, x 1856 y 624: shader 80, libaom 691
  in the 32x16 block at x 1856 y 624, GLOBAL_GLOBALMV, ref 1 (identity, mv 0,0) + ref 7 (rotzoom warp), wedge, with residual
...
warp_check: frames that differ from libaom: 0 with double precision, 247 without
warp_check: double precision operations executed by the shaders: 9860032 (the console does not execute them)
warp_check: FAILED
```

so with working double precision the shaders were right in every case, edges included,
and without it every `GLOBAL_GLOBALMV` block with a reference that is not warped was
wrong. The mutants of `--mutate` each change one detail: the right, the left and the
bottom edge clamp of the warp, the right edge clamp of the compound warp and of its
reference that is not warped, a shear term, the centre of the 8x8 warp unit, the
offset and the rounding of `scale_value()`, two group syncs, and the size a block is
clamped to (see below). The workflow runs the check with `--mutate` in the job
`warp-check`.

**The size of the block's own reference.** `inter_warp.hlsl` and `inter_warp_hbd.hlsl`
clamped the coordinates of a warped block with `cb_dims`, which
`av1_prediction_run_all()` fills with the size of reference 0, whatever reference the
block has. In a frame whose references all have its size that is the same number. In the
upper layer of a stream with two spatial layers it is not: LAST is the frame of the lower
layer of the temporal unit, half the size, and a block with a local warp from an earlier
frame of the upper layer was clamped to the lower layer's size. That made every 1280x720
frame after the first of AOM's `av1-1-b8-22-svc-L2T*` vectors wrong (the first has the
lower layer as its only reference, and a block with a scaled reference cannot be coded
with a local warp). The shaders now clamp with `cb_refplanes[refplane]`, the block's own
reference, as the shaders for frames with scaled references always did. Three mutants are
the shaders as they were (8-bit, 10-bit, and the same mistake put into the compound warp),
and the check rejects them:

```text
warp_check: mutant rejected as expected: size of reference 0, not of the reference of the block, as the clamp of the warp (8-bit; the code before the fix)
```

`inter_warp_compound(_hbd).hlsl` had the same clamp and got the same change; no pipeline
state uses those two (`GLOBAL_GLOBALMV` blocks go through `inter_scale_warp_compound`).

**No warp from a scaled reference.** libaom's `av1_allow_warp()` refuses the warp of a
scaled reference before it looks at the model, and so does the specification (`useWarp` in
section 7.11.3.1 needs `is_scaled(refFrame)` to be 0): a `GLOBALMV` or `GLOBAL_GLOBALMV`
block whose reference is scaled is predicted from its motion vector, also when the
reference has a rotzoom or affine model. The host and `gen_pred_blocks.hlsl` asked the
model only. `av1_setup_frame()` now decides per reference (`td->gm_warp`), and the three
places that asked the model read that: `av1_mi_push_block()`, the constants of
`gen_pred_blocks.hlsl`, and the constants of the compound warp shader. A local warp needs
no test: the motion mode of a block with a scaled reference cannot be a local warp
(`motion_mode_allowed()`), so the bitstream has none. The check follows the rule (a
reference frame of another size has no warp model there); the rule itself is host code,
which the check does not run.

**Looked at and not a defect**: `inter_scale_compound_diff_luma(_hbd).hlsl` hands the mask of
a difference weighted block to chroma without a case for blocks 4 samples wide, and so does
`inter_compound_diff_luma(_hbd).hlsl`. A block with two references is at least 8 luma
samples wide and high (`is_comp_ref_allowed()`), so the thread that stores a word of the
mask always finds its second half in a thread of the same block.

What the check does not run: `gen_pred_blocks.hlsl` and `av1_mi_push_block()`, which
decide which blocks are warped (the records are written by the check, following them);
the chroma of warped blocks smaller than 16x16, which the translation shaders predict;
OBMC and inter-intra on top of a warped block; a `GLOBAL_GLOBALMV` block whose reference
that is not warped is scaled (the shader scales it; the check has no reference code for
that, and the conformance run's stream with scaled references has no global motion).

### Console run

**Not run on the console yet.** To check, against
[dav1d references](#md5-references-from-dav1d), one md5 task per launch:

```text
-i /streams/<1080p clip with global motion>.ivf -t 6 -frate -1 -limit 1000 -queue_size 4 -progress -nopresent -md5_frame_check
```

- `MD5 is OK` over at least 1000 frames of the 1080p stream above (frames 525, 601, 613,
  697 and 699 have global motion; errors used to spread from about frame 603 on).
- Unchanged `MD5 is OK` on the four 4K clips, none of which has global motion.
- The line `GPU double precision shader operations: ...` in the log, for the record.
- If a stream with scaled references is at hand: it runs the other nineteen shaders that
  used double precision, which no clip has exercised here.

## Super-resolution

A frame with `use_superres` is coded at a smaller width and upscaled horizontally after CDEF
(the upscaling process of the specification, in the decoding process chapter; libaom's
`av1_upscale_normative_rows()`). Upstream decoded such frames wrong on every GPU: the
SVT-AV1 stream of the conformance run (`superres-mode=2`, all denominators) had every frame
wrong on WARP and on the console, the key frame with about 85% of its samples off by up to
several hundred.

**Cause.** Which buffers the filters of a frame use depends on which filters it has. With
CDEF and superres and without loop restoration, CDEF writes into the back buffer the filters
share (`back_buffer1`) and the upscale reads from there. `av1_superres()` takes the width it
scales from out of the visible width of its source buffer, and `back_buffer1` got the
frame's layout at the frame's setup but not its visible size: that was whatever an earlier
frame had left there, 0 for the first frame. With a source width of 0 the step of the
upscale is 0, and every row of the upscaled frame is its first sample. With loop
restoration, or without CDEF, the source is a buffer whose description is complete, and
those frames were right.

**Fix.** `av1_setup_frame()` gives `back_buffer1` the frame's whole description (visible
size and bit depth next to the layout), as it does for the frame's own back buffer. No
shader changes; frames without superres never read these fields.

**Proof.** The [stage dumps](#stage-dumps--dumpstages) of the key frame on WARP, before the
fix: the dump behind CDEF, upscaled on a host as the specification says, is dav1d's frame,
sample for sample; the dump behind the upscale was a frame of constant rows (84.7% of the
luma samples differ from dav1d). After the fix the stream is md5-identical to dav1d on WARP,
64 of 64 frames, and is off `testing/conformance/expected.txt`. The stream has not been run
on the console since; its frames after the key frame also predict from references of
another width, which is the console-only bug that is still open.

**Host-side check of the shader.** `testing/superres_check` (not part of the player build)
compiles `upscaling.hlsl` as C++ (the converter and the HLSL types of `testing/sync_check`)
and runs it, with the constants `av1_superres()` gives it, over planes laid out like the
decoder's frame buffers. The reference is `av1_upscale_normative_rows()` of libaom's C code
in this tree, tile column by tile column as libaom does it. Every visible sample must be
equal, and the shader may write nothing outside the rows of the destination plane.

```sh
sh testing/superres_check/run.sh             # the check
sh testing/superres_check/run.sh --mutate    # and six mutated shaders, which must all fail
```

```text
superres_check: 792 cases, 8195300 samples compared with libaom, 0 failed
superres_check: mutant rejected as expected: the share of the rounding error in the initial subpixel position
superres_check: mutant rejected as expected: right edge clamp: the visible width instead of the last mode info column
superres_check: mutant rejected as expected: rounding of the step
superres_check: mutant rejected as expected: one filter tap
superres_check: mutant rejected as expected: left edge clamp
superres_check: mutant rejected as expected: first source word of the four output samples
```

The cases: every denominator (9 to 16) at twelve widths from 17 to 1920, among them widths
that are not a multiple of 8 or of 4 at either size, 8-bit and 10-bit, and 600 random cases
(width 16 to 4100, one to eight tile columns of random widths, 64x64 and 128x128
superblocks, noise, extremes, ramps and spikes as content). The source rows are filled up to
the last column of the last mode info unit, which is where the specification clamps, not at
the visible width. The check covers the shader, which was right; the bug was in the host
code that the conformance run covers. It runs as the `superres-check` job of the workflow.

## Layers and new sequences

**One frame per temporal unit.** A stream with spatial layers has one shown frame per layer in
a temporal unit. The decoder used to output all of them. The output process of the
specification (section 7.18.1) leaves the choice to the application and says what one that
displays the video does: one frame per temporal unit, the highest spatial layer that is in
the operating point and present in the temporal unit. libaom does that by default
(`output_all_layers` 0), and so do aomdec and dav1d; AOM's md5 files for the vectors with
two layers have that frame only. It is the library's default now:

- A frame's output (the copy into an output image, with film grain) is part of the frame's
  own command list, recorded before the next frame of the temporal unit is read. So the
  decision is taken from the OBU headers behind the frame (`av1_frame_is_output()` in
  `libav1/av1/decoder/obu.c`): a shown frame is output unless an OBU of a frame of a higher
  spatial layer that is in the operating point follows before the next temporal delimiter or
  the end of the data of the `aom_codec_decode` call. A frame that is not output takes no
  output image, like a frame that is not shown.
- A stream without layers has no such OBU: every shown frame is output, as before.
- `aom_codec_control(ctx, AV1D_SET_OUTPUT_ALL_LAYERS, 1)` gives every shown frame (the
  behaviour before); `AV1D_SET_OPERATING_POINT` selects the operating point of the sequence
  header by its index (default 0, the first; an index the stream does not have selects 0, as
  in libaom; it takes effect at the next sequence header). Both controls were "incapable"
  before. In the player and in `warp_decoder`: `-alllayers`, `-oppoint <n>`.
- An application has to pass one temporal unit per `aom_codec_decode` call, or data that
  keeps its temporal delimiters: with two temporal units in one call and no delimiter between
  them the frames of the first would count as lower layers of the second.

**A sequence that does not fit the decoder.** A decoder is created for a frame size and a bit
depth, which the player takes from the sequence header at the first key frame. A later
sequence can announce larger frames or more bits (`av1-1-b8-03-sizeup.mkv`: 352x288, then
1280x720). Its first frame used to fail with `Failed to allocate frame buffer`. Now
`aom_codec_decode` returns **`AOM_CODEC_NEEDS_RECONFIGURE`** (`libav1/aom/aom_codec.h`) when
it finds a sequence header whose largest frame, rounded up to 128, or whose bit depth is more
than the decoder was created for. It has decoded nothing of the call's data and the decoder
is as it was; `aom_codec_error_detail` has both sizes. What an application does then, and
what the player and `warp_decoder` do:

1. `aom_codec_decode(ctx, NULL, 0, NULL)`: the frames of the sequence before are handed over.
2. `aom_codec_destroy`.
3. `aom_codec_peek_stream_info` on the same data, and a decoder for what it says, created as
   for the start of a stream (the player never makes it smaller than the one before).
4. `aom_codec_decode` with the same data.

A sequence starts with a key frame, so the new decoder needs nothing of the old one. The
library does not replace its buffers itself because nearly everything it would need comes
from the application at creation: the host memory (which grows with the size, so the block
the application gave is too small), the device, the queue, the pipeline cache and the budget
callback; and the application has output images of its own to size and a memory limit under
which the old decoder has to go before the new one fits. With the code an application keeps
one path for creating a decoder and takes it a second time.

In the player the output queue and the output pool of the task stay across the two decoders
(the pool makes images of the new size as the frames arrive), so the md5 check and the
presentation go on without a break. The log has

```text
RECONFIGURE: a new sequence of 1280x720 8-bit frames (Sequence of 1280x720 8-bit frames, decoder created for 384x384 8-bit); the decoder is replaced by one of 1280x768 8-bit
```

followed by the `MEMORY` lines of the destruction and of the new creation. A sequence that
needs more than the player creates (`-nofitsize`, or more than 4096x2176) is a decode error
that says so. Not run on the console yet; `warp_decoder` decodes the vector right on WARP
([warp-decoder.md](warp-decoder.md#results)).

## Experiments

For the two bugs that show on the console only (8-bit streams, references of another size;
[gpu-av1-decoder.md](gpu-av1-decoder.md#bit-fields-of-width-0-2026-11-12-candidate-fix-not-yet-run-on-the-console)).
Each is off by default, and on a device that follows D3D each leaves the output as it is: the
conformance run decodes the whole set on WARP once more with each of them and gets the same
results. The task's log has an `Experiments:` line that says what is in effect.

None of them changed the console's output; the cause of the 8-bit mismatch was a different one (a
paired read before an overlapping in-place write in the vertical loop filter), and the scaled-reference
"bug" was a reference-file error. The flags stay in the player, but the workflow builds the uavsync
package and decodes the set with each flag only when it is started by hand with the `experiments`
input (`gh workflow run build.yml --ref <branch> -f experiments=true`); a push runs the conformance
set once, which takes the job from about 15 to about 5 minutes.

| | What it changes | Why |
| --- | --- | --- |
| `-lfhsplit 1` | The horizontal loop filter of a plane is no longer one dispatch: first the even rows of blocks (a block is 4 columns by 64 rows), a UAV barrier, then the odd rows. | The first edge of a block reads and writes the last rows of the block above it, which another thread group of the same dispatch filters; the vertical filter is two dispatches for the same reason. By the rules of the loop filter the rows two neighbouring edges write never overlap, and what one reads of the other's it does not use, so the single dispatch should be right; the flag tests that on the device. |
| `-lfhsplit 2` | Every row of blocks is a dispatch of its own, top to bottom, with a UAV barrier behind each: the order WARP comes closest to. Slower (34 dispatches and barriers per plane at 2160 rows). | The same, with nothing left in parallel between rows of blocks. |
| `-intraedgefix` | `intra_main.hlsl` (8-bit) stops the taps of the above edge filter at the last entry the filter writes, as `intra_main_hbd.hlsl` and the left edge do, instead of at the last entry that was loaded. | The two shaders differ there. The entries between the two bounds are loaded and all hold the last available sample, so the sums are the same; `testing/intra_check` passes with either bound. The flag tests that on the device. |
| the **uavsync package** (artifact `rav1g-testplayer-uavsync-x64-<run id>`) | Every group sync of every shader is compiled as `AllMemoryBarrierWithGroupSync()` instead of `GroupMemoryBarrierWithGroupSync()` (`/p:Rav1gUavSync=1`, an FXC define; no source differs). | A group sync of the group memory orders the group memory. Several shaders also store frame buffer words in one step and have another thread of the group load them in a later step (the horizontal loop filter from edge to edge, filter intra before its residuals are added); D3D asks for a sync of all memory for that. |

The uavsync package has the identity of the normal one and version `0.2.<run>.1` where the
normal one has `0.2.<run>.0`: it installs over the normal package of the same run and keeps
LocalState (streams, references), and the normal package of any later run installs over it.
Going back to the normal package of the same run needs an uninstall, which removes LocalState;
so run the normal package first. Its log says `Experiments: shaders uavsync`. The flags work in
it as well.

## Stage dumps (`-dumpstages`)

For finding the stage of the frame pipeline where two devices stop agreeing (the console and
WARP, say): `-dumpstages <frames>` copies, for the listed frames, the buffer every GPU stage
of the frame's command list has just written, and writes the copies to files. The same flag
exists in [warp_decoder](warp-decoder.md#stage-dumps), which writes the same files, and
`testing/stage_dump/stage_dump.py` compares the two sets.

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 2 -queue_size 4 -nopresent -md5_frame_check -dumpstages 0,1
```

- `<frames>` is a list like `0,3,10-12` (at most 256 frames) of decoder frame numbers:
  frames **in decoding order** from 0, frames that are decoded but not shown included. This
  is not the output order of the md5 lines when the stream has hidden frames; a
  `show_existing_frame` is not a frame of its own (its output dump is appended to the file of
  the frame it shows). For the first frames of a stream the two orders agree up to the first
  hidden frame; `stage_dump.py list` prints each dumped frame's type and `show` flag.
- The files go to `LocalState\dumps\<stream file name>_f<n>.stages`, one per frame;
  `-dumpdir /<dir>` names another directory below LocalState. The directory is created.
- `-limit <n>` counts temporal units and ends the task after n of them: give the smallest
  that covers the listed frames (a temporal unit has one shown frame and the hidden frames
  in front of it).
- **Size.** A dump is a whole frame buffer: an 8-bit 3840x2160 frame has about ten dumps of
  13.5 MB plus the residuals, 27 MB (about 150 MB a frame); 10-bit 1920x1080 about ten of
  7.2 MB plus 9 MB. The copies of a frame are readback buffers that live until the frame's
  GPU work is done, on top of the decoder: list few frames of a 4K stream. A copy that
  cannot be allocated is skipped with a `DUMPSTAGES ... no readback buffer` line.
- **Log.** `DUMPSTAGES frames ... to ...` at the start of the task, one `DUMPSTAGES frame
  <n> stage <i> <name>: ...` line per dump after the last frame, and a total.
- **Cleanup.** Fetch the `.stages` files with Device Portal and delete them, or the
  directory, afterwards: nothing else removes them, and the next run of the same stream and
  frame overwrites its file.
- The decoded output is the same with and without the flag: a dump is a copy between two
  stages that are a UAV barrier apart already. It does add a state transition of the buffer
  at each stage, so if a frame is wrong without the flag and right with it, that itself
  says the stages overlap on that device; the md5 check in the same task shows it.

The stages, in the order of a frame's command list (a frame has those that run for it):

| Stage | What is dumped |
| --- | --- |
| `idct` | the residuals of the frame (signed 16-bit samples, 128-aligned planes) |
| `inter` | the frame buffer after inter prediction, all passes |
| `recon` | after the residuals of the inter blocks were added |
| `intra` | after intra prediction: the frame before the loop filter |
| `lf_vert_even`, `lf_vert` | after the first and the second pass of the vertical loop filter |
| `lf_horz` | after the horizontal loop filter |
| `cdef` | the buffer CDEF wrote |
| `superres_lr`, `superres` | the upscaled frame without CDEF (which loop restoration reads), the upscaled frame |
| `loop_rest` | after loop restoration |
| `borders` | the finished frame with its borders: what later frames predict from |
| `output` | the output image, after film grain, as it is handed to the application |

A dump holds the buffer as stored (the whole frame buffer with its borders and padding) and
the layout of its planes; the comparison looks at the visible samples.

Between the stages a file also has the **data buffers** the stages read besides the frame:
lists and tables, no samples. Each is copied where the first stage that reads it starts, so
it holds what that stage reads on the device, and comes with a text that says how it is laid
out (the format is in `testing/common/stage_dump.h`):

| Buffer | What it is | Read by |
| --- | --- | --- |
| `mi_grid`, `mi_pool` | the frame's mode info: one record address per 4x4 unit, and the records of the frame's tiles (block size, references, modes, transform sizes, skip, segment, ...) | `gen_pred_blocks`, `gen_lf_blocks_vert/hor` |
| `pred_blocks`, `pred_blocks_warp` | the prediction block records `gen_pred_blocks` wrote | the prediction and reconstruction stages |
| `lf_params` | the constants of the loop filter: the frame's levels and deltas, the level per plane, segment, reference and mode, and the three limits of every level | `gen_lf_blocks_vert/hor`, `loopfilter_vert/hor` |
| `lf_blocks` | the six edge lists of the loop filter (vertical and horizontal edges of Y, U, V): per block sixteen edges, each a level, a filter length and the step to the next edge | `loopfilter_vert/hor` |
| `cdef_info` | CDEF's strength index per 64x64 block and skip flag per 8x8 block | `cdef_filter` |
| `lr_info` | loop restoration: type and coefficients per unit | `loop_restoration` |

A frame has the buffers of the stages that run for it (no `lf_blocks` without loop filter).
The buffers the shaders write (`pred_blocks*`, `lf_blocks`) are copied like a frame buffer,
with a state transition in front of the copy and one behind it; the others are memory the
GPU only reads, and are copied without one. They add little to a dump: the edge lists of a
3840x2160 frame are 3 MB, the mode info as many records of 224 bytes as the frame has
blocks.

A stream that starts a new sequence the decoder cannot take (`RECONFIGURE` in the log) gets
a new decoder, whose frame numbers start at 0 again: the frame list applies to every
sequence, and the files of the frames behind the n-th such start are called
`<stream file name>.seq<n>_f<n>.stages`. `-dumpstages 0` on a stream with one size change
writes `<stream>_f0.stages` and `<stream>.seq1_f0.stages`, the first frame of each size.

Compare on the host (Python 3, no packages):

```sh
python3 testing/stage_dump/stage_dump.py list console-dumps/
python3 testing/stage_dump/stage_dump.py compare console-dumps/ warp-dumps/
```

`compare` pairs the files by frame number, prints per stage whether the planes are equal, and
for the first stage that differs the pattern of the differences in each plane: how many
samples, how large, rows and columns, the position in the 8x8 and 64x64 grid, which sample
of a 32-bit word, runs, the first samples. `--all` prints the pattern of every stage. The
other side can be a digest (hashes per row and per 8x8 block; see
[warp_decoder](warp-decoder.md#stage-dumps)): then it names the rows and blocks, not the
sizes. `compare-yuv` compares one stage with a frame of a planar file from a reference
decoder, `extract` writes one stage as a planar file.

The data buffers are compared as well, each where it stands in the frame, and a buffer that
differs is printed decoded, both sides (`--samples N` lists more than twelve entries):

- `lf_blocks`: per list the blocks that differ, where they are in the plane, and every edge
  of the block on both sides. An edge of a vertical list is `x=<column> level <n> <taps>-tap
  step <n>`, of a horizontal list `y=<row> ...`; `not filtered` is level 0, the step is the
  distance to the next edge in 4-sample units. Entries of a record that differ but that the
  filter does not reach (behind the last edge of the block) are named as such. A list of a
  plane the frame does not filter is not written and not compared.
- `mi_grid`: the 4x4 units whose mode info record differs, counted per field, with the first
  units and the values on both sides. The fields the edge list shaders read are marked `*`,
  and the line above says how many units differ in one of those. The record addresses, which
  differ from run to run, are not compared; a field no stage reads for a block may hold what
  an earlier block left in the record, so a difference in an unmarked field is not by itself
  a wrong frame.
- `lf_params`: the constants that differ, by name.
- `cdef_info`, `lr_info`: the 64x64 and 8x8 blocks, or the units, that differ. Skip entries
  outside the part that is written for the frame are counted in a `NOTE` and are not a
  difference.
- `pred_blocks`, `pred_blocks_warp`: the records that differ, as 32-bit words.

A buffer that only one side has (dumps of an older package, format 1) is named and left out
of the result. Two more commands print one side on its own:

```sh
python3 testing/stage_dump/stage_dump.py edges dumps/<clip>_f1.stages --dir vert --plane 0 --x 64 --y 41
python3 testing/stage_dump/stage_dump.py mi dumps/<clip>_f1.stages 16 10
```

`edges` prints the edge list of the block that holds a sample of a plane (and the frame's
levels), `mi` the mode info record of a 4x4 unit (column, row), field by field.

What a comparison of a wrong frame says about the loop filter: if the first stage that
differs is `lf_vert_even`, `lf_vert` or `lf_horz` and `lf_blocks`, `lf_params` and the frame
in front of the stage are equal, the filter shader computed differently from the same
input; if `lf_blocks` differs and `mi_grid` (its `*` fields) and `lf_params` are equal, the
list shader did; if those differ too, the difference is in what the decode threads wrote.
If the frame is right with `-dumpstages` and wrong without, the copy itself changed the
outcome: then the stage did not see what the stage before it wrote.

### A model of the 8-bit loop filter on a dump

`testing/stage_dump/lf_model.py` filters a dump's own frame in front of the loop filter (`intra`) with
the arithmetic of `loopfilter_vert.hlsl`, compares the result with the dump's `lf_vert`, and does the
same from `lf_vert` to `lf_horz` with `loopfilter_hor.hlsl`, per plane: the number of samples that
differ and how many edges took which branch (4, 6, 8 or 14 taps). It reads the constants and the edge
lists from the dump (format 2), or with `--lists` from another dump or digest of the same frame, for a
dump of an older package; with a digest it also compares its own output with the digest's row hashes.

```sh
python3 testing/stage_dump/lf_model.py console/<clip>_f0.stages --lists warp/<clip>_f0.digest
python3 testing/stage_dump/lf_model.py console/<clip>_f0.stages --lists warp/<clip>_f0.digest --variant console-0.2.25
```

A stage the model reproduces was computed as the shader is written. `--variant console-0.2.25` is the
filter as the console ran it with the shaders before this change (package 0.2.25.0), with one step of the 8-tap branch of
the vertical filter changed; that is how the cause of the 8-bit mismatch was found
([gpu-av1-decoder.md](gpu-av1-decoder.md#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)).
8-bit frames only; exit code 1 when a stage differs from the model.

## Shader survey

To pick the next shader to look at, the workflow writes `shader-survey/survey.md` (also
shown in the run summary) and `survey.csv` from the listings of all shaders, compiled with
their Release x64 flags: thread group, instruction slots, temp registers, indexable temps
(count and total size), instructions that touch them, immediate constant buffer entries and
reads, group shared bytes, barriers (all, and those that wait for the whole group), loops
and FXC compile time. `shader-survey/asm/` has all listings, and `shader-survey/build.json`
the cache key, the compile time and the sha256 of the bytecode header of every shader. The
counts are static and are not a timing.

A listing and the bytecode header the package is built from come from the same FXC call
(`/Fc` and `/Fh`). The build step then checks two things and writes the result under
`survey.md` and the two `stats.txt` files: MSBuild ran FXC on no shader of the library, and
the bytecode of the headers is in the program inside the package, byte for byte
(`shaders.py verify`; of the 92 arrays the linker drops the ones no pipeline state uses, and
the line names them).

FXC runs on several shaders at a time (one per logical processor of the runner) and the
listings are parsed afterwards in name order, so the files do not depend on which compile
finishes first. A shader's FXC compile time is that of its own FXC process and includes
contention with the compiles running beside it; for a shader that came from the cache it is
the time of the run that compiled it. Compare compile times within one run only, and treat
small differences as noise. A shader that fails to compile still stops the run, with its FXC
output in the step log, and so does a bit field instruction with a run-time width
([gpu-av1-decoder.md](gpu-av1-decoder.md#bit-fields-of-width-0-2026-11-12-candidate-fix-not-yet-run-on-the-console)),
and a listing with a paired read before an overlapping in-place write ([Listings scan](#listings-scan)).

### Listings scan

After the survey the workflow reads the listings of both variants with
`testing/dxbc_scan/scan.py` and stops when it finds the pair of instructions that the console's
driver compiled wrongly in the 8-bit vertical loop filter
([gpu-av1-decoder.md](gpu-av1-decoder.md#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)):

- an *overlapping in-place write*: an instruction that writes components of a temp register and
  reads, of the same register, for a written component, another component that it also writes
  (`iadd r3.yw, r3.wwww, -r4.xxxy`: `r3.y` from `r3.w`). FXC emits about 1650 of them in the 92
  shaders; they are counted;
- with a *paired read before it*: the write is an arithmetic or logic instruction, and before it,
  with no flow control instruction between the two, an instruction that does not write the
  register reads two or more of the components the write replaces into one component of its
  result (`iadd r2.x, r3.w, -r3.y`). This stops the run, unless the write is in
  `testing/dxbc_scan/allow.txt`.

```sh
python3 testing/dxbc_scan/scan.py --self-test
python3 testing/dxbc_scan/scan.py --listings shader-survey/asm --out listing-scan.txt
```

The step log names every pair with its shader, the line of the listing and both instructions; the
artifact's `shader-survey/listing-scan.txt` also has every overlapping in-place write. `--self-test`
checks the scan itself: it must find the pair in a copy of the old listing and must not find it in
five sequences that differ from it in one property each. An opcode the scan does not know stops it,
so no instruction goes unread.

When the step fails after a shader change: find the HLSL of the two instructions and change its form,
not its arithmetic, until FXC no longer emits the pair. What worked so far: every output from its own
sum instead of a running sum (`loopfilter_vert.hlsl`), another order of the terms of one sum
(`loopfilter_hor.hlsl`), addresses of rows written as explicit vectors of another width
(`inter_warp.hlsl`, `inter_compound_masked_hbd.hlsl`), a `[branch]` in front of an `if` that FXC had
turned into a select (`inter_2x2chroma_hbd.hlsl`). The order of statements does not matter to FXC. A
line in `allow.txt` needs a proof that the console runs that pair correctly, written next to it; a
line that matches no pair any more is an error.

### Shader cache of the workflow

FXC takes about 40 s for the 92 shaders on the four processors of a runner, and MSBuild ran
it on one shader at a time. A workflow run compiled every shader five times: the survey, the
package, the uavsync package, `warp_decoder` and its uavsync build. Now a shader is compiled
once per variant (normal and uavsync), and not at all when an earlier run has compiled the
same thing (`.github/actions/shaders`, `testing/shader_cache/shaders.py`).

Measured on the player job: before the cache 3:42 for the job (run 43141191340); with a cold cache
the shader step took 42 s for both variants (run 43150569680); with a warm cache 5 s, the package
build 70 s and the whole job 2:18 (run 43153397292). How it works:

- **Key of a shader**: sha256 over the FXC binaries (`fxc.exe` and the `d3dcompiler` DLL
  beside it), the arguments that decide the output (`/T cs_5_0 /E main /Vn <name>`, `/Od`
  for the two shaders the project builds without optimization, the define of the uavsync
  variant), the source file and every file it includes, directly or not.
- **Cache entry**: the header (`/Fh`) and the listing (`/Fc`) of one FXC call, and what FXC
  printed (warnings are shown again when the entry is used).
- **Actions cache**: the directory of entries is saved under the digest of all keys
  (`rav1g-shaders-v1-<digest>`). A run with another digest starts from the newest saved
  directory, compiles the shaders whose keys are not in it, on four at a time, drops the
  entries it did not use and saves the directory under its own digest. So a change to one
  shader compiles that shader, a change to `inter_common.h` the 42 that include it, and a
  change to none compiles nothing.
- **The builds**: MSBuild gets `/p:Rav1gPrebuiltShaders=<directory>`. With it,
  `libav1.vcxproj` and `warp_decoder.vcxproj` put the directory first on the include path
  (`av1_compute.cpp` includes `dx/shaders/h/<name>.h`) and leave their `FxCompile` items
  out. Only the workflow sets the property: a build in Visual Studio or with plain
  `msbuild` compiles the shaders itself, into `libav1/dx/shaders/h`, as before.
- The player job and the conformance job each run the step; they have the same keys, so the
  packages and `warp_decoder` get the same bytecode.

To run it by hand (any FXC; `--jobs` defaults to the processor count):

```bat
python testing\shader_cache\shaders.py build --fxc "<path>\fxc.exe" --cache shader-cache ^
  --variant normal --root shader-build\normal --listings shader-build\normal\asm ^
  --report shader-build\normal\report.json
msbuild testing\warp_decoder\warp_decoder.vcxproj /p:Configuration=Release /p:Platform=x64 ^
  /p:Rav1gPrebuiltShaders=%CD%\shader-build\normal
```
