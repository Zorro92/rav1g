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
  the player flag `-cdefv2` that selects it; see [CDEF v2 run](#cdef-v2-run--cdefv2). The
  upstream shader `cdef_filter.hlsl` is not modified and stays the default. The new
  shader produces the same bytes as the upstream one, including where upstream differs
  from the AV1 specification. A host-side comparison of the two shaders lives in
  `testing/cdef_check`, which is not part of the player build.
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
  failed) and the player flag `-fitsize`, which sizes the decoder for the stream.

## Build

`.github/workflows/build.yml` runs on every push to `main` that touches more than docs, and
on demand. It builds `av1xbox.sln` (the player and its `uwp_common`/`libav1` dependencies)
as Release x64 with MSBuild, the `v145` toolset and the `10.0.26100.0` Windows SDK. The
minimum Windows version is overridden to `10.0.17763.0`. Upstream FXC/Shader Model 5.0
compute shader settings are unchanged, and without `-cdefv2`, `-intrav2` and `-intraopt` the
decoder runs exactly the upstream shaders. An appended compiler option `/wd4996` suppresses the
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

The run has two more jobs on Linux, `cdef-check` and `intra-check`. They run the
host-side comparisons of the CDEF shaders and of the intra paths; `intra-check` also runs
the self-test of the timing summary and uploads its log as `rav1g-intra-check-<run-id>`.
The run is green only if all three jobs pass.

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

The number is a running count. "Held by decoder" are buffers rented for frames in
flight and hidden frames not shown yet. A `BLOCKED` line with no `resumed` line at
the end of a log means the task hung right there; if "held by decoder" equals the
pool size, nothing outside the decoder could free a buffer.

After the last frame, the summary (first 10 decoded frames and first 10 output frames
are left out as warm-up when the task has more than 20):

```text
GPUTIME ==== summary: 300 frames decoded, 240 output; warm-up 10 skipped, 290 measured (...), GPU timestamps on 290 ====
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
GPUTIME per OUTPUT frame (1.25 decoded frames each): wall .. ms, gpu busy .. ms, cpu decode thread .. ms, ...
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
10-bit streams. CDEF v2 is a replacement to measure against it.

| Flag | Effect |
| --- | --- |
| `-cdefv2` | CDEF runs through `cdef_filter_v2.hlsl` instead of `cdef_filter.hlsl`. Decoded output is meant to be identical. |

Off by default: without the flag the decoder creates the same pipeline state and
issues the same dispatch as before. The task's log says which shader is in use:

```text
CDEF shader: cdef_filter (original)
CDEF shader: cdef_filter_v2 (-cdefv2)
```

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
shader is checked against it.

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -save_md5 /streams/<clip>.ivf.md5
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -cdefv2 -md5_frame_check
```

- `-save_md5` takes a path below LocalState, like `-i`. `-md5_frame_check` takes no
  value and always reads `<stream path>.md5`, so the saved file must have exactly the
  stream's name plus `.md5`, next to the stream.
- Use the same `-limit` in both tasks. The checker does not guard against a reference
  file with fewer frames than it decodes.
- **Save and check in separate launches.** The saver's file is not flushed when its
  task ends, so a check task in the same `args_av1.txt` reads only the first ~8 KB
  (about 210 frames). Past that point the checker either reports errors with an empty
  expected value (`Avaiting  In real ...`) or crashes without a log line. Run the save
  task, let the app exit, then launch again with only the check task.
- The result is in the log when the second task ends: `MD5 is OK`, or
  `MD5 checksum errors in N frames` with one `MD5 checksum error in <n> frame` line
  per bad frame. Later frames are predicted from earlier ones, so one wrong CDEF
  output shows up as errors from that frame on.
- Both flags read every frame back from the GPU. Do not combine them with `-gputime`
  for timing numbers.
- Do not add `-try10x3`: the saver and the checker hash that output format differently.

The md5 covers the visible picture after all later stages (loop restoration, film
grain). Run it on at least one 8-bit and one 10-bit clip.

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
| `-fitsize` | The decoder is created for the stream's largest frame size instead of 4096x2176, which makes every buffer smaller; see [Decoder size](#decoder-size--fitsize). |

All are off by default: the decoder then has two slots, clears on the decode thread and
is sized for 4096x2176, as before. They are independent of each other and of `-cdefv2`,
`-intrav2`, `-intraopt`, `-gputime`, `-nopresent` and the md5 flags.

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

- **Reference frame buffers**: 12 with two slots, one more per added slot (13, 14). A
  frame in flight holds its back buffer and its references until its GPU work is done,
  so a deeper pipeline with the same pool would only move the wait from `wait_slot` to
  the frame buffer pool.
- **Unchanged**: the GPU work item pool (8), the output image descriptors (16), and the
  player's output buffer pool (`-queue_size`). A frame that is shown holds one output
  buffer while it is in flight, so `-queue_size` still limits how many shown frames can
  be in the pipeline: with `-queue_size 4` and `-slots 4` the output pool, not the slot
  count, is the limit (`wait_output`).

Memory per slot. The player always creates the decoder for 4096x2176, 10-bit, so that
column is what an added slot costs in the test player; the other column is a decoder
created for 3840x2160, 10-bit. All of it is D3D12 buffer memory (on the console that is
the same memory as the host's); the host-heap part of a slot is about 0.5 MB.

| Per slot, MiB | 4096x2176 (player) | 3840x2160 |
| --- | --- | --- |
| mode info pool share | 59.5 | 55.8 |
| coefficient buffer share | 36.1 | 33.9 |
| palette buffer | 25.8 | 24.2 |
| transform block list | 12.8 | 12.0 |
| intra v2 schedule buffer | 10.6 | 10.0 |
| prediction block map and indexes | 14.1 | 13.2 |
| mode info grid, intra iteration grid | 8.7 | 8.2 |
| CDEF, loop restoration, film grain, command constants | 3.4 | 3.3 |
| **slot** | **171.0** | **160.4** |
| one more reference frame buffer (with its motion vectors and segment map) | 27.4 | 25.7 |
| **each slot above two** | **198** | **186** |

So `-slots 3` costs about 198 MiB more than the default and `-slots 4` about 397 MiB
more. If the allocation fails, the task ends with `Failed to initialize decoder`, and the
log says which allocation it was; see [Memory lines](#memory-lines-in-the-log-memory).

**On the console in app mode, `-slots 3` and `-slots 4` failed to allocate** with package
0.2.4.0: every such task ended with `Failed to initialize decoder: Memory allocation
error`, and the tasks after it in the same launch failed too, also with the default two
slots. That package did not say which allocation failed, and a failed creation did not
release the buffers it had already created, which is what broke the later tasks. Both are
changed after 0.2.4.0: the `MEMORY` lines below, and a failed creation that leaves nothing
behind. Whether three slots fit is still open; the lines are there to find out.

### Memory lines in the log (`MEMORY`)

Every decoder creation writes lines that start with `MEMORY ` (`grep MEMORY`). The decoder
writes them through `av1_set_log_callback` (`libav1/dx/av1_perf.h`), the player adds the
ones about the app and its output pool. The numbers below are what the allocation code
gives for the default 4096x2176, 10-bit and two slots; `N` stands for a value that depends
on the run.

Before the decoder is created (player):

```text
MEMORY decoder size: 4096x2176 10-bit (default), slots 2, host pool N.N MiB, output pool 4 images
MEMORY app before the decoder: usage N.N MiB of limit N.N MiB, N.N MiB left
```

`app` is `Windows::System::MemoryManager`: the memory of the whole app and the limit the
system gives it. On the console the D3D12 buffers are part of it.

After a creation that worked (decoder, then player):

```text
MEMORY decoder created: 4096x2176 10-bit, slots 2, frame buffers 12 + 1 back buffer of 27017216 bytes (25.8 MiB) each; 40 device buffers, 791353600 bytes (754.7 MiB); host pool N of N bytes (N.N MiB) in use
MEMORY decoder created by type: DeviceOnly 7 buffers 412.0 MiB; DeviceUpload 16 buffers 83.9 MiB; HostRW 11 buffers 185.8 MiB; ReadBack 0 buffers 0.0 MiB; DeviceOnlyConst 5 buffers 0.8 MiB; UploadUAV 1 buffers 72.2 MiB; textures 0
MEMORY decoder created per slot: 179266304 bytes (171.0 MiB) = 13 own buffers 75.3 MiB + mode_info_pool share 59.5 MiB + idct_coefs share 36.1 MiB; a slot above 2 also adds one frame buffer, 25.8 MiB
MEMORY decoder created largest: frame_buffer_pool 335.0 MiB DeviceOnly; mode_info_pool 119.0 MiB HostRW; idct_coefs 72.2 MiB UploadUAV; idct_residuals 26.0 MiB DeviceOnly; palette_buffer 25.8 MiB DeviceUpload (per slot); prediction_blocks 25.5 MiB DeviceOnly; idct_blocks 12.8 MiB DeviceOnly; idct_blocks_unordered 12.8 MiB DeviceUpload (per slot); intra_v2_buffer 10.6 MiB HostRW (per slot); prediction_blocks_warp 9.6 MiB DeviceOnly
MEMORY dxgi before the decoder's allocations: local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available; non-local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available
MEMORY dxgi after the decoder's allocations: local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available; non-local budget N.N MiB, usage N.N MiB, reserved N.N MiB of N.N MiB available
MEMORY app after the decoder: usage N.N MiB of limit N.N MiB, N.N MiB left
```

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
MEMORY FAILED buffer palette_buffer of slot 2: 27017216 bytes (25.8 MiB), type DeviceUpload, heap UPLOAD, CreateCommittedResource hr 0x8007000E; allocated before it: 970619904 bytes (925.7 MiB) in 52 buffers
MEMORY FAILED decoder creation at: decoder buffers (the MEMORY FAILED line above names the one)
MEMORY FAILED decoder, allocated up to the failure: 4096x2176 10-bit, slots 3, frame buffers 13 + 1 back buffer of 27017216 bytes (25.8 MiB) each; 52 device buffers, 970619904 bytes (925.7 MiB); host pool N of N bytes (N.N MiB) in use
MEMORY FAILED decoder, allocated up to the failure by type: ...
MEMORY FAILED decoder, allocated up to the failure per slot: ...
MEMORY FAILED decoder, allocated up to the failure largest: ...
MEMORY dxgi FAILED, before the creation: local budget N.N MiB, usage N.N MiB, ...
MEMORY dxgi FAILED, at the failure: local budget N.N MiB, usage N.N MiB, ...
MEMORY dxgi FAILED, after releasing the failed decoder: local budget N.N MiB, usage N.N MiB, ...
Failed to initialize decoder: Memory allocation error: MEMORY FAILED buffer palette_buffer of slot 2: 27017216 bytes (25.8 MiB), type DeviceUpload, heap UPLOAD, CreateCommittedResource hr 0x8007000E; allocated before it: 970619904 bytes (925.7 MiB) in 52 buffers
MEMORY app after the failed decoder creation: usage N.N MiB of limit N.N MiB, N.N MiB left
Unable to create decoder. Error 2
```

Other `MEMORY FAILED` lines: `host pool` (the pool the player allocated is too small for
the configuration), `compute pipeline state N` (a shader), `decoder creation at: <step>`
for the command allocators, fences, command list and the lookup table upload, and
`output image texture` / `output image readback buffer` from the player's output pool.

A creation that fails releases everything it had created before it returns, so the next
task starts as it would after a fresh launch: `usage` in the `dxgi ... after releasing`
line should be back at the value of `before the creation`. The player also releases the output
pool of the decoder before, ahead of creating the new one.

### Decoder size (`-fitsize`)

The player creates the decoder for 4096x2176, 10-bit, whatever the stream is. With
`-fitsize` it reads on to the stream's first key frame before it creates the decoder and
uses the largest frame size the sequence header announces (`max_frame_width`,
`max_frame_height`), rounded up to a multiple of 128 as the decoder rounds it: 3840x2176
for a 3840x2160 stream, which is the second column of the table above (about 47 MiB less
for two slots, 59 MiB less for three). Off by default. The decoded output is the same.

```text
MEMORY decoder size: 3840x2176 10-bit (-fitsize, the stream's largest frame is 3840x2160), slots 3, host pool N.N MiB, output pool 4 images
Decoder size: 3840x2176 (-fitsize)
```

Without the flag the second line reads `Decoder size: 4096x2176 (default)`.

- The bit depth stays 10: the peek at the sequence header does not read it, and 10-bit
  buffers hold 8-bit frames.
- A stream larger than 4096x2176 in either direction keeps the default size (and fails at
  its first frame, as without the flag).
- The player creates a new decoder when the size differs from the one it has, as for
  `-slots`.
- Every frame of a sequence is within the announced size (frame size override, superres
  and reference scaling included), so the buffers fit. A later sequence header in the same
  file may announce a larger size; the decoder now refuses a frame that is wider or higher
  than it was created for (decode error `Failed to allocate frame buffer`) instead of
  only comparing the byte size of a frame buffer. That check is on with and without the
  flag.
- Not verified on the console yet: the decoder has only ever run there at 4096x2176. Check
  a `-fitsize` run with `-md5_frame_check` before using it for timing.

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

### Timing: the four combinations

All four lines in `args_av1.txt`, same clip, all with `-cdefv2`:

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

### Same output: md5 of every frame

As for [CDEF](#same-output-md5-of-every-frame), **in separate launches**. First launch,
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

## Shader survey

To pick the next shader to look at, the workflow compiles every shader of the project
once more with its Release x64 flags and writes `shader-survey/survey.md` (also shown in
the run summary) and `survey.csv`: thread group, instruction slots, temp registers,
indexable temps (count and total size), instructions that touch them, immediate constant
buffer entries and reads, group shared bytes, barriers (all, and those that wait for the
whole group), loops and FXC compile time. `shader-survey/asm/` has all listings. The
last line of `survey.md` says whether every listing matches the bytecode compiled into
the package. The counts are static and are not a timing.

The survey runs FXC on several shaders at a time (one per logical processor of the
runner) and parses the listings afterwards in name order, so the files do not depend on
which compile finishes first. A shader's FXC compile time is that of its own FXC process
and includes contention with the compiles running beside it; `survey.md` and the
`stats.txt` files say how many ran at a time. Compare compile times within one run only,
and treat small differences as noise. A shader that fails to compile still stops the run,
with its FXC output in the step log.
