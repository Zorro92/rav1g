# rav1d in rav1g: what is here and what was changed

This directory is a copy of [rav1d](https://github.com/memorysafety/rav1d), the Rust port of dav1d, at
the commit `UPSTREAM` names (`2e7216c`, version 1.1.1 in its `Cargo.toml`, 212 commits behind the tag
`v1.1.0`). rav1g uses its **parse pass**: the code that reads an AV1 stream down to blocks, motion
vectors and coefficients. The pixels are made by rav1g's compute shaders. The design, and the
measurements that led to it, are in `docs/phase2-rav1d.md` of the repository.

Licence: BSD 2-Clause, `COPYING` (VideoLAN, the dav1d authors and the Internet Security Research
Group); `doc/PATENTS` is the patent licence of the Alliance for Open Media that upstream carries;
`THANKS.md` and `README.md` are upstream's. The packages of rav1g that contain this code carry
`COPYING`.

## The rule

Changes only take away, or route what the parse pass reads to the application. Nothing here changes how
a syntax element is read. An update to a newer upstream is then: copy the files again, apply the list
below again, run the two checks at the end.

`upstream_diff.sh <checkout of rav1d at the commit>` holds the directory against upstream: every file
is upstream's byte for byte, or one of the twelve changed files below, or one of the four files of
rav1g (`PATCHES.md`, `UPSTREAM`, `upstream_diff.sh`, `src/parse.rs`). It writes the diff of the changed
files (+549 −4852 lines at this commit) for a review. CI runs it (job `rav1d`).

Size: 67 files, 1.4 MB, 39,100 lines here; upstream has 1,364 files (296 under `src`, 329,000 lines
there).

## Not copied

- Everything outside `src` except `Cargo.toml`, `build.rs`, `rustfmt.toml` and the notices: the tests
  and their data, the tools (`dav1d` command line), the examples, the C headers, CI, meson files.
- `src/arm`, `src/riscv`, `src/loongarch`, `src/ppc` and all of `src/x86` except `msac.asm` and
  `refmvs.asm` (and `src/ext/x86/x86inc.asm`, which both include): the assembly of the pixel code.
  `src/x86/cpuid.asm` is not needed either: the CPU features come from the `raw-cpuid` crate.
- The pixel code: `cdef.rs`, `cdef_apply.rs`, `fg_apply.rs`, `filmgrain.rs`, `ipred.rs`,
  `ipred_prepare.rs`, `itx.rs`, `itx_1d.rs`, `lf_apply.rs`, `loopfilter.rs`, `looprestoration.rs`,
  `lr_apply.rs`, `mc.rs`, `pal.rs`, `wedge.rs`, `pic_or_buf.rs`.
- `rust_api.rs`, the safe wrapper around the C API.

## Changed files

| File | Change |
| --- | --- |
| `Cargo.toml` | The package as a library of this workspace: `rlib` only, not published, no profiles and no workspace of its own; the features are `asm`, `bitdepth_8`, `bitdepth_16`. The dependencies are upstream's. |
| `build.rs` | Assembles the two files above for x86-64 and nothing else. The symbols get the prefix `rav1g_dav1d` instead of `dav1d` (`private_prefix` of `config.asm`), so that an application can link dav1d or rav1d next to this copy. |
| `src/msac.rs`, `src/refmvs.rs` | The names of the assembly functions with that prefix. |
| `src/cpu.rs` | Without `dav1d_set_cpu_flags_mask` (C API) and without the number of processors (see `lib.rs`). |
| `src/tables.rs` | Without the nine `#[no_mangle] dav1d_*` tables of the pixel assembly. |
| `src/lib.rs` | Without the C API (the 19 `dav1d_*` functions) and the modules above; no film grain. `rav1d_open` takes the sink. The number of threads and of frames in parse is what the application says (0 is 1): upstream takes the number of processors and its square root, which on Windows are two imports the library does not have otherwise. A crate-level `allow` for the code the removals leave unused, so that the files stay close to upstream. `pub mod parse`, `pub mod levels`. |
| `src/internal.rs` | Without the pixel DSP tables, the film grain task data and the buffers of the reconstruction pass (blocks, coefficients, palette indexes and colours per frame; CDEF and loop restoration line buffers). The context has the sink; a frame has its number at the sink. |
| `src/decode.rs` | The parse pass only. A block is handed to the sink when its mode info is read, and its coefficients are read right behind it (upstream stores the block and reads the coefficients in the same pass; the second pass reconstructs). The palette index map goes to the sink as it is read (upstream packs it for the second pass). The sink is told when a frame, a tile and a superblock begin and end; the loop restoration units of a superblock are collected for it. No reconstruction, no bookkeeping of the lowest pixel a block predicts from, no pixel buffers. The reference handling, the CDF updates and the motion vector storage are upstream's. |
| `src/recon.rs` | Reading of the coefficients and of the palette colours only (1,990 of 4,190 lines). `decode_coefs` asks the sink where the coefficients of a transform block go and writes them there: dequantised, 32 bits each, in scan order (upstream writes them at their position in the block, clamped to the range of the bit depth; the inverse scan tables for that are built once). The values and the context are computed as upstream computes them. |
| `src/thread_task.rs` | Without the film grain tasks; the filter tasks do nothing. The scheduler, its task order and its progress signals are upstream's: the reconstruction task of a tile returns at once. |
| `src/picture.rs` | A picture has no samples: each plane is the smallest piece of memory a plane can be. Pictures remain the handles of the reference bookkeeping. |

## Added

- `src/parse.rs`: the interface to the application. `ParseSink` is called per frame, tile, block, coded
  transform block, palette plane and superblock; `Parser` takes the temporal units of a stream.
- `PATCHES.md`, `UPSTREAM`, `upstream_diff.sh`.

## Checks

- `upstream_diff.sh`, above.
- Upstream itself at this commit, unchanged, decodes the conformance streams of rav1g to their
  reference md5 on Linux (CI job `rav1d`): the base the changes start from is right.
- The changed parser against the entropy decoder of libaom on the same streams, every block, transform
  block and coefficient (`tools/rav1g-parse-check`, CI job `parse-check`).
