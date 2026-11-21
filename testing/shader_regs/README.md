# Shader registers and occupancy (RDNA2, offline)

How many waves of a shader fit on the GPU at once depends on its registers (VGPRs) and its group memory
(LDS), and occupancy is what hides memory latency, for example in the intra wavefront. There is no
profiler on the console. This check compiles every shader of the decoder offline for an RDNA2 target and
records, per shader, what the compiler gave it. The CI job `shader-regs` holds the numbers against
[`baseline.tsv`](baseline.tsv) and fails when a shader loses occupancy or starts to spill.

**What the numbers are.** This is AMD's desktop Vulkan compiler, not the console's: the console takes
DXIL through its own driver and compiler. The absolute numbers are approximate. What means something is a
change from one commit to the next: a shader edit that adds 40 VGPRs or drops a wave per SIMD here
almost certainly costs registers on the console too.

## How

For each shader of `crates/rav1g-sys/shaders.txt` (`shader_regs.py measure`):

1. DXC compiles the HLSL to SPIR-V (`-spirv -fspv-target-env=vulkan1.1`) with the arguments of the real
   build, taken from `testing/shader_cache/shaders.py`: `-T cs_6_0 -E main -HV 2018 -WX
   -Wno-ambig-lit-shift`, and `-Od` for `intra_main_legacy` and `intra_main_hbd_legacy`. Register
   classes get separate binding ranges (`-fvk-{t,u,b,s}-shift`): DXC would put `t0`, `u0` and `b0` on the
   same binding.
2. LLPC (`utils/amdllpc` of RGA) compiles the SPIR-V for gfx1030 with its automatic descriptor layout,
   and dumps that layout as a pipeline file. The pipeline file gets `options.waveSize = 64` and is
   compiled again: the code object.
3. RGA's binary analysis (`rga -s bin -a`) reads the VGPRs, SGPRs, LDS bytes, scratch and spills of the
   code object. Its PAL metadata gives the wave size and the thread group, and has to agree with RGA on
   VGPRs, LDS and scratch.
4. The theoretical waves per SIMD follow from these (below).

`measure` takes about 45 s for the 128 shaders with 4 jobs.

**Wave size: wave64**, as the console runs. One exception: LLPC makes wave32 code for a thread group of
at most 32 threads, whatever `waveSize`, `subgroupSize` or `--native-wave-size` say. These shaders
(today `intra_filter`, `intra_filter_hbd`, `intra_filter_prefetch`, `intra_filter_prefetch_hbd`, groups
of 4x8) have `32` in the column `wave`; their occupancy is computed as one wave64 per group, as on the
console, from the register count of the wave32 code.

**Target: gfx1030** (Navi 21), the nearest public target to the console's GPU (RDNA2).

## Occupancy model

The model of LLVM's AMDGPU backend for gfx1030 in wave64 (`AMDGPUBaseInfo.cpp`), per SIMD:

- VGPRs: 512 per lane, allocated in granules of 8: `floor(512 / align8(VGPRs))` waves.
- At most 16 waves per SIMD.
- SGPRs do not limit occupancy on gfx10 (each wave gets its own 106).
- LDS: 64 KiB per compute unit of 2 SIMDs, allocated in granules of 512 bytes. LLPC compiles in CU mode
  (`.wgp_mode 0`); WGP mode has 128 KiB for 4 SIMDs, the same per SIMD.
- Waves come in whole thread groups: the groups that fit on a compute unit are the smallest of the
  three limits, and the waves per SIMD are `groups x waves per group / 2` (a group of 64 threads with
  room for 5 groups per compute unit gives 2.5).

The column `limit` names what binds: `vgprs`, `lds`, or `waves` (the hardware's 16). Not modelled: the
hardware's limit on thread groups per compute unit, barriers, and scratch (a shader with scratch spills
to memory, which costs far more than a lost wave).

## Columns of the TSV

`shader threads wave vgprs sgprs lds_bytes scratch_bytes waves_per_simd limit`

`scratch_bytes` is the scratch memory per lane LLPC reserves (spills or a stack). Today one shader has
scratch: `intra_main_hbd_legacy`, 48 bytes, the `-Od` comparison kernel of `-intraod`.

## The baseline

`shader_regs.py compare` holds the measured TSV against `baseline.tsv`:

- **Fails** when a shader has fewer `waves_per_simd` than its baseline row, when it has more
  `scratch_bytes`, when a new shader (no baseline row) has scratch, or when a shader does not compile
  and is not in [`known_failures.txt`](known_failures.txt).
- **Notes**, without failing: other changes (VGPRs up or down within the same occupancy, more waves),
  new shaders without scratch, baseline rows of shaders that are gone, and known failures that compile
  again.

A drop in occupancy or new scratch can be right (a faster kernel that needs more registers): then the
same commit updates the baseline, and its commit message says why. To update it, copy the measured TSV:
from the artifact `rav1g-shader-regs-<run id>` of the job (`shader_regs.tsv`), or from a local run:

```sh
python3 testing/shader_regs/shader_regs.py measure --out testing/shader_regs/baseline.tsv --failures /dev/null
```

Improvements are not required in the baseline, but recording them keeps a later regression from hiding
behind the old number.

The job writes the step summary (lowest occupancy first, with the change against the baseline) and the
artifact `rav1g-shader-regs-<run id>`: `shader_regs.tsv`, `failures.tsv`, `summary.md`, `compare.txt`.

## Known failures

[`known_failures.txt`](known_failures.txt) lists shaders that DXC cannot turn into SPIR-V or LLPC cannot
compile, each with the reason; the job lists them instead of failing. It is empty: every shader compiles.
An entry has to say why it is there, and it is taken out when the shader compiles again (the job says so).

## The tools

Pinned by version and SHA-256 in `shader_regs.py` (`TOOLS`), downloaded and checked by
`shader_regs.py fetch` (CI caches the archives, and the hash is checked on every run):

- **RGA 2.13** (Radeon GPU Analyzer, GPUOpen, MIT licence), `rga-linux-2.13.tgz`. Of the archive only
  `rga`, `rga-bin`, `utils/amdllpc` and the disassembler are unpacked; not the bundled Vulkan driver
  (`utils/vulkan`, which comes under AMD's driver EULA) and not the GUI. 2.13 is the newest release whose
  LLPC still takes gfx10 targets: the LLPC of 2.14.1 and 2.15 answers `Invalid gfxip: gfx1030`
  (2.14.2 has no Linux build).
- **DXC v1.9.2602**, the Linux release of the version line of the real build (Windows SDK 10.0.26100.0,
  DXC 1.9.2602).

## Locally

```sh
python3 testing/shader_regs/shader_regs.py fetch          # once: about 270 MB, into ~/.cache/rav1g/shader_regs
python3 testing/shader_regs/shader_regs.py measure --out regs.tsv --failures failures.tsv
python3 testing/shader_regs/shader_regs.py compare --failures failures.tsv regs.tsv
```

`--tools DIR` or `RAV1G_SHADER_REGS_TOOLS` put the tools elsewhere; `measure --only a,b` measures some
shaders; `TMPDIR` is where the work files go. `testing/precheck.sh` runs the check when a shader changes
and the tools are there. Linux x86-64; RGA's notes say its Vulkan offline mode needs Ubuntu 24.04 or
newer (or a distribution as recent).
