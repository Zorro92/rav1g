# warp_decoder and the conformance run

`warp_decoder` (`testing/warp_decoder`) is the decoder as a desktop console program. It
decodes AV1 streams with libav1 on a D3D12 device, by default the **WARP software adapter**,
reads every output frame back and computes one md5 per output frame, the way the test player
does. It needs no GPU, no window and nothing of UWP, so it runs on a CI runner: every push
decodes AOM's decoder test vectors and a few other streams with it and compares the md5 of
every frame with a reference (the [conformance run](#conformance-run)).

It exists because decoder bugs were found only on the console, and only by luck. It does not
measure speed: WARP rasterises on the CPU.

## What it shares with the test player

- **The decoder**: the sources and shaders of `libav1/build/libav1.vcxproj`, compiled with
  the settings that project gives them in Release x64 (the same FXC profile and flags, the
  same per-file compiler optimisation, asserts compiled in). The library project itself is a
  Windows Store static library, which a desktop program cannot reference, so `warp_decoder`
  compiles the files itself; `testing/warp_decoder/libav1_sources.props` lists them and is
  written from `libav1.vcxproj` by `testing/warp_decoder/sync_sources.py` (CI checks that it
  is up to date). Nothing in the library is UWP-only.
- **The decoder configuration** of `AV1Decoder::DecodeOne` in
  `testing/player_common/decoder.cpp`: the first key frame is looked for with
  `aom_codec_peek_stream_info`, the decoder is created for the stream's largest frame size
  rounded up to 128 and its bit depth, with two pipeline slots and the shared back buffer,
  CDEF runs through `cdef_filter_v2.hlsl`. The player's flags switch the same things:
  `-cdefv1`, `-nofitsize`, `-lowmemparts <n>`, `-lowmem`, `-slots <n>`, `-intrav2`,
  `-intraopt`, `-asyncclear`, `-t <threads>`, `-limit <n>`.
- **The output path**: the decoder asks for an output image for every frame that is output
  and hands it back through the frame-ready callback; the frame is hashed there.
- **The stream readers** of `testing/common` (IVF, WebM/Matroska, OBU, MP4) and the md5 code.
- **The hash**: the visible samples of the luma plane, then of U, then of V; one byte a sample
  for 8-bit, two bytes (low byte first) above that; luma only for a monochrome stream; each
  frame at the size it has.

What differs: the output images are readback buffers only (the player also creates the
textures it presents), there is no budget guard (`memory_available_cb` is not set: a desktop
process has no app memory limit), the stream is read on the decoding thread, and every task
gets a new decoder.

## Build

Windows, Visual Studio 2022 or later with the C++ desktop workload and a Windows 10/11 SDK:

```bat
msbuild testing\warp_decoder\warp_decoder.vcxproj /m /p:Configuration=Release /p:Platform=x64
```

The program is `testing\warp_decoder\bin\x64\Release\warp_decoder.exe` (statically linked
runtime, no other files needed). The project is not part of `av1xbox.sln` and writes nothing
into the directories that solution builds in: the shader headers go to
`testing\warp_decoder\obj`. CI builds it with the `v145` toolset and the `10.0.26100.0`
SDK and keeps it as the artifact `rav1g-warp-decoder-x64-<run id>`. In CI the project does
not compile the shaders: it gets the headers of the workflow's shader step through
`/p:Rav1gPrebuiltShaders=<directory>` ([test-player.md](test-player.md#shader-cache-of-the-workflow)),
the same bytecode the test player's package has.

## Run

```bat
warp_decoder -i clip.ivf -save_md5 clip.ivf.md5
warp_decoder -i clip.ivf -check clip.ivf.md5
warp_decoder -i av1-1-b8-01-size-16x16.ivf -check av1-1-b8-01-size-16x16.ivf.md5 -aom_md5
warp_decoder -list tasks.txt -t 4
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
| `-cdefv1`, `-nofitsize`, `-lowmemparts <n>`, `-lowmem`, `-slots <n>`, `-intrav2`, `-intraopt`, `-asyncclear` | as in the [test player](test-player.md#defaults-and-opt-out-flags) |
| `-lfhsplit <n>`, `-intraedgefix` | the [experiments](test-player.md#experiments) of the test player; the conformance job runs the whole set with each, and with a decoder built with `/p:Rav1gUavSync=1` (the shaders of the uavsync package; the program prints `SHADERS uavsync`) |
| `-alllayers` | a stream with spatial layers: output the frame of every layer of a temporal unit (`AV1D_SET_OUTPUT_ALL_LAYERS`); the default is one frame per temporal unit, the highest layer it has. See [Layers and new sequences](test-player.md#layers-and-new-sequences) |
| `-oppoint <n>` | a stream with layers: decode operating point n of its sequence header (`AV1D_SET_OPERATING_POINT`, default 0) |
| `-list <file>` | one task per line, with the flags above; flags on the command line are the defaults of every line. A decode error, a timeout or a crash ends the process at that task |
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

A `PROCESS before the task: ...` and a `PROCESS after the task: ...` line give the private bytes
and working set of the process and the free memory of the machine: the tasks of a list run in one
process, and a D3D12 device that runs out of memory is removed.

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
error, 3 bad arguments or no D3D12 device, 4 timeout. A crash (an access violation, a failed
assert of the library) ends the process without a `RESULT` line: a `CRASH exception 0x... at
<module>+0x<offset>` line names the place, which `warp_decoder.map` resolves.

## Stage dumps

`-dumpstages <frames>` is the test player's flag
([test-player.md](test-player.md#stage-dumps--dumpstages): the frame numbers, the stages, the
file format's use): for the listed frames the buffer behind every GPU stage of the frame's
command list is written to `<dumpdir>/<name>_f<n>.stages`. The player on a console and
warp_decoder on WARP write the same files for the same stream, and
`testing/stage_dump/stage_dump.py compare` says at which stage the two part and how.

```bat
warp_decoder -i clip.ivf -limit 2 -dumpstages 0,1 -dumpdir dumps
python testing/stage_dump/stage_dump.py list dumps
python testing/stage_dump/stage_dump.py compare console-dumps dumps
```

The conformance job writes such dumps on WARP for the streams and frames listed in
`testing/conformance/dump_tasks.txt` (`<stream> <frames> [<temporal units>]`), after the
run, and keeps them in the artifact `rav1g-stage-dumps-<run id>`, one directory per stream
with the dumps, the task's log and its md5 file:

- vectors (`aom`) and streams encoded in the job (`svt`): the `.stages` files. An `svt`
  stream is ffmpeg's test pattern, and what the encoder makes of it depends on the encoder's
  version, so the stream itself is in the artifact too (`streams/`): put that file on the
  other device to compare the same frames.
- cuts of published streams (`cut`): `.digest` files only, a hash per row and a CRC per 8x8
  block of every plane of every stage. A digest has no samples; neither the streams nor
  their decoded frames leave the job. `stage_dump.py compare` takes a digest on either side
  and then reports which rows and 8x8 blocks differ, not by how much. The same cut can be
  made anywhere with the line of `streams.txt` (`ffmpeg -ss <start> -i <url> -map 0:v:0
  -frames:v <frames> -c copy -f ivf`).

To get dumps of other frames, edit `dump_tasks.txt` and push; the job prints what it dumped
(`stage_dump.py list`) in the step's log.

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

`.github/workflows/build.yml` runs it on every push, in two jobs.

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
  sizes than the frames they predict from) and super-resolution (`superres-mode=2`: all
  denominators). The script checks that the encoder produced the feature (frame headers with
  `use_superres`, more than one coded width). Reference from dav1d.

The streams are media. They are never committed and never uploaded as an artifact: the job
keeps them in the Actions cache, keyed by the list and the scripts, and the Windows job
restores them from there. The job's artifact `rav1g-conformance-md5-<run id>` has the md5
references and the task list only.

**`conformance`** (Windows) builds `warp_decoder` and runs `testing/conformance/run.py`: the
vectors in one decoder process, in the order of the list, with `-t 4`, and every large stream
(the cuts and the streams encoded in the job) in a process of its own (see
[Results](#results) for why). A task that ends in a
decode error, a timeout or a crash ends its process (a failed decode may leave the device in
a state the next stream should not inherit), and the run goes on with the next stream in a
new process. A stream that fails after other streams in the same process is decoded again
alone and must fail the same way; otherwise its result is `unstable` (with one exception, see
[Results](#results): a run that lost the D3D12 device is repeated). Every task has a
timeout (300 s for a vector, 2400 s for the larger streams), so a hang ends that stream, not
the job.

The result is a table in the job summary, one row per stream: size, bit depth, frames,
matching frames, first bad frame, decode time, result. The artifact
`rav1g-conformance-<run id>` has `results.tsv`, `summary.md`, the decoder's output for every
stream (`logs/<stream>.log`) and the md5 of every output frame (`md5/<stream>.md5`, in the
player's format).

### Results

On WARP the shaders run as D3D12 specifies them and not as the console's GPU happens to run
them, which is what the run is for. Upstream's shaders had 0 of 250 streams right there, with
another md5 on every decode: they counted on the 64 threads of a thread group running in lockstep
([gpu-av1-decoder.md](gpu-av1-decoder.md#lockstep-reliance-portability-2026-11-11)). With group
syncs where threads hand data to each other, the super-resolution fix
([test-player.md](test-player.md#super-resolution)) and the three fixes below, all 251 streams
are md5-identical to their reference:

| streams | pass |
| --- | --- |
| quantizer 0..63, 8-bit (`av1-1-b8-00-*`) | 64 of 64 |
| quantizer 0..63, 10-bit (`av1-1-b10-00-*`) | 64 of 64 |
| frame sizes 16x16 to 226x226 (`av1-1-b8-01-*`) | 100 of 100 |
| all intra, CDF update, motion vectors, motion field (`-02`, `-04`, `-05`, `-06`), intra-only frames (`-16`) | 5 of 5 |
| size changes at key frames (`av1-1-b8-03-*`) | 2 of 2 |
| temporal and spatial layers (`av1-1-b8-22-svc-*`) | 5 of 5 |
| film grain, monochrome, 8 and 10 bit (`-23`, `-24`) | 4 of 4 |
| Netflix Open Content cuts, 270p to 2160p, 8 and 10 bit | 5 of 5 |
| SVT-AV1 reference scaling | 1 of 1 |
| SVT-AV1 super-resolution | 1 of 1 |

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

It also shows without a lost device: in run 43134793241 the super-resolution stream, the 247th
of its process, came out wrong from its first frame on (64 frames, no error), the three 8-bit
streams behind it were right, the 1080p 10-bit stream after those was wrong from its first frame
on, and only the stream behind that one found the device removed. Both streams are right alone.
So the large streams (the cuts and the streams encoded in the job: a task with a `-timeout` of
20 minutes or more) no longer share a process with what was decoded before them: each gets a
decoder process of its own (`run.py --alone-timeout`), and the vectors share one as before.

Because the stream it hits changes, it cannot be listed in `expected.txt`. `run.py` decodes a
stream whose run lost the device again alone, up to three times, and takes the result of a run
that kept its device; every loss is reported under "D3D12 device removed" in the summary and on a
`D3D12 DEVICE REMOVED` line of the log. The run does not fail on a lost device. It fails, as
before, on a stream that fails with its device, and on one that loses the device three times
alone.

Two streams that are wrong on the console pass here: the 8-bit 4K stream
(`nf-dvbdash-a1-8b-2160p.ivf`, luma off by ±1–2 on the console) and the stream with scaled
references (`syn-resize-10b-1080p.ivf`).

### expected.txt

`testing/conformance/expected.txt` lists the streams that fail today (none at the moment), one
per line:

```text
<stream> <mismatch|error|timeout|crash|unstable> <reason>
```

The job fails when

- a stream fails that is not listed (a regression, or a new stream that finds a bug),
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
python testing\conformance\run.py --tool testing\warp_decoder\bin\x64\Release\warp_decoder.exe ^
  --streams conformance-streams --out conformance-out
```

`--only <regular expression>` runs the streams whose name matches, `--tool-args "-cdefv1"`
adds flags to every task (`-hw` for a hardware adapter), `--no-repeat` skips the repeat of
failing streams. One stream by hand: take its line from `conformance-streams\manifest.txt`,
run `warp_decoder` with those arguments from inside `conformance-streams`, and add `-o` or
`-save_wrong_yuv` for the samples.

### References for streams whose frames change size

`make_md5_ref.sh` passes `-noautoscale` to ffmpeg. Without it ffmpeg scales every frame to
the size of the first frame of the stream, and the md5 of a scaled frame matches no decoder:
for a stream with reference scaling such a reference reports every frame of another size
than the first as wrong, whatever the decoder does. The references made before this change
are only wrong for streams whose frames change size.
