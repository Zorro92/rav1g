# Argon Streams AV1 on WARP

This harness checks the [Argon Streams AV1 v2.1.1 release](https://aom-cwg-av1-argon-streams-public.s3.us-east-1.amazonaws.com/argon_coveragetool_av1_base_and_extended_profiles_v2.1.1.zip).
The vectors cover AV1 v1.0.0-errata1, with reference output from libaom
`v1.0.0-errata1`. The six Profile 0 folders use about 1.47 GB of file
contents. Keep the extracted data outside the repository.

See the [Profile 0 run results](RESULTS.md) for coverage, exclusions, and
diagnostics for the first differing frame.

```powershell
python testing/argon/fetch.py --out ../argon-v2.1.1
python testing/argon/run.py --tool target/release/rav1g-dec.exe --streams ../argon-v2.1.1 --out ../argon-results
```

`fetch.py` reads the ZIP directory and the byte ranges for the six selected
folders. It checks the S3 object's published release size and pinned multipart
ETag, validates each HTTP range, and checks every extracted member's ZIP CRC32.
S3's multipart ETag is different from the whole-archive MD5. If you already have
the full ZIP, pass `--zip <archive>` to verify its published MD5 before extraction.

## Selecting tests

- By default the runner uses each folder's `all_list.txt`. Pass `--folder`
  one or more times to narrow the run. With one folder, `--list <file>`
  accepts that folder's bare `.obu` names, including its `level5.x_list.txt`.
  A list spanning folders uses `profile0_core/testN.obu` or
  `profile0_core/streams/testN.obu`, one stream per line.
- The default is operating point zero, which is the operating point the
  suite uses for coverage. To check an alternate point for selected
  streams, pass `--operating-point N`. The runner then uses
  `layers/N/<stream>_layerN` in the reference directories. Select streams
  with the matching reference files. The stress and error folders provide
  only point zero.
- `--grain off` compares with `md5_no_film_grain`; the default compares
  with `md5_ref` and synthesizes grain when signaled. The runner reads
  each `ref_cmd` as text to determine framing, all-layers output and
  the reference operating point. It never runs the shell script.
  The v2.1.1 error commands omit `--annexb`, although all 114 streams
  have Annex B framing. The runner explicitly selects Annex B for that
  folder so an input-format probe cannot stand in for error testing.
- The Windows executable defaults to WARP. `--hw --launcher wine` can
  use Wine's D3D12 implementation when configured locally. Extra
  diagnostic flags such as `--decoder-arg=-poisonbuf` can be passed one
  argument at a time.

Each test gets its own process and watchdog (`--timeout`). For valid
streams, `-check_stream` compares one incremental MD5 of the concatenated
visible frames in presentation order. Each frame contributes Y, then U,
then V, in row order, with one byte per 8-bit sample and two little-endian
bytes per 10-bit sample. Monochrome contributes only Y. Resolution, bit depth
and plane count can change midstream. The decoder prints a `STREAM_MD5`
line with the expected and actual digest. The stream digest cannot locate
a differing frame. To investigate a mismatch, generate frame references
with the pinned libaom and use `-check` and `-save_wrong_yuv` or
`-dumpstages` on that case.

`profile0_error` is a negative test set with no MD5 references. A pass
requires exit code 2, a decoder `BITSTREAM_ERROR` marker, and a terminal
`RESULT status=error`. Successful decode, timeouts, crashes, device loss
and other errors fail. The runner writes `results.tsv`, a per-folder
`summary.md`, and decoder logs for failed cases under `logs/`. Any failed
row makes the run exit 1. Setup errors exit 2.

## Exclusions and CI

[Exclusions](exclusions.tsv) list the specific streams whose sequence
headers declare a maximum width above 4096 or height above 2176. These
are the dimensions rav1g-dec uses to cap its allocation, independent of
level signaling. Some level 5.x and low-overhead streams exceed one
dimension. Monochrome, film grain and supported-size scalability cases
stay in the run. The two large-scale-tile folders require separate packing
and tile-list decoding and are outside the six downloaded folders.

Select `argon` on a manual **Build** workflow run to test on WARP. An
Ubuntu job populates one cross-OS Actions cache. Eight Windows shards restore
it, use the normal `rav1g-dec` build, and upload their result tables even if
checks fail. Each shard runs the default decoder with `-parseframes 2`.
To test a decoder switch in the same build, dispatch with:

```sh
gh workflow run build.yml --ref <branch> -f argon=true -f argon_switches=-newswitch
```

`argon_switches` accepts comma-separated passes, such as
`-newswitch,-poisonbuf`. A pass is one flag or several, each with an
optional plain value, separated by spaces: `-output p010` decodes into NV12
and P010 textures the output pass writes (the hash is that of the planes, so
the reference md5 holds), which is the only way Argon reaches the direct
semi-planar output and its fusion into the last postfilter stage. Each
distinct pass runs with `-parseframes 2`. Shard artifacts contain
`results.tsv` and `summary.md` in `default/` and in a directory named for
each pass (spaces become `_`).

No Argon job runs on pushes, tags, or the weekly schedule. The cache is
approximately 1.5 GB. Plan on roughly 15–35 minutes for the default Windows
matrix after a cache hit and allow another 10–15 minutes for a cold
fetch and cache save. This estimate uses the suite's frame sizes and
existing WARP timings. The Profile 0 range fetch transferred 1.59 GB
in about 13 minutes in local testing, including the ZIP directory.

For future console tests, the player's shared reader accepts `-annexb`
in its task lines. The aggregate checksum and runner described here
use the desktop executable.
