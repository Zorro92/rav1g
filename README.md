# rav1g

A fast, open-source AV1 decoder on the GPU: entropy decoding on the CPU, everything after
it (inverse transforms, intra and inter prediction, loop filter, CDEF, loop restoration,
super-resolution, film grain) in D3D12 compute shaders. It runs on any D3D12 GPU. The name
stands for Rust AV1 GPU; a Rust host is the plan once the shaders are fast enough.

rav1g is a fork of AOMedia's [av1-xbox-one](https://aomedia.googlesource.com/av1-xbox-one)
(commit `b5c61563b48625c261a8d627cfa928896fd10b13`, August 2021), with upstream's history
kept. Upstream's own description is in `README.pdf`.

## Why

Heavy 4K AV1 scenes stutter with dav1d on an Xbox Series console (~6–7 CPU cores busy), and
the console has no AV1 hardware decoding. They also choke with av1-xbox-one as shipped. A
faster GPU decoder is the remaining route to smooth heavy 4K AV1 there, and it would leave
most of the CPU free.

## Status

Experimental. The decoder is upstream's, plus timing instrumentation, a rewritten CDEF
shader and a smaller memory footprint. Tested on Xbox Series consoles in UWP app mode with
the test player (`testing/uwp_dx12_player`).

Defaults (test player and library): the rewritten CDEF shader, a decoder sized for the
stream, one back buffer shared by all frames, two frames in flight. Each was md5-exact on
the console; the upstream behaviour is `-cdefv1 -nofitsize -lowmemparts 0` in the test
player. Everything else that was tried (`-slots 3`, `-lowmem`, `-asyncclear`, `-intrav2`,
`-intraopt`) stays behind its flag. The columns below name the flags the runs were made
with; `-cdefv2`, `-fitsize` and the shared back buffer of `-lowmem` are the defaults now.

4K 10-bit, 1 tile, 40 Mbps, 300 frames, on the console, no presentation (ranges are over
separate runs). 24 fps needs at most 41.7 ms per frame:

| | Upstream | `-cdefv2` | `-cdefv2 -lowmem -fitsize -slots 3` |
| --- | --- | --- | --- |
| CDEF per frame | ~52 ms | 5.5–7.2 ms, md5-exact | same |
| SVT-AV1 low-delay clip | 13.4 fps, GPU busy 66.5 ms | 21.0–23.1 fps, GPU busy 30.7–32.2 ms | 25.3–25.6 fps (27.3 with `-asyncclear`, one run) |
| Jellyfin test clip | 10.5 fps, GPU busy 95.4 ms | 16.4–16.8 fps, GPU busy 50 ms (intra 21, inter 19) | 18.6–18.7 fps (19.2 with `-asyncclear`), GPU-bound at ~51 ms |

- **CDEF v2** is 7–9x faster than upstream and bit-exact. It is the default (`-cdefv1`
  selects upstream's shader).
- **Intra v2** (`-intrav2`) is bit-exact but 2.3–2.4x slower. It is a negative result, off by
  default and kept for reference.
- **Intra optimisation A/B** (`-intraopt`): upstream builds its `intra_main` shader with FXC
  optimisation disabled. With optimisation on it is no faster and decodes wrongly (a
  timing-dependent race), which is probably why upstream turned it off. Negative result.
- **A third frame in flight** (`-slots 3`) lets CPU and GPU overlap: the decode thread's wait
  for a free slot drops from 5.6–8.3 ms to 0.3–0.6 ms per frame. It only fits in memory with
  `-lowmem`. All combinations are md5-exact. On the SVT clip the CPU (entropy decoding,
  ~34 ms on one core) is now the limit; on the Jellyfin clip the GPU is.
- **Memory**: a console app may use 1 GiB. At 4K the decoder took 882.7 MiB of it (app memory
  after creation, 2 slots), then 812.7 MiB, and 765.5 MiB sized for the stream (measured).
  The default now also shares one back buffer, 24 MiB less (computed: 615.7 MiB of D3D12
  buffers for a 4K 10-bit stream). With `-lowmem` it is 663.4 MiB and with
  `-lowmem -slots 3` 773.5 MiB (measured). A decoder that
  does not fit is refused instead of losing the D3D12 device, and a destroyed decoder returns
  its memory (in md5 runs about 75 MiB stays behind; under investigation).
- 1080p24/30 already decodes in real time.
- **Hidden frames**: encodes with frames that are decoded but shown later (SVT-AV1 random
  access, and most real-world AV1) used to stop after the first frames, because every hidden
  frame held an output buffer. An output buffer is now taken only for a frame that is output,
  when it is output. Verified on the console: an SVT-AV1 random-access 4K clip decodes
  md5-identical to dav1d (27.9 fps at 12.6 Mbps with 2 slots, 29.4 with 3).
- **Global motion**: frames with global motion were wrong on the console in upstream (a
  shader computed block positions in double precision, which the console does not execute).
  Fixed with integer arithmetic and checked on a host against libaom
  (`testing/warp_check`). Verified on the console: 1200 frames of a 1080p stream with global
  motion, and 1080p and 4K film-grain streams with global motion and show-existing frames,
  are md5-identical to dav1d.
- **Known bug, 8-bit streams**: an 8-bit 4K stream decodes with luma off by ±1–2 on about 0.4%
  of the pixels from the first (key) frame on; chroma is exact. Upstream behaviour (the same
  with every new option switched off). 10-bit streams are not affected.

Next targets: a conformance run on WARP in CI (AOM test vectors, dav1d references), the 8-bit
bug, entropy decoding on the CPU (frame-parallel or a faster
parser), and inter and intra prediction on the GPU. The Rust port starts once 4K GPU and CPU time per frame are well
under 41.7 ms on heavy content.

## Documentation

- [docs/test-player.md](docs/test-player.md): building, installing and running the test
  player, its defaults and flags, log lines, md5 checks (references can be made with dav1d:
  `testing/md5ref/make_md5_ref.sh`) and the host-side shader checks.
- [docs/gpu-av1-decoder.md](docs/gpu-av1-decoder.md): history, measurements and reasoning.

## Licence

BSD-2-Clause (`LICENSE.txt`, `libav1/LICENSE`) plus the Alliance for Open Media Patent
License 1.0 (`libav1/PATENTS`). `third_party/libwebm` carries its own licence and patent
notices (`third_party/libwebm/LICENSE.TXT`, `third_party/libwebm/PATENTS.TXT`). Keep all of
them when redistributing source or binaries; CI artifacts include them under `licenses/`.

## Disclaimers

Not affiliated with or endorsed by Microsoft. Xbox is a trademark of Microsoft Corporation.

Independent project, not affiliated with rav1e or rav1d.

Not affiliated with or endorsed by the Alliance for Open Media.
