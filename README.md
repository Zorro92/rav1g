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

Experimental. The decoder is upstream's, plus timing instrumentation and rewritten shaders
behind player flags; the defaults still run the upstream shaders. Tested on Xbox Series
consoles in UWP app mode with the test player (`testing/uwp_dx12_player`).

4K 10-bit, 1 tile, 40 Mbps, 300 frames, on the console, no presentation (ranges are over
separate runs). 24 fps needs at most 41.7 ms per frame:

| | Upstream | Now (`-cdefv2`) |
| --- | --- | --- |
| CDEF per frame | ~52 ms | 5.5–7.2 ms, md5-exact |
| SVT-AV1 low-delay clip | 13.4 fps, GPU busy 66.5 ms | 21.3–23.1 fps, GPU busy 30.7–32.2 ms |
| Jellyfin test clip | 10.5 fps, GPU busy 95.4 ms | 16.5–16.8 fps, GPU busy 50 ms (intra 21, inter 19) |

- **CDEF v2** (`-cdefv2`) is 7–9x faster than upstream and bit-exact.
- **Intra v2** (`-intrav2`) is bit-exact but 2.3–2.4x slower. It is a negative result, off by
  default and kept for reference.
- **Intra optimisation A/B** (`-intraopt`): upstream builds its `intra_main` shader with FXC
  optimisation disabled. With optimisation on it is no faster and decodes wrongly (a
  timing-dependent race), which is probably why upstream turned it off. Negative result.
- On both clips the CPU decode thread is now the wall limit: single-tile entropy decoding
  takes 34–44 ms per frame on one core, plus ~12–15 ms of setup and recording.
- **Pipeline depth and clears** (`-slots`, `-asyncclear`): the number of frames in flight
  between CPU and GPU can be raised from 2 to 3 or 4, and the per-frame buffer clears can
  move off the decode thread. Both are off by default and are being measured.
- 1080p24/30 already decodes in real time.
- Known bug: some encodes with hidden frames (SVT-AV1 random access) deadlock on output
  buffer starvation.

Next targets: intra prediction and inter prediction on the GPU, entropy decoding on the CPU,
and the hidden-frame hang. The Rust port starts once 4K GPU and CPU time per frame are well
under 41.7 ms on heavy content.

## Documentation

- [docs/test-player.md](docs/test-player.md): building, installing and running the test
  player, its flags, log lines, md5 checks and the host-side shader checks.
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
