# rav1g-dec, warp_decoder and the conformance run

`rav1g-dec` (`tools/rav1g-dec`) is the decoder as a desktop console program. It decodes AV1 streams
with the `rav1g` crate on a D3D12 device, by default the **WARP software adapter**, reads every
output frame back and computes one md5 per output frame, the way the test player does. It needs no
GPU, no window and nothing of UWP, so it runs on a CI runner: every push decodes AOM's decoder test
vectors and a few other streams with it and compares the md5 of every frame with a reference (the
[conformance run](#conformance-run)).

It exists because decoder bugs were found only on the console, and only by luck. It does not
measure speed: WARP rasterises on the CPU.

**`warp_decoder`** is the same program of the C++ decoder: C++, an MSBuild project,
`testing/warp_decoder` in branch `cpp`. It came first, and `rav1g-dec` was written to take its flags,
print its lines, write its files and return its exit codes, so that every script of
`testing/conformance` and `testing/stage_dump` runs with either (`--tool`). When the C++ host left
`main` (2026-11-13, [rust-port.md](rust-port.md)) `warp_decoder` left with it. CI still builds it,
from branch `cpp` at a pinned commit, as the second decoder that `rav1g-dec` has to agree with: see
[Equality with the C++ decoder](#equality-with-the-c-decoder). Where this document says
`warp_decoder` in a result or a measurement, that run was made with it; the flags and lines are
those of both.

## Build

On Windows (stable Rust, the MSVC build tools, a Windows SDK, and LLVM for `bindgen`):

```bat
set RAV1G_SHADER_DIR=<directory whose dx\shaders\h has the shader headers>
cargo build --release
```

The program is `target\release\rav1g-dec.exe` (statically linked runtime, no other files needed).
The shader headers are those of the FXC build: the directory
`testing/shader_cache/shaders.py build --root <dir>` writes
([test-player.md](test-player.md#shader-cache-of-the-workflow)); the build stops when the variable is
not set. With `RAV1G_SM6TEST_SHADER_DIR=<dir>` (what `testing/sm6test/compile.py --out <dir>` writes)
`-sm6test` has its test shaders; without it the test reports that they are not built in.
`--features uavsync` builds the decoder for the headers of the uavsync variant.

On Linux, for Windows (clang 19 or newer with `clang-cl`, `lld-link` and `llvm-lib` on the path,
libclang, and `cargo-xwin`, which fetches the Windows SDK and the MSVC C runtime; no wine):

```sh
rustup target add x86_64-pc-windows-msvc
RAV1G_SHADER_DIR=<dir> cargo xwin build --release --target x86_64-pc-windows-msvc
```

The shader headers come from a Windows build (FXC): CI keeps them as the artifact
`rav1g-shader-headers-<run id>`.

CI: the job `rust` builds the program with MSVC (artifact `rav1g-dec-x64-<run id>`), and the jobs
`rust-conformance`, `rust-parser-c` and `rust-switches` run every check of this document with it,
side by side ([The jobs of a push](#the-jobs-of-a-push)); `rust-cross` builds it on Linux
(`rav1g-dec-x64-xwin-<run id>`) and `rust-cross-run` and `rust-cross-run-parsers` run that program:
the conformance set, the player's decoder code, the decode errors, the order check, the memory
check, `-poisonmi`, `-poisonbuf`, the set with `-interfast` and with `-gpuclear`, and all of it with
`-parser c`; `cpp-reference` and `rust-equal` are the comparison with the C++ decoder.

## What it shares with the test player

- **The decoder**: the Rust library of the workspace (`crates/rav1g`) with the C of the parser and
  entropy decoder (`crates/rav1g-sys`: the files of `c_sources.txt` at `/O1 /Ot`, the marked ones at
  `/O2`, asserts compiled in, link time optimisation), built from the same shader headers as the
  libraries of the player's packages.
- **The decoder configuration** of `AV1Decoder::DecodeOne` in
  `testing/player_common/decoder.cpp`: the first key frame is looked for with
  `aom_codec_peek_stream_info`, the decoder is created for the stream's largest frame size
  rounded up to 128 and its bit depth, with two pipeline slots and the shared back buffer,
  CDEF runs through `cdef_filter_v2.hlsl`. The player's flags switch the same things:
  `-cdefv1`, `-nofitsize`, `-lowmemparts <n>`, `-lowmem`, `-slots <n>`, `-intrav2`,
  `-intraopt`, `-gpuclear`, `-cpuclear`, `-syncclear`, `-parser c`, `-parseopt <list>`,
  `-t <threads>`, `-parseframes <n>` (frames in parse; the `CONFIGURATION` line of a task says what
  the decoder was created with), `-limit <n>`.
- **The output path**: the decoder asks for an output image for every frame that is output
  and hands it back through the frame-ready callback; the frame is hashed there.
- **The stream readers** of `testing/common` (IVF, WebM/Matroska, OBU, MP4), compiled into the program
  (`tools/rav1g-dec/shim`), with the timing summary, the stage dump writer and the Shader Model 6 test.
- **The hash**: the visible samples of the luma plane, then of U, then of V; one byte a sample
  for 8-bit, two bytes (low byte first) above that; luma only for a monochrome stream; each
  frame at the size it has.
- **The player's decoder code itself**, for `-playersmoke` ([below](#the-test-players-decoder-code)).

What differs: the output images are readback buffers only (the player also creates the
textures it presents), there is no budget guard unless `-budget` asks for one (a desktop
process has no app memory limit), the stream is read on the decoding thread, and every task
gets a new decoder. The program uses the decoder through the Rust interface of the crate; with
`-capi` through the functions of `rav1g.h`, as the player does.

## Flags of rav1g-dec that warp_decoder does not have

- `-capi`: the decoder is used through its interface for C, the functions of `rav1g.h`, instead of
  the Rust interface (docs/rust-port.md, step 5); the output is the same, and the program says so in
  an `INTERFACE` line. CI decodes the whole set once more with it.
- `-continue_on_error` (also a flag of the test player): a decode call that fails does not end the
  task. The error is counted and the next temporal units go to the same decoder, which is what an
  application does with a damaged stream; one `CONTINUE <stream>: decode error <n> at temporal unit
  <u>: <text>` line per error, and `CONTINUE <stream>: <n> decode errors, <m> temporal units decoded
  behind the first, <f> frames out` behind the task. The result line is that of the frames (`nocheck`
  without a reference), with the errors in its detail.
- `-keep_going`: a task that ends with a decode error does not end the run; the tasks behind it run
  in the same process, and the exit code is still 2.
- `-budget <MiB>`: the decoder is created with a budget callback that says the application has this
  much memory left for it, so the budget guard of the creation can be run on a desktop: with `-v` the
  `MEMORY budget` line of an accepted creation, or the wait and the `MEMORY REFUSED` line of a refused
  one (the task then fails at the creation of its decoder).
- `-memstate`: `MEMSTATE stream=<name> at=<before|decoded|released> private=<bytes> commit=<bytes>
  working_set=<bytes> dxgi_local=<bytes> dxgi_non_local=<bytes> settle_ms=<n>` lines: the private
  bytes of the process (`PrivateUsage`; `commit` is `PagefileUsage`, the same number where the system
  reports both) and the video memory the adapter reports for the process
  (`IDXGIAdapter3::QueryVideoMemoryInfo`, -1 where it does not). `before`: at the start of a task;
  `decoded`: its frames are out, its decoder and its output images still exist; `released`: nothing
  of the task exists any more. The released state is read when it has settled: again every 20 ms
  until the private bytes have not gone down by 64 KiB for 100 ms, for 2 s at most. For the
  [memory check](#memory-check).
- `-playersmoke <directory>`: see the next section. (`warp_decoder` has the flag too, on the C++
  library.)

Other differences: the usage text names `rav1g-dec`; the stream path goes to the C stream reader as
UTF-8 where `warp_decoder` passes the bytes of its command line (the same for plain ASCII); lines end
with a line feed where `warp_decoder`, a C program writing to a redirected stdout, ends them with
carriage return and line feed. The md5 files are the same bytes (`<md5>   <n>\r\n`).

## The test player's decoder code

The test player is a UWP app and cannot run on a CI runner, and a path that CI only compiles is not
tested: two bugs of the player's port to `rav1g.h` were found on the console that way
([rust-port.md](rust-port.md), step 5). So the part of the player that uses the decoder
(`testing/player_common`: the task file, the decoder creation with a device and no queue, the output
pool, the md5 check, the timing run, `-continue_on_error`) is compiled into `rav1g-dec` from the same
files, and

```bat
rav1g-dec -playersmoke <directory>
```

runs it as the player runs it: the tasks are the lines of `<directory>/args/args_av1.txt`, as in the
player's LocalState, stream paths are relative to `<directory>`, and the player's log goes to stdout.
It calls the functions of `rav1g.h` that the `rav1g` crate of the program has, which are the functions
the libraries of the packages have; nothing of the C++ decoder is involved. The exit code is 0 when
the player ended its launch (what its tasks did is in its log) and 2 when it did not within 25
minutes.

`testing/conformance/player_smoke.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>` is the check
of CI: five tasks (a small 8-bit vector; 10-bit with four threads, three frame slots and `-gpuclear`;
a stream whose frames change size, from Matroska, with `-r -lowmem -cpuclear`; spatial layers with
`-gputime -syncclear`; the HDR10 stream), each of which has to end with `MD5 is OK`; the log has to
say who clears the grids of a slot in each task (`Clears:`), name the Rust library
(`LIBRARY rav1g ... the Rust host`) and have no error line; the launch has to end, and so does a
launch without a task file, which says `No args were found`. The `rust-conformance` job runs it with
the MSVC build and `rust-cross-run` with the build of Linux, whose C and C++ are compiled by clang as in the
default package of the player. The cases with a decode error are in
[A decode error in the middle of a stream](#a-decode-error-in-the-middle-of-a-stream).

## Equality with the C++ decoder

Phase 1 of the Rust port kept the output of the decoder byte for byte, and CI goes on checking that
against the C++ decoder, which is no longer in this branch:

- The job **`cpp-reference`** checks out branch `cpp` at the commit that `CPP_REFERENCE_COMMIT` of
  `.github/workflows/build.yml` names (df73d8c, the head of `cpp` on 2026-11-13), builds its
  `warp_decoder` with its own shaders, and runs it with the scripts, streams and task lists of this
  commit: the whole set (`run.py`; it has to give the results of `expected.txt` too), the stage dumps
  of `dump_tasks.txt`, and the MEMORY lines of ten decoder configurations (`memory_lines.py`).
  Artifacts `rav1g-cpp-reference-<run id>` and `rav1g-cpp-reference-stage-dumps-<run id>`.
- The job **`rust-equal`** first checks that the shaders of this commit are those of the pinned
  commit (`shaders.py same`, on the reports of the two shader builds), then compares, `rav1g-dec`
  against `warp_decoder`: the md5 of every output frame of every stream, the bytes of every stage
  dump (`compare_md5.py`), and the MEMORY lines (`memory_lines.py compare`). The host pool is left
  out of that comparison: the C++ decoder lies in its pool, the Rust decoder is an allocation of its
  own next to a pool that holds only what the entropy decoder is given, so the pool of a configuration
  is 74,688 bytes smaller by design ([rust-port.md](rust-port.md#the-host-pool-and-the-queues-2026-11-13));
  its two numbers in the `decoder created` line are made equal and the `host memory` line, which only
  the Rust decoder writes, is dropped. The device buffers, the frame buffers, the options, the budget
  decision and the steps of the release are compared as before.

The pin is bumped on purpose, in the workflow and here, when a change both decoders have (a shader,
the C of the parser) is on both branches. The comparison is between two hosts that run the same
shaders; it **ends when the shaders of the branches diverge**, which is planned for the switch to DXC
and for phase 2 ([rust-port.md](rust-port.md#equality-reference)). `rust-equal` then fails at its
first step and says so, and the jobs `cpp-reference` and `rust-equal` are removed; the references of
dav1d and AOM, which every pass of the set is held against, stay.

## Run

```bat
rav1g-dec -i clip.ivf -save_md5 clip.ivf.md5
rav1g-dec -i clip.ivf -check clip.ivf.md5
rav1g-dec -i av1-1-b8-01-size-16x16.ivf -check av1-1-b8-01-size-16x16.ivf.md5 -aom_md5
rav1g-dec -list tasks.txt -t 4
rav1g-dec -sm6test
```

| Flag | |
| --- | --- |
| `-i <file>` | the stream: `.ivf`, `.mkv`/`.webm`, `.obu`, `.mp4` |
| `-check <file>` | compare with this md5 file: the first 32 characters of each line are the md5 of one output frame, in output order. Reads the player's format and AOM's |
| `-md5_frame_check` | the same with `<stream>.md5`, as in the player |
| `-save_md5 <file>` | write the md5 of every output frame in the player's format, `<md5>   <n>\r\n` |
| `-aom_md5` | hash a monochrome frame with two grey chroma planes, as `aomdec --i420` writes it; see [AOM's md5 files](#aoms-md5-files) |
| `-o <file>` | write the visible samples of every output frame, as they are hashed |
| `-save_wrong_yuv <dir>` | with a reference: write each frame that differs to `<dir>/<name>_<frame>.yuv` |
| `-limit <n>` | decode only the first n temporal units |
| `-dumpstages <frames>` | write the buffers behind every GPU stage of these frames to `<dir>/<name>_f<n>.stages`; see [Stage dumps](#stage-dumps) |
| `-dumpdir <dir>` | where `-dumpstages` writes (default `dumps`; created when it is not there) |
| `-t <n>` | decoder threads (default 1) |
| `-queue_size <n>` | output images in the pool (default 4) |
| `-timeout <seconds>` | give up on a task after this time |
| `-name <text>` | what the `BEGIN` and `RESULT` lines call the stream (default: its file name) |
| `-cdefv1`, `-nofitsize`, `-lowmemparts <n>`, `-lowmem`, `-slots <n>`, `-intrav2`, `-intraopt`, `-gpuclear`, `-cpuclear` (`-asyncclear` is the same), `-syncclear` | as in the [test player](test-player.md#defaults-and-opt-out-flags); the task line of a run names the choice (`clears none needed`, `clears GPU (-gpuclear)`, `clears completion thread (-cpuclear)`, `clears decode thread (-syncclear)`) |
| `-lfhsplit <n>`, `-intraedgefix` | the [experiments](test-player.md#experiments) of the test player; with the `experiments` input the `rust-experiments` job runs the whole set with each, and with a program built with `--features uavsync` from the shaders of the uavsync package (it prints `SHADERS uavsync`) |
| `-poisonmi`, `-poisonbuf` | debugging, off by default: the decoder fills the mode info pool (`-poisonmi`) or every buffer the CPU writes for the shaders (`-poisonbuf`) with the byte 0xA5 when it creates it, instead of leaving what the memory held. With `-poisonbuf` it also fills the constants of every stage again in every frame, right before the stage writes them (their memory is reused from frame to frame). What the decoder clears or initialises after the creation is as in a normal run. The output must be the same; a shader that reads bytes the CPU never wrote gives another one, or loses the device. The decoder passes both. See [Order check and poisoned buffers](#order-check-and-poisoned-buffers) |
| `-interfast`, `-interfastlists` | the [inter fast path](test-player.md#inter-fast-path--interfast) of the test player: inter blocks listed by the fraction of their motion vectors (`inter_fast`, `inter_compound_fast`; with `-interfastlists` the original shaders on those lists), same output; the `rust` job decodes the whole set once more with `-interfast` on every push |
| `-interfastmode <n>` | `av1_set_inter_fast(n)` for diagnosis: 1 is `-interfast`, +2 keeps `inter_main` on the lists of one reference, +4 `inter_compound` on those of two (7 is `-interfastlists`); 8 only makes room for the lists in the block maps and leaves the path off |
| `-gputime`, `-gputimedetail`, `-gputimeisolate`, `-progress` | the [timing run](test-player.md#timing-run--gputime-and--nopresent) of the test player and its [inter breakdown](test-player.md#inter-prediction-breakdown--gputimedetail--gputimeisolate): the summary as `GPUTIME ...` lines on stdout, with `-progress` the per-frame lines too. On WARP the times are those of a CPU rasterizer; the counters are the stream's |
| `-alllayers` | a stream with spatial layers: output the frame of every layer of a temporal unit (`AV1D_SET_OUTPUT_ALL_LAYERS`); the default is one frame per temporal unit, the highest layer it has. See [Layers and new sequences](test-player.md#layers-and-new-sequences) |
| `-oppoint <n>` | a stream with layers: decode operating point n of its sequence header (`AV1D_SET_OPERATING_POINT`, default 0) |
| `-list <file>` | one task per line, with the flags above; flags on the command line are the defaults of every line. A decode error, a timeout or a crash ends the process at that task |
| `-sm6test` | run the [Shader Model 6 test](test-player.md#shader-model-6-test--sm6test) shaders on the device before the tasks, one `SM6TEST ...` line per shader. Needs no stream: without `-i` and `-list` the program ends after the test, with exit code 0 whatever the device did with the shaders and 2 when the test could not run |
| `-hw` | the first hardware adapter instead of WARP |
| `-v` | print the decoder's log lines (`MEMORY ...`) |

Unlike the player, paths are used as given: nothing is put in front of them.

Output, one line each on stdout:

```text
ADAPTER Microsoft Basic Render Driver (software), vendor 0x1414 device 0x008C; shader double precision reported
PIPELINE states created in 2210 ms
BEGIN stream=clip.ivf
TASK clip.ivf: stream 1920x1080 10-bit, decoder 1920x1152 10-bit (fit to the stream), slots 2, ...
MISMATCH frame 12 (1920x1080 10-bit): expected 0f3a..., got 77c1...
RESULT stream=clip.ivf status=mismatch frames=40 matched=12 first_bad=12 ref_frames=40 decode_ms=41250 create_ms=310 size=1920x1080 bitdepth=10 detail="40 temporal units in, 40 frames out"
```

After the `ADAPTER` line come the `CAPS ...` lines, at every launch: what the device reports about
itself (shader model, wave operations, native 16-bit types and more; see the
[Shader Model 6 test](test-player.md#capability-report-caps)).

A `PROCESS before the task: ...` and a `PROCESS after the task: ...` line give the private bytes
and working set of the process and the free memory of the machine: the tasks of a list run in one
process, and a D3D12 device that runs out of memory is removed.

Two kinds of `COLOUR` line say what the stream says about its colour, which the decoder passes
to the application and does not use ([color.md](color.md)): `COLOUR <stream> sequence: ...` is the
colour description of the sequence header a decoder is created for (the stream info), and `COLOUR
<stream> frame <n>: ...; <MDCV>; <CLL>` what a frame carries, with its HDR metadata: for the first
frame of a decoder and for every frame that says something else than the frame before it.

```text
COLOUR clip.ivf sequence: primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2 (colocated)
COLOUR clip.ivf frame 0: primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2 (colocated); MDCV R 0.6800,0.3200 G 0.2650,0.6900 B 0.1500,0.0600 W 0.3127,0.3290 L 0.0050..1000 cd/m2 [44564,20972 17367,45220 9830,3932 20493,21561 256000 82]; CLL 1000/400
```

A `RECONFIGURE <stream>: temporal unit <n> starts a sequence of ...; new decoder ...` line
says that the stream went on with a sequence that did not fit the decoder (larger frames, or
more bits a sample) and that the program replaced the decoder, as an application has to
([Layers and new sequences](test-player.md#layers-and-new-sequences)); a second `TASK` line
describes the new one, and the `detail` of the result ends with `a new decoder for a larger
sequence`.

`status` is `pass` (every frame matches and the number of frames is the reference's),
`mismatch` (a frame differs, or the number of frames: without `-limit` the stream must give
exactly the frames of the reference, with it no more than those), `error` (the decoder
refused the stream or stopped with a decode error; `detail` has the decoder's message),
`timeout`, or `nocheck` (decoded, no reference given). `first_bad` is the first output frame
that differs, counted from 0. At most 16 `MISMATCH` lines are printed per task; the md5 of
every frame is in the `-save_md5` file.

Exit code: 0 every task decoded and matched, 1 a frame or a frame count differs, 2 a decode
error (or `-sm6test` could not run its test), 3 bad arguments or no D3D12 device, 4 timeout. A crash (an access violation, a failed
assert of the library) ends the process without a `RESULT` line: a `CRASH exception 0x... at
<module>+0x<offset>` line names the place, which the program's `.pdb` resolves (`rav1g_dec.pdb` in
the artifact `rav1g-dec-x64-<run id>`).

## Stage dumps

`-dumpstages <frames>` is the test player's flag
([test-player.md](test-player.md#stage-dumps--dumpstages): the frame numbers, the stages, the
file format's use): for the listed frames the buffer behind every GPU stage of the frame's
command list is written to `<dumpdir>/<name>_f<n>.stages`. The player on a console and
warp_decoder on WARP write the same files for the same stream, and
`testing/stage_dump/stage_dump.py compare` says at which stage the two part and how. A file
also holds the buffers the stages read besides the frame (mode info, prediction block
records, loop filter constants and edge lists, CDEF and loop restoration lists), which
`compare` prints decoded where they differ. After a `RECONFIGURE` the frame numbers start
again and the files are `<name>.seq<n>_f<n>.stages`.

```bat
rav1g-dec -i clip.ivf -limit 2 -dumpstages 0,1 -dumpdir dumps
python testing/stage_dump/stage_dump.py list dumps
python testing/stage_dump/stage_dump.py compare console-dumps dumps
```

The `rust-conformance` job writes such dumps on WARP for the streams and frames listed in
`testing/conformance/dump_tasks.txt` (`<stream> <frames> [<temporal units>]`), after the
run, and keeps them in the artifact `rav1g-dec-stage-dumps-<run id>` (those of the C++ decoder
are in `rav1g-cpp-reference-stage-dumps-<run id>`), one directory per stream
with the dumps, the task's log and its md5 file:

- vectors (`aom`) and streams encoded in the job (`svt`): the `.stages` files. An `svt`
  stream is ffmpeg's test pattern, and what the encoder makes of it depends on the encoder's
  version, so the stream itself is in the artifact too (`streams/`): put that file on the
  other device to compare the same frames.
- cuts of published streams (`cut`): `.digest` files only, a hash per row and a CRC per 8x8
  block of every plane of every stage. A digest has no samples; neither the streams nor
  their decoded frames leave the job. Of the data buffers it keeps the loop filter, CDEF and
  loop restoration lists as they are, and a CRC per 4x4 unit of the mode info and per
  prediction block record. `stage_dump.py compare` takes a digest on either side
  and then reports which rows and 8x8 blocks differ, not by how much. The same cut can be
  made anywhere with the line of `streams.txt` (`ffmpeg -ss <start> -i <url> -map 0:v:0
  -frames:v <frames> -c copy -f ivf`).

To get dumps of other frames, edit `dump_tasks.txt` and push; the job prints what it dumped
(`stage_dump.py list`) in the step's log, and compares every directory with itself, which
reads every buffer of every dump the way a comparison with another device does.

## AOM's md5 files

The decoder test vectors of the Alliance for Open Media
(`https://storage.googleapis.com/aom-test-data/av1-1-b8-*`, `av1-1-b10-*`) each come with an
`.md5` file. It has **one line per output frame**, in output order:

```text
6353b245c305a5f4f2845ee7ad2b128b  av1-1-b8-01-size-16x16.ivf-16x16-0001.i420
```

the md5 of the frame as `aomdec --i420` writes it, at the size the frame has (the size is
part of the name: `av1-1-b8-03-sizeup.mkv` goes from 352x288 to 1280x720 at frame 11). libaom's
own test (`test/test_vector_test.cc`, `test/md5_helper.h`) reads the first field of each line
and compares it with the md5 of each decoded frame.

That hash is the test player's hash, with one exception: for a **monochrome** stream `aomdec
--i420` writes two chroma planes of the mid grey value (128, or 512 as 16 bits for 10-bit)
after the luma plane, and the player (like dav1d) hashes luma only. `-aom_md5` makes
`warp_decoder` hash a monochrome frame the aomdec way; it changes nothing for other streams.
So AOM's files are used as published, with `-check <file>.md5 -aom_md5`. Checked against
dav1d (`ffmpeg -c:v libdav1d ... -noautoscale -f framemd5`): its md5s equal AOM's for the
vectors, and for the two monochrome vectors once the grey planes are added.

## Conformance run

`.github/workflows/build.yml` runs it on every push: one job has the streams, one builds the
program, and the jobs behind that one run it ([The jobs of a push](#the-jobs-of-a-push)); and once
more with the program built on Linux (`rust-cross-run`, `rust-cross-run-parsers`).

**`conformance-streams`** (Linux) runs `testing/conformance/prepare_streams.sh`, which reads
`testing/conformance/streams.txt` and fetches or makes:

- **AOM's test vectors**: all 244 `av1-1-b8-*` and `av1-1-b10-*` vectors of the bucket that
  have an `.md5` file (quantizer 0..63 for 8 and 10 bit, 100 frame sizes from 16x16 to
  226x226, all-intra, size changes at key frames, CDF update, motion vectors, motion field
  motion vectors, intra-only frames with intra block copy, temporal and spatial layers, film
  grain, monochrome), with AOM's md5 files.
- **Cuts of Netflix Open Content** (`http://download.opencontent.netflix.com.s3.amazonaws.com/AV1/`):
  a number of frames from a key frame on, copied without re-encoding (`ffmpeg -ss <start>
  -i <url> -frames:v <n> -c copy`), 270p to 2160p, 8 and 10 bit. The reference is dav1d's
  output (`testing/md5ref/make_md5_ref.sh`).
- **Streams encoded in the job** with SVT-AV1 from ffmpeg's `testsrc2` pattern, for features
  the other streams do not have: reference scaling (`resize-mode=2`: frames coded at other
  sizes than the frames they predict from), super-resolution (`superres-mode=2`: all
  denominators), eight tiles with a key frame every 16 frames
  (`tile-columns=2:tile-rows=1:keyint=16`; every other stream has one tile), and HDR10 (BT.2020
  with the PQ transfer, a mastering display and a content light level as metadata OBUs: the only
  stream of the set with HDR metadata, for the check of what the decoder reports about the colour
  of a stream, [color.md](color.md#the-check-in-ci)). The script checks
  that the encoder produced the feature (frame headers with `use_superres`, more than one coded
  width, several tile columns, more than one key frame). Reference from dav1d.

The streams are media. They are never committed and never uploaded as an artifact: the job
keeps them in the Actions cache, keyed by the list and the scripts, and the Windows job
restores them from there. The job's artifact `rav1g-conformance-md5-<run id>` has the md5
references and the task list only.

**`rust`** (Windows; it was `conformance`, with `warp_decoder`, while the C++ host was in this
branch) builds `rav1g-dec`, and **`rust-conformance`** (Windows) runs `testing/conformance/run.py`
with it: the
vectors in one decoder process, in the order of the list, with `-t 4`, and every large stream
(the cuts and the streams encoded in the job) in a process of its own (see
[Results](#results) for why). A task that ends in a
decode error, a timeout or a crash ends its process (a failed decode may leave the device in
a state the next stream should not inherit), and the run goes on with the next stream in a
new process. A stream that fails after other streams in the same process is decoded again
alone and must fail the same way; otherwise its result is `unstable`, which fails the run and
cannot be listed in `expected.txt` (with one exception, see [Results](#results): a run that lost
the D3D12 device is repeated). Every task has a
timeout (300 s for a vector, 2400 s for the larger streams), so a hang ends that stream, not
the job.

Behind the build the `rust` job runs `cargo test` and `rav1g-dec -sm6test`, and shows the `CAPS` and
`SM6TEST` lines of WARP in its summary
([Shader Model 6 test](test-player.md#shader-model-6-test--sm6test)); the log is `warp.log` in the
artifact `rav1g-dec-sm6test-<run id>`.

The result is a table in the job summary, one row per stream: size, bit depth, frames,
matching frames, first bad frame, decode time, result. The artifact
`rav1g-dec-conformance-<run id>` has `results.tsv`, `summary.md`, the decoder's output for every
stream (`logs/<stream>.log`) and the md5 of every output frame (`md5/<stream>.md5`, in the
player's format).

**`colour`** (Linux) reads the `COLOUR` lines of three of these runs in the artifacts of
`rust-conformance` and `rust-parser-c` (the whole set with rav1d's parser, through `rav1g.h`, and
with `-parser c`) and holds them against FFmpeg's
`trace_headers` and against `ffprobe` with libdav1d, stream by stream
(`testing/conformance/colour_check.py`, [color.md](color.md#the-check-in-ci)): the colour description
of the sequence header, and the colour properties, the mastering display and the light levels of
every output frame.

### The jobs of a push

Until 2026-11-13 one job, `rust`, built the program and ran every check with it, one after the
other: 50.5 minutes of a push that took 51.8 (run 43517057205), 17.4 of them the paired timing of
the two parsers. Now `rust` builds the program once and keeps it as an artifact, and the jobs behind
it take it from there and run side by side, each on its own runner with the streams from the cache.
Run 43540270287 (b766420), 17.1 minutes for the push:

| Job | What it runs | Artifacts (`-<run id>`) | Minutes |
| --- | --- | --- | ---: |
| `rust` (Windows) | `cargo build --release` with MSVC, the shader checks, `cargo test`, `-sm6test` | `rav1g-dec-x64`, `rav1g-dec-sm6test` | 2.1 |
| `rust-conformance` (Windows) | the player's decoder code; the whole set; the parse digest; the whole set with `-capi`; decode errors in cut streams (Rust interface, `rav1g.h`, player); the order check; the memory check; the timing smoke test; the stage dumps; the MEMORY lines | `rav1g-dec-conformance`, `rav1g-dec-stage-dumps`, `rav1g-parse-digest-msvc` | 8.0 |
| `rust-parser-c` (Windows) | with `-parser c`: the whole set; the part of the set with `-poisonmi`, with `-poisonbuf` and with `-capi`; the whole set with `-interfast`; decode errors in cut streams (three ways); the order check; the memory check; the player's decoder code; the stage dumps; the MEMORY lines | `rav1g-dec-parser-c` | 12.3 |
| `rust-switches` (Windows) | the part of the set with `-poisonmi` and with `-poisonbuf`; the whole set with `-interfast`; the whole set with `-syncclear` (with the memory check up to 2026-11-13, in the 10.7 minutes; the memory check is with `-gpuclear` now, in `rust-clears`); with `-parser rav1d -parseopt none,latelists,time` the whole set, the stage dumps, the player's decoder code and the decode errors in cut streams | `rav1g-dec-switches` | 10.7 |
| `rust-clears` (Windows; since 2026-11-13, [below](#the-clears-of-a-slot-in-ci)) | who clears the grids of a slot and how much (`clears_check.py`); the whole set and the memory check with `-gpuclear`; the whole set and the stage dumps with `-cpuclear`; the part of the set with `-gpuclear -parser c` and with `-gpuclear -poisonbuf -parseopt cleargrid -slots 3 -t 4` | `rav1g-dec-clears` | 7.9 (run 43558835990) |
| `rust-frames` (Windows; since 2026-11-14, [phase2-rav1d.md](phase2-rav1d.md#step-4-frames-in-parallel)) | frames in parallel: what a decoder is created with, the budget guard's choice, a panic in the parse and the timers with one and two frames in parse (`frames_check.py`); the whole set with `-parseframes 2 -t 2`; decode errors in cut streams with 2 and 3 frames in parse (`cut_tile_check.py --frames-in-parse`); the stage dumps and the memory check with two frames in parse | `rav1g-dec-frames` | |
| `rust-frames-threads` (Windows; since 2026-11-14) | the whole set with `-parseframes 3 -t 4`, `-parseframes 4 -t 8` and `-parseframes 2 -t 3 -slots 3` | `rav1g-dec-frames-threads` | |
| `frames-matrix` (Windows; once a week, and by hand with the `frames_matrix` input) | the whole set with every combination of 1 to 4 frames in parse and up to 8 threads, twice: four jobs side by side | `rav1g-dec-frames-matrix-<frames>` | not a job of a push |
| `rust-parsers-equal` (Linux) | no decoder: the run, the dumps and the MEMORY lines of `-parser c` against those of the default; the dumps of the other parser switches against those of the default, byte for byte, and its run and dumps against those of `-parser c`; the dumps of `-cpuclear` against those of the default, byte for byte; the dumps of two frames in parse against those of one, byte for byte | | 0.4 |
| `rust-equal` (Linux) | the run, the dumps and the MEMORY lines of `rust-parser-c` against the C++ decoder (`cpp-reference`) | | 0.2 |
| `colour` (Linux) | the `COLOUR` lines of `rust-conformance` (two runs) and `rust-parser-c` | `rav1g-colour` | 0.8 |
| `rust-cross-run` (Windows) | the program of the Linux build: the whole set; the parse digest; the player's decoder code; decode errors; the order check; the memory check; `-poisonmi`, `-poisonbuf`, `-interfast`, `-gpuclear` (`-syncclear` up to 2026-11-13) | `rav1g-dec-xwin-conformance`, `rav1g-parse-digest-xwin` | 13.3 |
| `rust-cross-run-parsers` (Windows) | the program of the Linux build: everything with `-parser c`, and the other side of the parser switches | `rav1g-dec-xwin-parsers` | 13.5 |
| `parse-digest` (Linux) | the digests of `rust-conformance` and `rust-cross-run` against those of `parse-check` | | 0.1 |

The other jobs are as before and done within 5.5 minutes: `player` (4.5), `cpp-reference` (5.2),
`parse-check` (3.2), `rust-cross` (3.0), the host checks of the shaders, `rav1d`,
`conformance-streams`, `shader-headers`. A job is a draw of a runner: the whole set took 1.8
minutes in `rust-conformance` and 2.9 in `rust-parser-c` and in the two `rust-cross-run` jobs of
that run. In the two runs so far the longest job took 13.5 and 13.8 minutes, and the jobs of a
push were done after 17.1 and 17.8 minutes (the second is run 43540279557, without its
`rust-experiments` job).

- The artifact of the `rust` job before, `rav1g-dec-conformance-<run id>`, is three now, one per
  job, each with the directories of its runs (`conformance-c/`, `stage-dumps-c/` and
  `memory-lines-c/` are in `rav1g-dec-parser-c-<run id>`; `conformance-interfast/`,
  `conformance-syncclear/` and `stage-dumps-switches-rav1d/` in `rav1g-dec-switches-<run id>`).
- The comparisons of two runs need the output of two jobs, so they are a job of their own behind
  the three, `rust-parsers-equal`; it also runs when one of the three failed, on what that job wrote.
- A job that fails fails the run, and the jobs behind it are skipped: `rust-equal` wants
  `rust-parser-c`, `colour` wants `rust-conformance` and `rust-parser-c`, `parse-digest` wants
  `rust-conformance` and `rust-cross-run`.
- `rust-cross-run` was one job of 21 minutes; its two halves share nothing but the program.
- Every command of the workflow before is in the workflow now, once. Not on a push any more: the
  paired timing, below.

Two jobs run only when the workflow is started for them (`workflow_dispatch`):

- `rust-experiments`, with the `experiments` input: the whole set with each experiment flag and
  with the poisoned buffers (seven passes), and with the program the `rust` job then also builds
  with the shaders of the uavsync package (`rav1g-dec-experiments-<run id>`; 26.9 minutes in run
  43540279557).
- `parser-timing`, with the `parser_timing` input.

### The clears of a slot in CI

Since 2026-11-13 a frame clears no grid of its pipeline slot
([phase2-rav1d.md](phase2-rav1d.md#no-clears-by-the-cpu)): every run of the workflow that names no
flag has the wavefront of intra prediction at the edges of the tiles. The three ways that keep it in
the slot's grid and clear that stay as flags. The job `rust-clears` runs them:

- `testing/conformance/clears_check.py --tool <rav1g-dec.exe> --streams <dir> --out <dir> --stream
  <name>[:<temporal units>]`: the stream in eight ways with `-gputime -progress` (no flag,
  `-gpuclear`, `-cpuclear`, `-syncclear`; no flag and `-gpuclear` with `-parseopt cleargrid` and with
  `-parser c`), each against its reference, and from the frame lines of the timing run (`| cleared
  memset=N completion=N gpu=N`) who cleared how many bytes for every frame. Without a flag: nothing
  on the completion thread or the GPU, and less than 64 KiB a tile on the decode and tile threads
  (12,368 bytes a tile: the counters of its block lists). With `-gpuclear`: the command list of
  every frame, with the stage `slot_clear` in it, and no thread. With `-cpuclear`: the completion
  thread, the same bytes, frame by frame. With `-syncclear`: the decode thread. With the mode info
  grid cleared too: on the completion thread without a flag, on the GPU with `-gpuclear`, where a
  frame clears the bytes of both grids. The job runs it on the 2160p stream and on the stream with
  eight tiles; its table is in the summary of the job.
- The whole set and the memory check with `-gpuclear`; the whole set and the stage dumps with
  `-cpuclear` (`rust-parsers-equal` holds the dumps against those of the default, byte for byte); the
  part of the set of the poisoned runs with `-gpuclear -parser c` and with `-gpuclear -poisonbuf
  -parseopt cleargrid -slots 3 -t 4`.

With the job the push of run 43558835990 took 16.4 minutes (`rust-clears` 7.9, `rust-switches` 10.9,
the longest job `rust-cross-run` with 13.3); the artifact `rav1g-dec-clears-<run id>` has
`clears-check/` (the log of every run and `summary.md`), the directories of the runs of the set,
`memory-check-gpuclear/` and `stage-dumps-cpuclear/`.

Elsewhere: `rust-switches` decodes the whole set with `-syncclear`, as before; `rust-cross-run`
decodes the whole set with `-gpuclear` with the program of the Linux
build; the player smoke test has a task with each of the three flags and reads the player's
`Clears:` lines; `parse-check` runs `rav1g-parse-check -wave check` (the grid for one parser, the
edges for the other: the block lists have to be equal) with one thread and with four, and `-wave
grid` once.

### Paired timing of the parsers

`testing/conformance/parser_timing.py` times the decoder with `-parser c`, with `-parser rav1d` and
with rav1d and other switches in turns on one runner ([phase2-rav1d.md](phase2-rav1d.md)). It was a
step of every push and is not any more: the console makes the speed decisions, the C parser leaves
in the last step of phase 2, and the step took 17 minutes. The job `parser-timing` runs

- when the workflow is started by hand with the input:
  `gh workflow run build.yml --ref main -f parser_timing=true` (or "Run workflow" on the Actions
  page, with the box `parser_timing`), and
- once a week, Monday 03:23 UTC (`schedule`), with every job of a push.

Its tables are in the summary of the job, and the artifact `rav1g-parser-timing-<run id>` has, for
each of the four runs of the script (`parser-timing/`, `parser-timing-t4/`,
`parser-timing-d2-t4/`, `parser-timing-switches/`), `parser-timing.md` and the log of every timed
decode. On a runner of its own the job took 12.9 minutes (run 43540279557, started with both
inputs; the other jobs of the workflow run next to it).

### Order check and poisoned buffers

A stream that is wrong behind other streams and right alone is not a flake of the runner: the
decoder's output depends on what the process decoded before. That was the case until 2026-11-12
([gpu-av1-decoder.md](gpu-av1-decoder.md#padding-of-the-mode-info-record-2026-11-12)): a shader
read two bytes of struct padding in the mode info, and a decoder that got the memory of a
destroyed one found other bytes there than zero. `av1-1-b8-22-svc-L2T1-2` failed that way now and
then behind `svc-L2T1`, `av1-1-b10-00-quantizer-00` every time behind an 8-bit quantizer vector.
The repeat alone showed the stream as `unstable`, the job was run again, and it passed. Three steps
now test for it directly, the first in the `rust-conformance` job and the other two in
`rust-switches` (the workflow of branch `cpp` has the same steps with `warp_decoder`):

- **`testing/conformance/order_check.py`** decodes five sequences, each in one decoder process and
  in this order, every stream against its reference: `av1-1-b8-00-quantizer-00` then
  `av1-1-b10-00-quantizer-00`, and the ten vectors `av1-1-b8-00-quantizer-00` to `-09` then the
  10-bit one, both with `-t 1` and with `-t 4`; `svc-L2T1` then `svc-L2T1-2` with `-t 4`. The first
  result counts: no stream is decoded again alone. Only a sequence in which the D3D12 device was
  removed runs again, whole and in the same order, at most three times in all.
- **A part of the set with `-poisonmi`** (`run.py --tool-args=-poisonmi --only ...`): 137 streams
  (the quantizer vectors 00 to 09 of each bit depth, all other vectors, the 270p cut) against
  `expected.txt`. With the mode info pool filled with 0xA5 at creation, a read of bytes the entropy
  decoder never wrote shows in any stream, alone and at once, without the right decoder before it.
  The whole set with the flag is one of the passes of the `experiments` input.
- **The same part of the set with `-poisonbuf`** (`run.py --tool-args=-poisonbuf --only ...`): every
  buffer the CPU writes for the shaders filled with 0xA5 at creation (17 kinds, the mode info pool
  among them), and each constant record of a stage filled again when it is handed out, in every
  frame. The constants of a frame are pieces of one upload buffer per slot that is only rewound
  between frames, so a field that a stage does not write holds what an earlier frame had at that
  place; filled, it has no plausible value. The `experiments` input adds the whole set with
  `-poisonbuf`, with `-poisonbuf -interfast` and with `-poisonbuf -intrav2`.

All three pass (`cpp` 15af3fb, run 43277284568: 5 of 5 sequences in 14 s, 137 of 137 streams in
37 s with either flag; `main` 3271d43, run 43277284480: 12 s, 32 s and 30 s, and with `rav1g-dec`
14 s, 38 s and 40 s). Together they add 1.2 to 1.5 min to the job, the step with `-poisonbuf` 30
to 40 s of it. The whole set with these flags (run 43271723373, `experiments`): 251 of 251 in every
pass.

`-poisonbuf` failed until 2026-11-12 (34 of 137 streams right, run 43262574427), because of one
record: for a plane without loop restoration the host left the unit fields of the plane's constants
unwritten, and `loop_restoration.hlsl` computed an address from them and loaded from it without a
branch, from a root SRV, which has no bounds checking. That also happened in normal runs, with
stale data in those fields
([gpu-av1-decoder.md](gpu-av1-decoder.md#unwritten-fields-of-a-constant-record-2026-11-12)).

### A decode error in the middle of a stream

`testing/conformance/cut_tile.py <in.ivf> <out.ivf> [--unit N] [--fraction F] [--then-whole]` makes a
damaged copy of a stream: it keeps a part of the payload of the largest OBU with tile data of one
temporal unit and sets the sizes of the OBU and of the IVF frame right, so the stream parses up to
the cut. The decoder then fails behind the setup of the frame and before its submission: with
`Failed to decode tile data` in a stream of one tile, with `Truncated packet or corrupt tile size`
where the cut takes a later tile. `--then-whole` lets the undamaged stream follow in the same file.

`testing/conformance/cut_tile_check.py` does that to two streams of the set (a vector with one tile,
the stream with eight) and runs them, each time followed by an undamaged stream in the same process:
with `rav1g-dec` through the Rust interface and through `rav1g.h`, and with the decoder code of
the test player on the same library (`rav1g-dec -playersmoke`), all in the `rust-conformance` job. It wants: the task of a damaged stream ends with that error and has the frames before the
damage right; the stream behind it is right; with `-continue_on_error` the decoder goes on, and the
frames before the damage and from the next key frame on are those of the reference (in the stream
with eight tiles the key frame 11 temporal units behind the damage; for the vector, which has one
key frame, the whole stream appended); no device is lost, and the process ends. No damaged stream is
in the repository or in an artifact.

### Memory check

Two things went wrong on the console that no run here could see, because they are states a process
gets into over several decoders: a destroyed decoder left memory behind, and the player lost the
credit of the budget guard when a creation was refused, so that the next, smaller decoder was refused
as well ([test-player.md](test-player.md#budget-guard-memory-refused)).
`testing/conformance/memory_check.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>` runs two
sequences that would show them again, in the `rust-conformance` job (MSVC build) and in
`rust-cross-run` (the build of Linux).

**The warm sequence.** One `rav1g-dec -memstate -keep_going` process runs six tasks eight times over,
a few temporal units each: the largest stream of the set (3840x2160 8-bit; 470.7 MiB of device
buffers), a decoder of the 4K 10-bit size (`-nofitsize`, 656.3 MiB: the set has no 4K 10-bit stream),
1080p 10-bit with `-slots 3`, 1080p 10-bit with `-lowmem`, the 2160p stream with `-budget 300`,
which the guard must refuse (470.7 + 48 MiB), and a 720p stream with the same budget, which it must
accept (58.9 + 48 MiB). After every task, when its decoder, its output images and its stream reader
are gone, the program prints the private bytes of the process and the video memory the adapter
reports for it (`MEMSTATE ... at=released`).

What WARP does with the memory decides what can be checked. Measured on the runners of CI, with the
build before the [host pool change](rust-port.md#the-host-pool-and-the-queues-2026-11-13) and with the
build after it (runs 43384492217, 43387114817 and 43392151836: nine runs of 4, 8 and 12 iterations):

- **The video memory the adapter reports is exact.** `IDXGIAdapter3::QueryVideoMemoryInfo` gave 0
  bytes after each of 312 releases, and with a decoder it gives its buffers. A build in which every
  decoder keeps one reference to its coefficient buffer reported 67.8 MiB after the first release
  and 750.5 MiB after the 24th.
- **The private bytes after a release are not those before the first decoder.** The process starts
  the sequence at 74 MiB and is at 340 to 530 MiB after most releases: WARP and the D3D12 runtime
  keep memory of released buffers for the next ones, and how much differs from release to release
  (after the same task up to 170 MiB between two iterations of one run). It is the runtime's memory,
  not the decoder's, and the next decoder gets it back: the peak with the 4K 10-bit decoder is 911 to
  945 MiB in every iteration.
- **Only the release of the largest decoder makes the runtime give nearly all of it back**, so the
  lowest level of an iteration, its floor, is the number that repeats: 192 to 195 MiB in the first
  iteration, 268 to 272 in the second, 289 to 293 in the third, then 292 to 300, in every run and with
  both builds. Now and then it takes a step of about 10 MiB and keeps it (three of four runs of 12
  iterations ended at 309 to 314 MiB, one at 298).

So the script checks, and prints, per release and per iteration:

| Check | Tolerance | Measured without a defect |
| --- | --- | --- |
| Video memory after every release, against before the first task | 64 KiB | 0 bytes |
| The floor of iterations 5 to 8, against the floor of iteration 4 (three iterations warm up) | 40 MiB | at most 15.6 MiB up to iteration 8, 19.1 MiB up to iteration 12 |
| Private bytes after every release of iterations 5 to 8, against the highest level after a release of iterations 1 to 4 | 96 MiB | at most 27.5 MiB up to iteration 8, 32.2 MiB up to iteration 12 |
| The budget guard | `MEMORY REFUSED` for the task that must be refused and no other; `accepted` for the task with a budget that fits; `not checked` without a budget | |
| md5 | every task that decodes: `status=pass` | |

The second check is the sharp one for host memory: a decoder that leaves 2 MiB behind on average
(five decoders an iteration, four iterations) fails it. The third is loose on purpose and is there
for what the floor cannot see, memory that only comes back with the next large decoder. What the
check does not see is host memory below these tolerances that every decoder leaves, such as the
decoder's own struct of 69 KiB.

**The credit sequence.** The player's decoder code (`rav1g-dec -playersmoke`) with its
[model of an app with a memory limit](test-player.md#the-apps-memory-as-a-model--budget--retain),
`-budget 250 -retain 160 -queue_size 4` on every task: a 1080p 10-bit decoder (166.9 + 48 MiB of the
250 less the 25.2 MiB held back for the output pool) is created, used and destroyed for the next task
and leaves 160 MiB charged; the 2160p decoder does not fit and is refused; the 720p decoder needs
106.9 MiB, more than the 84.4 MiB the app has left for it and less than the 244.4 MiB with the
credit, so it is accepted only when the refusal before it kept the credit; the 2160p decoder is
refused again; a 270p stream runs behind it. Checked: refused exactly there, accepted and `MD5 is OK`
everywhere else, no error line but those of a refused creation, the launch ends. The numbers are the
model's and the same in every run.

**That it fails when it should** was tried with three builds that have a known defect (run
43392151836, the script as it is here):

- The host pool of every decoder is never freed (4 to 20 MiB a decoder): the floor rises by 58 MiB an
  iteration, so iterations 5 to 8 are 58.3, 115.7, 170.6 and 234.1 MiB above iteration 4, and from
  iteration 6 on the releases are 97 to 220 MiB above the highest of iterations 1 to 4. The video
  memory stays at 0. 16 failed checks.
- Every decoder keeps one reference to its coefficient buffer: the adapter reports 67.8 MiB after the
  first release and 1,501.0 MiB after the last, so each of the 48 releases fails, and the private bytes
  follow (the floor 188 to 764 MiB above iteration 4). 74 failed checks.
- The player forgets the credit when a creation is refused, which was the bug of the console: the
  720p creation is refused with 84.4 MiB left. 2 failed checks.

Without a defect the same script passed twice in that run: the floor of iterations 5 to 8 at most 9.3
and 15.6 MiB above iteration 4, the releases at most 1.5 and 8.5 MiB above the highest of iterations
1 to 4, the video memory 0 bytes after each of the 96 releases.

A D3D12 device that is removed while a sequence runs is not a result of the check; the sequence then
runs again in a new process, up to three times, and the script prints the `D3D12 DEVICE REMOVALS`
line of the other checks. The check takes about 1.5 min of a job.

### Results

On WARP the shaders run as D3D12 specifies them and not as the console's GPU happens to run
them, which is what the run is for. Upstream's shaders had 0 of 250 streams right there, with
another md5 on every decode: they counted on the 64 threads of a thread group running in lockstep
([gpu-av1-decoder.md](gpu-av1-decoder.md#lockstep-reliance-portability-2026-11-11)). With group
syncs where threads hand data to each other, the super-resolution fix
([test-player.md](test-player.md#super-resolution)) and the three fixes below, all streams of the set
(251 then; 252 with the eight-tile stream added on 2026-11-12, run 43366744476; 253 with the heavy
10-bit 2160p cut `nf-dvbdash-d2-10b-2160p.ivf` added on 2026-11-13; 254 with the HDR10 stream
`syn-hdr10-10b-1080p.ivf` of the same day) are md5-identical to their reference:

| streams | pass |
| --- | --- |
| quantizer 0..63, 8-bit (`av1-1-b8-00-*`) | 64 of 64 |
| quantizer 0..63, 10-bit (`av1-1-b10-00-*`) | 64 of 64 |
| frame sizes 16x16 to 226x226 (`av1-1-b8-01-*`) | 100 of 100 |
| all intra, CDF update, motion vectors, motion field (`-02`, `-04`, `-05`, `-06`), intra-only frames (`-16`) | 5 of 5 |
| size changes at key frames (`av1-1-b8-03-*`) | 2 of 2 |
| temporal and spatial layers (`av1-1-b8-22-svc-*`) | 5 of 5 |
| film grain, monochrome, 8 and 10 bit (`-23`, `-24`) | 4 of 4 |
| Netflix Open Content cuts, 270p to 2160p, 8 and 10 bit | 6 of 6 |
| SVT-AV1 reference scaling | 1 of 1 |
| SVT-AV1 super-resolution | 1 of 1 |
| SVT-AV1 eight tiles, three key frames | 1 of 1 |

`expected.txt` is empty. The five streams that were on it until run 43149171146:

- `av1-1-b8-03-sizeup.mkv` goes from 352x288 to 1280x720 with a new sequence header at temporal
  unit 10. The decoder, created for the first sequence, could not allocate the larger frames (a
  decode error). The library now reports the sequence that does not fit before it decodes
  anything of it (`AOM_CODEC_NEEDS_RECONFIGURE`), and the program creates a decoder for it and
  passes the temporal unit again, as an application has to
  ([test-player.md](test-player.md#layers-and-new-sequences)). 20 of 20 frames.
- The four vectors with two spatial layers (`svc-L2T1`, `-L2T1-2`, `-L2T2`, `-L2T2-2`) had two
  defects. The decoder output the frame of each layer, 16 frames for 8 temporal units, where
  AOM's md5 has the 1280x720 frame of each temporal unit: it now outputs the highest layer a
  temporal unit has, as libaom and dav1d do by default (the output process of the
  specification, section 7.18.1; `-alllayers` for the frames of all layers). And of the
  1280x720 frames only the first was right: a block with a local warp was clamped to the size
  of reference 0 of the frame, which in those frames is the 640x360 frame of the lower layer,
  instead of to the size of its own reference, the 1280x720 frame before
  ([test-player.md](test-player.md#host-side-check-of-the-warp-shaders)). 8 of 8 frames each.

**The D3D12 device is removed, once or twice in every run.** Somewhere in the list, mostly at one
of the last, large streams (a 720p or 1080p cut of Netflix Open Content) but once at a 352x288
vector, a stream loses the D3D12 device: its frames come back wrong or as the frame before, an
allocation or a `Map` fails with 0x887A0005 (`DXGI_ERROR_DEVICE_REMOVED`), and the task ends in a
decode error, or it ends as a mismatch and the next stream cannot create its decoder. It is
another stream from run to run. Decoded again alone, in a new process, the stream is right, every
frame, though once the run alone lost the device as well, at the second frame. It happened in
every run so far, also in the three with upstream's shaders (runs 43092362685, 43094607558,
43099052089), so it does not come from the group syncs, and it is not memory: the process holds
465 MiB at that point and the machine has 13 GiB free (the `PROCESS` lines). **The cause is not
known**, and it is the open problem of the run: whether it is WARP, the runner, or the decoder on
a slow device has to be found out.

**One cause found and removed (2026-11-12): a load outside the buffer that stands in for the
palette buffer.** `reconstruct_block` (the residuals of OBMC blocks and of palette blocks) read
`if (do_palette) palette_buf.Load2(...) else dst_frame.Load2(...)`, which FXC compiled to both
loads and a select: every thread loaded from `palette_buf` at the frame offset of its row. A slot
has a palette buffer only once a frame with screen content tools has used it; until then another
buffer of the slot is bound there (`idct_blocks_unordered`, 16 bytes per 4x4 unit: 3 317 760
bytes for a 1920x1152 decoder), and a frame with its borders is larger than that: 3 391 488
bytes with 8-bit samples (the last rows of V are behind the buffer's end), twice that with 16-bit
samples (everything below the middle of the frame). The value was never used, but on WARP a load
behind the end of a buffer reads whatever memory is there, and when there is none the device is
lost. That fits what was seen: the streams it hit were the cuts encoded with OBMC, at 720p and
1080p, another one from run to run.

How it was found: the first run with `-interfast` (43196198862) lost the device in the two
10-bit 1080p cuts every time, also alone. Frame dumps showed the frame before the loss byte
identical with and without the flag, and only making room for the lists of the flag without
using them (`-interfastmode 8`, 1 MiB more in one buffer of a slot) gave the same loss: the
layout of the process, not the new path. With the debug layer and GPU-based validation
(`-debuglayer -gpuvalidation`, which makes such a load harmless and reports it) the stream
passed, 30 of 30 frames, with over 30 000 reports of `Root descriptor access out of bounds`, the
stored ones all for this view: SRV, root parameter 2, 3 317 760 bytes available, offsets up to
6 146 763. The load is now behind a real branch (`[branch]`), in both builds of the shader; with
that the run decodes all 251 streams without and with `-interfast` and loses no device
(43202750602), and the validation no longer reports that view.

It also shows without a lost device: in run 43134793241 the super-resolution stream, the 247th
of its process, came out wrong from its first frame on (64 frames, no error), the three 8-bit
streams behind it were right, the 1080p 10-bit stream after those was wrong from its first frame
on, and only the stream behind that one found the device removed. Both streams are right alone.
So the large streams (the cuts and the streams encoded in the job: a task with a `-timeout` of
20 minutes or more) no longer share a process with what was decoded before them: each gets a
decoder process of its own (`run.py --alone-timeout`), and the vectors share one as before.
Frames that are wrong from the first one on, without an error, behind other decoders of the
process and right alone: that is what the padding read fixed on 2026-11-12 looked like
([Order check and poisoned buffers](#order-check-and-poisoned-buffers)), so these two streams may
have been its doing and not the device's. That has not been tested: the large streams keep their
own processes.

**A second cause found and removed (2026-11-12): a load at an address made of stale constants.**
`loop_restoration.hlsl` loaded an entry of the table of restoration units for every plane, also
for a plane that is not restored, and for such a plane the host had not written the fields the
address is computed from: they held what an earlier frame had left at that place of the constant
upload memory. The table is a root SRV, so the load went wherever those values pointed. In a run of
136 streams, 110 of 346 such plane records held stale values (run 43269330205). The host now writes
a valid unit grid for such a plane and the shader loads under a branch, for a restored plane only
([gpu-av1-decoder.md](gpu-av1-decoder.md#unwritten-fields-of-a-constant-record-2026-11-12)). It
is the candidate for the removals that were left after the palette load; whether they are gone is
what the count below is for.

Because the stream it hits changes, it cannot be listed in `expected.txt`. `run.py` decodes a
stream whose run lost the device again alone, up to three times, and takes the result of a run
that kept its device; every loss is reported under "D3D12 device removed" in the summary and on a
`D3D12 DEVICE REMOVED` line of the log. The run does not fail on a lost device. It fails, as
before, on a stream that fails with its device, and on one that loses the device three times
alone.

**The count.** Every run of `run.py` and of `order_check.py` ends with one line, also when nothing
was removed:

```
D3D12 DEVICE REMOVALS: 0 in 6 decoder processes (with -interfast)
D3D12 DEVICE REMOVALS: 1 in 8 decoder processes: nf-chimera-10b-1080p.ivf [first in its process]
```

One removal is one decoder process that lost the device (the first pass and the repeats alone
count alike), with the first stream of the process that reported it and where it was. The line is
in the step summary too, and a run with a removal gets a warning annotation
(`D3D12 device removed`). With `gh run view <run> --log | grep 'DEVICE REMOVALS'` the passes of a
run are one line each. The repeats stay until the count has been 0 over enough runs; since the fix
of 2026-11-12: 0 in the 213 decoder processes of the first three runs (43271723373 with the
`experiments` input, 43277284568 on `cpp`, 43277284480 on `main`).

Two streams that are wrong on the console pass here: the 8-bit 4K stream
(`nf-dvbdash-a1-8b-2160p.ivf`, luma off by ±1–2 on the console) and the stream with scaled
references (`syn-resize-10b-1080p.ivf`).

### expected.txt

`testing/conformance/expected.txt` lists the streams that fail today (none at the moment), one
per line:

```text
<stream> <mismatch|error|timeout|crash> <reason>
```

The job fails when

- a stream fails that is not listed (a regression, or a new stream that finds a bug),
- a stream is `unstable`: wrong behind other streams and right alone, or the other way round. The
  decoder's output then depends on what the process decoded before; that is a defect of the
  decoder, cannot be listed, and is no reason to run the job again,
- a listed stream fails in another way than listed,
- a listed stream passes: the fix has to take the stream off the list in the same change, so
  the list can only shrink (a ratchet),
- a listed stream is not part of the run.

`expected.suggested.txt` in the artifact has the failing streams of the run in this format.

### Run it locally

On Linux (or any shell with curl and an ffmpeg that has libdav1d and libsvtav1), the streams:

```sh
sh testing/conformance/prepare_streams.sh conformance-streams
```

On Windows, with that directory and Python 3:

```bat
python testing\conformance\run.py --tool target\release\rav1g-dec.exe ^
  --streams conformance-streams --out conformance-out
```

`--only <regular expression>` runs the streams whose name matches, `--tool-args "-cdefv1"`
adds flags to every task (`-hw` for a hardware adapter), `--no-repeat` skips the repeat of
failing streams. One stream by hand: take its line from `conformance-streams\manifest.txt`,
run `rav1g-dec` with those arguments from inside `conformance-streams`, and add `-o` or
`-save_wrong_yuv` for the samples.

### References for streams whose frames change size

`make_md5_ref.sh` passes `-noautoscale` to ffmpeg. Without it ffmpeg scales every frame to
the size of the first frame of the stream, and the md5 of a scaled frame matches no decoder:
for a stream with reference scaling such a reference reports every frame of another size
than the first as wrong, whatever the decoder does. The references made before this change
are only wrong for streams whose frames change size.
