# Argon AV1 Profile 0 results

## Current results (2026-11-20, main at 0a841b4)

WARP, CI run 44708485158 (`gh workflow run build.yml -f argon=true`), operating point 0, film grain
where signaled:

| Folder | Bit-exact | Invalid, decoded | Invalid, rejected | Excluded (above 4096x2176) | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `profile0_core` | 621 | | | 143 | 764 |
| `profile0_core_special` | 46 | | | 9 | 55 |
| `profile0_not_annexb` | 8 | | | 7 | 15 |
| `profile0_not_annexb_special` | 46 | | | 9 | 55 |
| `profile0_stress` | 44 | | | 44 | 88 |
| `profile0_error` | | 78 | 36 | | 114 |
| **Total** | **765** | **78** | **36** | **212** | **1091** |

No mismatch, decode error, crash, timeout or device loss. Every Argon job of the run passed.

What took it from 188 bit-exact (the first WARP run, below) to 765, in order:

| Commits | Cause | Streams fixed |
| --- | --- | ---: |
| a230001, 58414fd | Film grain: chroma bounds and the luma pair of odd sizes; the planar apply pass dispatched for three blocks per group (7.18.3.5) | 27 |
| a28d5be | Restoration records sized for 64x64 chroma units; with lr_uv_shift a 4:2:0 chroma plane has as many units as luma (5.9.20) | 24 |
| 7e5b2bb | Palette blocks without a residual got no reconstruction step (7.11.4) | 469 |
| 72e87c3 | 10-bit intra block copy read its second sample from the wrong bits (7.11.3.4) | 13 |
| 87440cc | Output staging of narrow pictures overwrote a reference picture (7.20, 7.21) | 10 |
| 900c06b | Intra block copy blocks waited for every earlier block and ran past the wavefront's step limit (6.10.25) | 4 |
| 45d8f88 | Overlapped prediction from a 128-sample neighbour as one block instead of two 64-sample steps (7.11.3.9) | 7 |
| f8d161f, 0a841b4 | Mode info pool too small for the densest frames; the pools now grow once, to the worst case, when a frame needs it (5.9.15, 5.11) | 20 |

The counts add up to 574; the other 3 are streams that failed only in some runs (crashes, device losses
or timeouts on a slow runner), which no longer fail. The bugs were in the code rav1g
was forked from; branches `original` and `cpp` have the same ones.

## First run (2026-11-19)

Argon v2.1.1 covers AV1 v1.0.0-errata1. This run used operating point 0,
the framing and all-layer settings from the release's reference commands,
and film grain where signaled. The decoder was the existing desktop
`rav1g-dec` build for `exp/argon` at `ece0ab3`, running on the Wine D3D12
hardware path. The primary counts combine the saved per-stream runs and
the local batch runner. Together they cover all 977 release-list entries,
including the 753 entries queued when the first run stopped.

| Folder | Pass | Differs | Decode error | Watchdog | No `RESULT` | Excluded | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `profile0_core` | 128 | 479 | 0 | 0 | 14 | 143 | 764 |
| `profile0_core_special` | 16 | 23 | 2 | 0 | 5 | 9 | 55 |
| `profile0_not_annexb` | 5 | 3 | 0 | 0 | 0 | 7 | 15 |
| `profile0_not_annexb_special` | 22 | 21 | 2 | 0 | 1 | 9 | 55 |
| `profile0_stress` | 16 | 7 | 16 | 1 | 4 | 44 | 88 |
| **Total** | **187** | **533** | **20** | **1** | **24** | **212** | **977** |

Pass and Differs describe completed decodes with matching and differing
stream checksums, respectively. All 20 tile-parse errors reproduced with
one process per stream. `profile0_stress/test80571.obu` returned a timeout
`RESULT`. The 45 streams covered by both runners agreed on outcome and
frame count, with different actual checksums in 14 of the mismatch pairs.
`profile0_stress/test26500.obu` is a primary mismatch that passed both a
per-frame rerun and a standalone whole-stream rerun.

The sequence-header allocation ceiling is 4096×2176. Of the 212 exclusions,
41 exceed the width alone, 75 the height alone, and 96 both limits across
their sequence headers. The table covers the five valid-stream folders listed
above. The negative tests and both large-scale tile folders are outside that
run. The large-scale tile folders require tile-list input and separate
packing. [Full exclusion list](exclusions.tsv)

### Confirmed on WARP

The CI job (`workflow_dispatch` with `argon`, eight WARP shards, 2026-11-19) gave the same picture on
Microsoft's D3D12 software adapter: 188 bit-exact, 531 mismatches, 23 crashes, 20 decode errors, three
device losses and the same 212 exclusions. Stream by stream, 966 of the 977 outcomes agree with the
Wine run. The others move between crash, mismatch and device loss, so the faults behind them depend
on timing. The mismatches are decoder bugs, not artefacts of the platform. All of them were already
on `v0.5.0-alpha.1`: they come from the code rav1g was forked from.

In `profile0_error`, 36 invalid streams are rejected and 78 decode. The specification defines no output
for an invalid stream, and libaom decodes most of them too. The runner counts a decoded invalid stream as
a pass (`decoded_invalid`), and only a crash, a hang or a lost device on one as a failure.

### First differing output

Frame numbers below are zero-based in output order. Hidden coded pictures
have their own indices in stage dumps. The per-frame references came from
the pinned libaom `v1.0.0-errata1` build. Each reference reproduced the
release's whole-stream checksum before its frames were compared. The
shown-frame counts in `stream documentation.xlsx` were summed across coded
video sequences and checked against the reference output. All 533 completed
mismatches returned the expected number of frames. Of those, 350 changed
visible dimensions within the stream.

The original 34 cases had the same first differing frame and planes with
the legacy switches and `v0.5.0-alpha.1` build. The group labels
below describe the first difference using the spreadsheet, sequence and frame
headers, and pixel comparisons. When features overlap, the table gives
priority to grain signaling, scaled inter references, super-resolution, then
`show_existing_frame`.

| Signal at first difference | Streams | Representative evidence |
| --- | ---: | --- |
| Grain signaled | 170 | `test8243.obu` frame 0 U/V and `test7701.obu` frame 2 U/V. The buffers before grain match libaom without grain. The first difference is at output with grain, and both streams pass with grain disabled. |
| Scaled inter references | 44 | `test9309.obu` frame 3 Y, coded frame 4. The 86×25 picture uses 51×44 and 45×23 references. `test6004.obu` frame 2 U/V uses 66×21 and 154×19 references for a 146×20 picture. Stage dumps confirm reference scaling. The differences are visible at output. |
| Super-resolution signaled | 28 | `test6471.obu` frame 0 Y, coded at 16×65 and upscaled to 18×65. The first comparable mismatch is after intra reconstruction, before upscaling. |
| Reference shown again | 119 | `profile0_not_annexb/test12207.obu` frame 0 Y/U/V. `show_existing_frame` displays a hidden key frame. All six expected frames are present, and that picture already differs after intra reconstruction. |
| Key or intra-only picture | 133 | `test7294.obu` frame 0 Y and `test1115.obu` frame 0 Y/U/V. Both differ at the intra dump against libaom reconstruction. Every differing luma sample in `test7294.obu` is in a palette-coded block. |
| Other inter picture | 34 | `test8653.obu` frame 5 Y/U/V, coded frame 3. The frame and its references are all 23×1152. The difference is visible at output. |
| Passed on rerun | 1 | All 60 output frames of `profile0_stress/test26500.obu` match on the per-frame rerun. |
| First differing plane unavailable | 4 | These streams had a completed whole-stream mismatch and the expected frame count. The per-frame reruns did not produce a usable plane comparison. |

Per-frame checks located the first difference in 528 streams. That includes
`profile0_stress/test52303.obu`, whose diagnostic run logged the first U/V
difference at frame 1 before its watchdog expired.

### Streams that exited without a result

- `profile0_core` had no result for `test10421.obu`,
  `test10606_10978_10646.obu`, `test10722_10822_10854.obu`,
  `test1381_8678_6115.obu`, `test1511_10427.obu`, `test2616_2089_9701.obu`,
  `test5416.obu`, `test5426.obu`, `test5580_8180_5415.obu`, `test560.obu`,
  `test5614_8028.obu`, `test5750.obu`, `test7374.obu`, `test9885.obu`.
- `profile0_core_special` had no result for `test1.obu`, `test26.obu`,
  `test35.obu`, `test43.obu`, `test50.obu`.
- `profile0_not_annexb_special` had no result for `test37.obu`.
- `profile0_stress` had no result for `test11164.obu`, `test14800.obu`,
  `test67154.obu`, `test98084.obu`.

Diagnostic reruns also exited without a result for
`profile0_core/test192_8154_5982.obu`, `profile0_core_special/test46.obu`,
`profile0_stress/test59060.obu`, `profile0_stress/test61011.obu`, and
`profile0_stress/test84852.obu`.
