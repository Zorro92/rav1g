# Checks of the testing directory

## `precheck.sh`: before every push

A CI run takes about 25 minutes. Many of its checks run natively on Linux in seconds or minutes: the
`testing/*/run.sh` scripts, which compile shaders as C++ and hold them against their legacy forms or
libaom. `testing/precheck.sh` runs the ones a change touches, with the flags CI gives them, so that a
failure shows up before the push and not a CI cycle later.

```sh
sh testing/precheck.sh                # the checks for what changed against origin/main
sh testing/precheck.sh --base HEAD~3  # against another base
sh testing/precheck.sh --all          # every check
sh testing/precheck.sh --list         # the mapping, and what would run
```

What changed: the commits since the merge base with the base, staged and unstaged changes, and
untracked files. The mapping from paths to checks is one table at the top of the script; a changed file
that no check is mapped to and that is not on its list of files no check reads (docs, `.github`, CI's
Windows-only scripts) starts every check. `--check-mapping` (the CI job `precheck-mapping`) fails when a
`testing/*/run.sh` is missing from the mapping: a new check gets a line there.

What runs, at most 4 at a time (`-j N` for fewer), heaviest first:

- the `run.sh` checks, with CI's flags (`intra_check --mutate --layouts 400`, `--mutate` for the
  others that CI runs that way). `copy_check`, `idct_check`, `loopfilter_check` and
  `residual_clear_check` are not in CI; here they run with `--mutate`.
- the timing summary self-test of the job `intra-check` (`perf_stats`),
- `cargo fmt --check` of the first-party packages (`third_party/rav1d` keeps its upstream's format),
- `xwin`: when the local image `localhost/rav1g-xwin` exists (podman, cargo-xwin, clang-cl), the
  Windows build of the job `rust-cross` (`cargo xwin build --release --locked --target
  x86_64-pc-windows-msvc`) and clippy for the same target. A clippy warning in a first-party file the
  change touches fails the check (`-D warnings` for the change); warnings in files it does not touch are
  listed, and `third_party/rav1d` is left out (its lint set is its upstream's). The library embeds the
  shader bytecode: `RAV1G_SHADER_DIR` if set, else the shaders are built by
  `testing/shader_cache/shaders.py` with the Linux DXC of `shader_regs` (cached by content in
  `~/.cache/rav1g/precheck`). Without the image or the bytecode the check says so in one line and is
  skipped.
- `shader_regs`: the register and occupancy check of the job `shader-regs`
  ([`shader_regs/README.md`](shader_regs/README.md)), when its pinned tools have been fetched once
  (`python3 testing/shader_regs/shader_regs.py fetch`); else skipped with one line.

At the end a table of check, result (`ok`, `FAIL`, `skip`) and seconds, the end of the log of each
failed check, and exit code 1 when one failed.

**Disk.** The checks build in directories made by `mktemp -d`, under `TMPDIR` (else `/tmp`), and so do
the logs. `/tmp` may be a small tmpfs: point `TMPDIR` at a directory on a disk with a few GB free.

**Tools.** POSIX sh, git, gcc and g++, python3, cargo (for `tables_check`, `residual_clear_check` and
fmt); podman for `xwin`. No GPU.
