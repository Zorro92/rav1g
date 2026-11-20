#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Run the Argon Streams AV1 Profile 0 tests on rav1g-dec."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import csv
from dataclasses import dataclass
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import time

from suite import FOLDERS

ERROR_FOLDER = "profile0_error"
EXCLUSIONS = Path(__file__).with_name("exclusions.tsv")
LARGE_SCALE_TILE = {"profile0_large_scale_tile", "profile0_large_scale_tile_special"}
STREAM = re.compile(r"test[0-9]+(?:_[0-9]+)*\.obu")
HEADER = ("folder", "stream", "operating_point", "grain", "result", "outcome", "frames", "seconds", "detail")
# DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_RESET.
DEVICE_LOSS = re.compile(r"device (?:was |has been )?removed|DXGI_ERROR_DEVICE_(?:REMOVED|HUNG|RESET)|0x887a000[567]",
                         re.IGNORECASE)
# These flags belong to the harness. Allow other decoder switches for reproducing failures.
RESERVED_FLAGS = {
    "-i", "-name", "-list", "-annexb", "-alllayers", "-oppoint", "-aom_md5", "-check", "-check_stream",
    "-md5_frame_check", "-skip-film-grain", "-limit", "-continue_on_error", "-timeout", "-hw",
    "-parsedigest", "-playersmoke", "-sm6test",
}


@dataclass(frozen=True)
class Stream:
    folder: str
    name: str

    def input(self, root):
        return root / self.folder / "streams" / self.name


def stream_list(path, folders):
    entries = []
    for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = line.partition("#")[0].strip()
        if not line:
            continue
        parts = PurePosixPath(line).parts
        if len(parts) == 1 and len(folders) == 1:
            folder, name = folders[0], parts[0]
        elif len(parts) == 2 or (len(parts) == 3 and parts[1] == "streams"):
            folder, name = parts[0], parts[-1]
        else:
            raise ValueError(f"{path}:{n}: use folder/testN.obu, or testN.obu with one --folder")
        if folder not in folders or not STREAM.fullmatch(name):
            raise ValueError(f"{path}:{n}: invalid or unselected stream {line!r}")
        entries.append(Stream(folder, name))
    if len(set(entries)) != len(entries):
        raise ValueError(f"{path}: duplicate streams")
    return entries


def load_exclusions():
    entries = {}
    with EXCLUSIONS.open(encoding="utf-8", newline="") as source:
        for folder, name, reason in csv.reader(source, delimiter="\t"):
            valid_scope = folder in LARGE_SCALE_TILE and name == "*"
            valid_stream = folder in FOLDERS and STREAM.fullmatch(name)
            if not reason or not (valid_scope or valid_stream):
                raise ValueError(f"{EXCLUSIONS}: invalid exclusion {folder}/{name}")
            stream = Stream(folder, name)
            if stream in entries:
                raise ValueError(f"{EXCLUSIONS}: duplicate exclusion {folder}/{name}")
            entries[stream] = reason
    return entries


def reference_args(stream, root, point, grain):
    directory = root / stream.folder
    stem = Path(stream.name).stem
    suffix = f"layers/{point}/{stem}_layer{point}" if point else stem
    ref_cmd = (directory / "ref_cmd" / suffix).with_suffix(".sh")
    # The release's scripts contain a shell prologue followed by one aomdec command.
    # Read the command as data. No shell runs it.
    commands = [
        shlex.split(line, comments=True)
        for line in ref_cmd.read_text(encoding="utf-8").splitlines()
    ]
    decoder, = [args for args in commands if args and args != ["set", "-eu"]]
    source = f"$ARGON_STREAMS_INPUT/{stream.name}"
    output = "$ARGON_STREAMS_OUTPUT/out.yuv"
    if decoder[:5] != ["$LIBAV1/aomdec", source, "--rawvideo", "-o", output]:
        raise ValueError(f"{ref_cmd}: unexpected aomdec command")
    flags = decoder[5:]
    allowed = {"--annexb", "--all-layers", "--skip-film-grain"}
    if point:
        allowed.add(f"--oppoint={point}")
    if set(flags) - allowed or (point and f"--oppoint={point}" not in flags):
        raise ValueError(f"{ref_cmd}: unexpected reference flags {flags}")

    args = []
    # The v2.1.1 error commands omit --annexb, but every error stream has
    # Annex B temporal/frame/OBU lengths. Feed the invalid payload to the decoder.
    if "--annexb" in flags or stream.folder == ERROR_FOLDER:
        args.append("-annexb")
    if "--all-layers" in flags:
        args.append("-alllayers")
    args.extend(("-oppoint", str(point)))
    skip_grain = grain == "off" or "--skip-film-grain" in flags
    if stream.folder != ERROR_FOLDER:
        if skip_grain:
            args.append("-skip-film-grain")
        checksums = directory / ("md5_no_film_grain" if skip_grain else "md5_ref")
        reference = (checksums / suffix).with_suffix(".md5")
        if not reference.is_file():
            raise ValueError(f"missing reference {reference}")
        args.extend(("-check_stream", str(reference)))
    return args, "n/a" if stream.folder == ERROR_FOLDER else ("off" if skip_grain else "on")


def classify(name, output, code, negative):
    if DEVICE_LOSS.search(output):
        return "fail", "device_loss", "", "D3D12 device lost during the test"
    if any(line.startswith("CRASH ") for line in output.splitlines()) or code not in range(5):
        return "fail", "crash", "", f"decoder exit code {code}"
    results = [line for line in output.splitlines() if line.startswith(f"RESULT stream={name} ")]
    result = results[0] if len(results) == 1 else ""
    head, _, detail = result.partition(' detail="')
    fields = dict(part.split("=", 1) for part in head.split()[1:] if "=" in part)
    frames = fields.get("frames", "")
    status = fields.get("status")
    detail = detail.rstrip('"')
    if code == 4:
        return "fail", "timeout", frames, detail or "decoder watchdog timed out"
    if not result:
        return "fail", "tool_error", frames, f"exit code {code}, expected one RESULT from the decoder"
    if negative:
        if code == 2 and status == "error" and f"BITSTREAM_ERROR stream={name}" in output.splitlines():
            return "pass", "rejected", frames, detail
        # The specification defines no output for an invalid stream: decoding it is allowed (libaom
        # decodes most of these too). What an invalid stream must not do is crash, hang or lose the
        # device, which the checks above already fail.
        if code == 0 and status == "nocheck":
            return "pass", "decoded_invalid", frames, detail
        if code == 2 and status == "error":
            return "fail", "other_error", frames, detail
    else:
        hash_line = re.search(rf"^STREAM_MD5 stream={re.escape(name)} expected=([0-9a-f]{{32}}) "
                              r"actual=([0-9a-f]{32})\s*$", output, re.MULTILINE)
        if hash_line:
            expected, actual = hash_line.groups()
            if code == 0 and status == "pass" and expected == actual:
                return "pass", "bit_exact", frames, detail
            if code == 1 and status == "mismatch" and expected != actual:
                return "fail", "mismatch", frames, f"expected {expected}, got {actual}; {detail}"
        if code == 2 and status == "error":
            return "fail", "decode_error", frames, detail
    return "fail", "tool_error", frames, f"exit code {code}, status={status}; {detail}"


def run_case(args, stream, exclusions):
    point = args.operating_point
    reason = exclusions.get(stream)
    if reason:
        return (stream.folder, stream.name, point, args.grain, "excluded", "unsupported", "", "", reason)

    start = time.monotonic()
    grain = args.grain
    output = ""
    try:
        source = stream.input(args.streams)
        if not source.is_file():
            raise ValueError(f"missing input {source}")
        if point and stream.folder == ERROR_FOLDER:
            raise ValueError("the error suite supplies only operating point 0")
        flags, grain = reference_args(stream, args.streams, point, grain)
        command = [*args.launcher, str(args.tool), *args.decoder_arg, "-i", str(source), "-name", stream.name,
                   "-t", str(args.threads), "-timeout", str(args.timeout), *flags]
        if args.hw:
            command.append("-hw")
        # Wait on the decoder itself. Launchers such as Wine can leave a helper
        # holding the inherited log handle after the decoder has exited.
        with tempfile.TemporaryFile(dir=args.out) as capture:
            with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=capture,
                                  stderr=subprocess.STDOUT, start_new_session=os.name == "posix") as process:
                try:
                    code = process.wait(timeout=args.timeout + 30)
                except subprocess.TimeoutExpired:
                    if os.name == "posix":
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    else:
                        process.kill()
                    process.wait()
                    code = None
            capture.seek(0)
            output = capture.read().decode("utf-8", errors="replace")
        if code is None:
            result = ("fail", "timeout", "", "external watchdog timed out")
        else:
            result = classify(stream.name, output, code, stream.folder == ERROR_FOLDER)
    except (OSError, ValueError) as error:
        result = ("fail", "harness_error", "", str(error))

    verdict, outcome, frames, detail = result
    if verdict == "fail":
        log = args.out / "logs" / stream.folder / f"{stream.name}.log"
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_text(output or detail + "\n", encoding="utf-8")
    return (stream.folder, stream.name, point, grain, verdict, outcome, frames, f"{time.monotonic() - start:.2f}",
            detail.replace("\t", " ").replace("\n", " "))


def summary(counts, args):
    lines = ["## Argon Streams AV1 Profile 0", "",
             f"{'Hardware adapter' if args.hw else 'WARP'}, operating point {args.operating_point}, "
             f"film grain {args.grain}, shard {args.shard + 1}/{args.shards}.", "",
             "| Folder | Pass | Fail | Excluded |", "| --- | ---: | ---: | ---: |"]
    for folder, n in counts.items():
        lines.append(f"| {folder} | {n['pass']} | {n['fail']} | {n['excluded']} |")
    total = sum(counts.values(), Counter())
    lines.append(f"| **Total** | {total['pass']} | {total['fail']} | {total['excluded']} |")
    text = "\n".join(lines) + "\n"
    print(text)
    (args.out / "summary.md").write_text(text, encoding="utf-8")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as out:
            out.write(text)
    return 1 if total["fail"] else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", type=Path, required=True)
    parser.add_argument("--streams", type=Path, required=True, help="directory containing the Profile 0 folders")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--folder", choices=FOLDERS, action="append")
    parser.add_argument("--list", type=Path, help="Argon list in a --folder, or a list of folder/testN.obu")
    parser.add_argument("--operating-point", type=int, choices=range(32), default=0)
    parser.add_argument("--grain", choices=("on", "off"), default="on")
    parser.add_argument("--shards", type=int, default=1)
    parser.add_argument("--shard", type=int, default=0, help="zero-based shard index")
    parser.add_argument("--threads", type=int, default=4, help="rav1d parser threads per process")
    parser.add_argument("--jobs", type=int, default=1, help="concurrent decoder processes")
    parser.add_argument("--timeout", type=int, default=180, help="decoder watchdog seconds per stream")
    parser.add_argument("--launcher", action="append", default=[], help="prefix command, e.g. --launcher wine")
    parser.add_argument("--hw", action="store_true", help="use a hardware adapter instead of WARP")
    parser.add_argument("--decoder-arg", action="append", default=[], help="additional argument, e.g. --decoder-arg=-intraod")
    args = parser.parse_args()
    if not 0 <= args.shard < args.shards or min(args.jobs, args.threads, args.timeout) < 1:
        parser.error("require 0 <= shard < shards and positive jobs, threads and timeout")
    if set(args.decoder_arg) & RESERVED_FLAGS:
        parser.error("a --decoder-arg conflicts with a harness-controlled option")

    args.tool = args.tool.resolve()
    args.streams = args.streams.resolve()
    if not args.tool.is_file():
        parser.error(f"decoder does not exist: {args.tool}")
    folders = list(dict.fromkeys(args.folder or FOLDERS))
    try:
        exclusions = load_exclusions()
        if args.list:
            streams = stream_list(args.list, folders)
        else:
            streams = []
            for folder in folders:
                entries = stream_list(args.streams / folder / "all_list.txt", [folder])
                if len(entries) != FOLDERS[folder]:
                    raise ValueError(f"{folder}: expected {FOLDERS[folder]} streams, found {len(entries)}")
                streams.extend(entries)
        # Distribute the larger inputs across shards, with stable order for the same release and selection.
        def shard_order(stream):
            try:
                size = stream.input(args.streams).stat().st_size
            except FileNotFoundError:
                size = 0
            return -size, stream.folder, stream.name

        if args.shards > 1:
            streams = sorted(streams, key=shard_order)[args.shard::args.shards]
        if not streams:
            raise ValueError("no streams selected")
        args.out.mkdir(parents=True, exist_ok=True)
        counts = {}
        with ((args.out / "results.tsv").open("w", encoding="utf-8", newline="") as table,
              ThreadPoolExecutor(max_workers=args.jobs) as executor):
            writer = csv.writer(table, delimiter="\t", lineterminator="\n")
            writer.writerow(HEADER)
            for row in executor.map(lambda stream: run_case(args, stream, exclusions), streams):
                writer.writerow(row)
                table.flush()
                folder, name, _, _, verdict, outcome, frames, seconds, _ = row
                counts.setdefault(folder, Counter())[verdict] += 1
                print(f"{verdict:8} {folder}/{name} {outcome} frames={frames} {seconds}s", flush=True)
        return summary(counts, args)
    except (OSError, ValueError) as error:
        print(f"Argon harness: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
