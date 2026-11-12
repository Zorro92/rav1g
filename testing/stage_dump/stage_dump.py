#!/usr/bin/env python3
# Copyright (c) 2020, Alliance for Open Media. All rights reserved
#
# This source code is subject to the terms of the BSD 2 Clause License and
# the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
# was not distributed with this source code in the LICENSE file, you can
# obtain it at www.aomedia.org/license/software. If the Alliance for Open
# Media Patent License 1.0 was not distributed with this source code in the
# PATENTS file, you can obtain it at www.aomedia.org/license/patent.
"""Reads the stage dumps of the test player and of warp_decoder (-dumpstages) and compares two runs.

  stage_dump.py list <file or directory>...
      what is in the dumps: one line per dump (frame, stage, layout) and the frame's info line

  stage_dump.py compare <A> <B> [--all] [--samples N]
      A and B: two .stages files, or two directories (the files of the same frame number are paired; with
      several streams in a directory, the files of the same name). Either side may be a digest. Per frame
      and stage: equal or not, and for the first stage that differs the pattern of the differences in
      every plane (how many, how large, rows and columns, where in the 8x8 and 64x64 grid, which byte of a
      32-bit word, runs). --all prints the pattern for every stage that differs. Exit code 1 when a stage
      differs or a frame is missing on one side.

  stage_dump.py digest <file.stages or directory> <output directory>
      writes <name>.digest: the same dumps with the samples replaced by a hash per row and a CRC per 8x8
      block. A digest has no samples (it is not media) and is small; compare takes it in place of a dump
      and then names the rows and blocks that differ, but not by how much.

  stage_dump.py extract <file.stages> <stage> <out.yuv> [--occurrence N]
      the visible samples of one dump, planar (Y, U, V; one byte a sample for 8-bit, else two, low first)

  stage_dump.py compare-yuv <file.stages> <stage> <ref.yuv> [--frame N] [--width W --height H]
      compares one dump with frame N of a planar 4:2:0 file of the same sample size (for example a
      reference decoder's output; --width/--height when the file's frames are not of the dump's size)

Frame numbers are the decoder's: frames in decoding order from 0, hidden frames included; a
show_existing_frame is not a frame of its own (its output dump is appended to the file of the frame it
shows, marked sef).

No dependencies beyond the Python standard library.
"""

import argparse
import hashlib
import os
import re
import struct
import sys
import zlib

MAGIC_DUMP = b"RAV1STG1"
MAGIC_DIGEST = b"RAV1DIG1"
HEADER_SIZE = 384
PLANE_NAMES = ("Y", "U", "V")


class Dump:
    """One dump: its header, and either the buffer (data) or, for a digest, the hashes."""

    def __init__(self):
        self.frame = 0
        self.index = 0
        self.kind = 0
        self.bytes_per_sample = 1
        self.bit_depth = 8
        self.is_sef = 0
        self.planes = []  # (offset, stride, width, height)
        self.stage = ""
        self.info = ""
        self.data = None  # bytes of the buffer; None for a digest
        self.row_hashes = None  # per plane: list of 8-byte hashes, one per row
        self.block_crcs = None  # per plane: list of CRCs, one per 8x8 block, row-major
        self.path = ""

    def label(self):
        return "%s%s" % (self.stage, " (sef)" if self.is_sef else "")

    def has_plane(self, p):
        return self.planes[p][2] > 0 and self.planes[p][3] > 0

    def row(self, p, y):
        offset, stride, width, _ = self.planes[p]
        start = offset + y * stride
        return self.data[start:start + width * self.bytes_per_sample]

    def rows(self, p):
        return [self.row(p, y) for y in range(self.planes[p][3])]


def plane_hashes(rows, width, bps):
    """Row hashes and 8x8 block CRCs of a plane given as a list of row byte strings."""
    row_hashes = [hashlib.md5(r).digest()[:8] for r in rows]
    crcs = []
    step = 8 * bps
    for by in range(0, len(rows), 8):
        band = rows[by:by + 8]
        for bx in range(0, width * bps, step):
            c = 0
            for r in band:
                c = zlib.crc32(r[bx:bx + step], c)
            crcs.append(c)
    return row_hashes, crcs


def read_file(path):
    """All dumps of a .stages or .digest file, in file order."""
    dumps = []
    with open(path, "rb") as f:
        while True:
            header = f.read(HEADER_SIZE)
            if not header:
                break
            if len(header) < HEADER_SIZE or header[:8] not in (MAGIC_DUMP, MAGIC_DIGEST):
                raise ValueError("%s: not a stage dump at offset %d" % (path, f.tell() - len(header)))
            (size,) = struct.unpack_from("<I", header, 8)
            if size != HEADER_SIZE:
                raise ValueError("%s: header of %d bytes, this script reads %d" % (path, size, HEADER_SIZE))
            d = Dump()
            d.path = path
            d.frame, d.index, d.kind, d.bytes_per_sample, d.bit_depth, d.is_sef = struct.unpack_from("<6i", header, 12)
            d.planes = [struct.unpack_from("<4I", header, 40 + 16 * p) for p in range(3)]
            (data_size,) = struct.unpack_from("<Q", header, 88)
            d.stage = header[96:128].split(b"\0")[0].decode("ascii", "replace")
            d.info = header[128:384].split(b"\0")[0].decode("ascii", "replace")
            data = f.read(data_size)
            if len(data) != data_size:
                raise ValueError("%s: dump %d (%s) is cut short" % (path, d.index, d.stage))
            if header[:8] == MAGIC_DUMP:
                d.data = data
            else:
                d.row_hashes, d.block_crcs = [], []
                pos = 0
                for p in range(3):
                    _, _, width, height = d.planes[p]
                    d.row_hashes.append([data[pos + 8 * i:pos + 8 * i + 8] for i in range(height)])
                    pos += 8 * height
                    blocks = ((width + 7) // 8) * ((height + 7) // 8)
                    d.block_crcs.append(list(struct.unpack_from("<%dI" % blocks, data, pos)))
                    pos += 4 * blocks
            dumps.append(d)
    return dumps


def hashes_of(d, p):
    if d.data is None:
        return d.row_hashes[p], d.block_crcs[p]
    return plane_hashes(d.rows(p), d.planes[p][2], d.bytes_per_sample)


def write_digest(dumps, path):
    with open(path, "wb") as f:
        for d in dumps:
            body = b""
            for p in range(3):
                if d.has_plane(p):
                    row_hashes, crcs = hashes_of(d, p)
                    body += b"".join(row_hashes) + struct.pack("<%dI" % len(crcs), *crcs)
            header = bytearray(HEADER_SIZE)
            header[:8] = MAGIC_DIGEST
            struct.pack_into("<I", header, 8, HEADER_SIZE)
            struct.pack_into("<6i", header, 12, d.frame, d.index, d.kind, d.bytes_per_sample, d.bit_depth, d.is_sef)
            for p in range(3):
                struct.pack_into("<4I", header, 40 + 16 * p, *d.planes[p])
            struct.pack_into("<Q", header, 88, len(body))
            header[96:96 + min(31, len(d.stage))] = d.stage.encode("ascii", "replace")[:31]
            header[128:128 + min(255, len(d.info))] = d.info.encode("ascii", "replace")[:255]
            f.write(bytes(header))
            f.write(body)


def stage_files(path):
    """The .stages and .digest files of a directory (or the file itself), keyed by (stream, frame)."""
    if os.path.isfile(path):
        names = [path]
    else:
        names = [os.path.join(path, n) for n in sorted(os.listdir(path)) if n.endswith((".stages", ".digest"))]
    out = {}
    for n in names:
        m = re.match(r"^(.*)_f(\d+)\.(stages|digest)$", os.path.basename(n))
        key = (m.group(1), int(m.group(2))) if m else (os.path.basename(n), -1)
        # A dump beats its digest when a directory has both.
        if key not in out or n.endswith(".stages"):
            out[key] = n
    return out


def samples(row, bps, signed):
    if bps == 1:
        return row
    return struct.unpack("<%d%s" % (len(row) // 2, "h" if signed else "H"), row)


def histogram_text(counts, limit=None):
    items = sorted(counts.items())
    if limit and len(items) > limit:
        items = sorted(sorted(counts.items(), key=lambda kv: -kv[1])[:limit])
        return " ".join("%s:%d" % kv for kv in items) + " (the %d most frequent of %d)" % (limit, len(counts))
    return " ".join("%s:%d" % kv for kv in items)


def diff_pattern(rows_a, rows_b, width, bps, signed, max_samples, out):
    """Prints the pattern of the differences between two planes given as lists of row byte strings."""
    height = min(len(rows_a), len(rows_b))
    count = 0
    plus = minus = 0
    max_abs = 0
    mags = {}
    bad_rows = []
    cols = set()
    mod_word = {}
    mod8x = {}
    mod8y = {}
    mod64x = {}
    mod64y = {}
    blocks8 = set()
    blocks64 = {}
    hruns = {}
    first = []
    col_hits = {}
    samples_per_word = 4 // bps
    for y in range(height):
        ra, rb = rows_a[y], rows_b[y]
        if ra == rb:
            continue
        bad_rows.append(y)
        sa, sb = samples(ra, bps, signed), samples(rb, bps, signed)
        run = 0
        for x in range(min(len(sa), len(sb))):
            d = sb[x] - sa[x]
            if d == 0:
                if run:
                    hruns[run] = hruns.get(run, 0) + 1
                    run = 0
                continue
            run += 1
            count += 1
            if d > 0:
                plus += 1
            else:
                minus += 1
            a = abs(d)
            if a > max_abs:
                max_abs = a
            bucket = a if a <= 4 else (8 if a <= 8 else 16 if a <= 16 else 64 if a <= 64 else 256 if a <= 256 else 65536)
            mags[bucket] = mags.get(bucket, 0) + 1
            cols.add(x)
            col_hits[x] = col_hits.get(x, 0) + 1
            mod_word[x % samples_per_word] = mod_word.get(x % samples_per_word, 0) + 1
            mod8x[x & 7] = mod8x.get(x & 7, 0) + 1
            mod8y[y & 7] = mod8y.get(y & 7, 0) + 1
            mod64x[x & 63] = mod64x.get(x & 63, 0) + 1
            mod64y[y & 63] = mod64y.get(y & 63, 0) + 1
            blocks8.add((x >> 3, y >> 3))
            key = (x >> 6, y >> 6)
            blocks64[key] = blocks64.get(key, 0) + 1
            if len(first) < max_samples:
                first.append((x, y, sa[x], sb[x]))
        if run:
            hruns[run] = hruns.get(run, 0) + 1
    total = width * height
    if not count:
        out("      no sample differs")
        return 0
    out("      %d of %d samples differ (%.3f%%): B above A %d, B below A %d, largest difference %d"
        % (count, total, 100.0 * count / total, plus, minus, max_abs))
    names = {1: "1", 2: "2", 3: "3", 4: "4", 8: "5-8", 16: "9-16", 64: "17-64", 256: "65-256", 65536: ">256"}
    out("      size of the difference: " + " ".join("%s:%d" % (names[k], v) for k, v in sorted(mags.items())))
    out("      rows %d..%d (%d rows with a difference), columns %d..%d (%d columns)"
        % (bad_rows[0], bad_rows[-1], len(bad_rows), min(cols), max(cols), len(cols)))
    out("      8x8 blocks with a difference: %d; 64x64 blocks: %d of %d"
        % (len(blocks8), len(blocks64), ((width + 63) // 64) * ((height + 63) // 64)))
    worst = sorted(blocks64.items(), key=lambda kv: -kv[1])[:8]
    out("      64x64 blocks with the most differences (column,row of the block: samples): "
        + " ".join("%d,%d:%d" % (k[0], k[1], v) for k, v in worst))
    out("      sample within its 32-bit word (x mod %d): %s" % (samples_per_word, histogram_text(mod_word)))
    out("      x mod 8: %s" % histogram_text(mod8x))
    out("      y mod 8: %s" % histogram_text(mod8y))
    out("      x mod 64: %s" % histogram_text(mod64x, 24))
    out("      y mod 64: %s" % histogram_text(mod64y, 24))
    out("      horizontal runs of differing samples (length:count): %s" % histogram_text(hruns, 16))
    busiest = sorted(col_hits.items(), key=lambda kv: -kv[1])[:8]
    out("      columns with the most differences (x:samples): " + " ".join("%d:%d" % kv for kv in sorted(busiest)))
    out("      first differences (x,y: A -> B): " + " ".join("%d,%d:%d->%d" % s for s in first))
    return count


def hash_pattern(da, db, p, out):
    """Compares a plane through row hashes and 8x8 block CRCs (one side is a digest)."""
    rows_a, crcs_a = hashes_of(da, p)
    rows_b, crcs_b = hashes_of(db, p)
    width, height = da.planes[p][2], da.planes[p][3]
    bad_rows = [y for y in range(min(len(rows_a), len(rows_b))) if rows_a[y] != rows_b[y]]
    if not bad_rows:
        return 0
    per_row = (width + 7) // 8
    bad = [i for i in range(min(len(crcs_a), len(crcs_b))) if crcs_a[i] != crcs_b[i]]
    blocks64 = {}
    mod8x = {}
    mod8y = {}
    for i in bad:
        bx, by = i % per_row, i // per_row
        blocks64[(bx >> 3, by >> 3)] = blocks64.get((bx >> 3, by >> 3), 0) + 1
        mod8x[bx & 7] = mod8x.get(bx & 7, 0) + 1
        mod8y[by & 7] = mod8y.get(by & 7, 0) + 1
    out("      %d of %d rows differ: rows %d..%d" % (len(bad_rows), height, bad_rows[0], bad_rows[-1]))
    out("      8x8 blocks with a difference: %d of %d; 64x64 blocks: %d of %d"
        % (len(bad), len(crcs_a), len(blocks64), ((width + 63) // 64) * ((height + 63) // 64)))
    if bad:
        xs = [i % per_row for i in bad]
        out("      8x8 block columns %d..%d, 8x8 block rows %d..%d" % (min(xs), max(xs), bad[0] // per_row, bad[-1] // per_row))
        out("      8x8 block within its 64x64 block, column: %s" % histogram_text(mod8x))
        out("      8x8 block within its 64x64 block, row: %s" % histogram_text(mod8y))
        worst = sorted(blocks64.items(), key=lambda kv: -kv[1])[:8]
        out("      64x64 blocks with the most 8x8 blocks that differ (column,row: blocks): "
            + " ".join("%d,%d:%d" % (k[0], k[1], v) for k, v in worst))
        out("      first 8x8 blocks that differ (x,y of the block's first sample): "
            + " ".join("%d,%d" % ((i % per_row) * 8, (i // per_row) * 8) for i in bad[:12]))
    out("      (one side is a digest: which rows and blocks differ, not by how much)")
    return len(bad_rows)


def compare_dumps(da, db, detail, max_samples, out):
    """Compares two dumps of the same stage. Returns True when every plane is equal."""
    equal = True
    lines = []
    for p in range(3):
        if not da.has_plane(p) and not db.has_plane(p):
            continue
        if da.planes[p][2:] != db.planes[p][2:] or da.bytes_per_sample != db.bytes_per_sample:
            lines.append("    %s: sizes differ, A %dx%d %d bytes a sample, B %dx%d %d bytes a sample"
                         % (PLANE_NAMES[p], da.planes[p][2], da.planes[p][3], da.bytes_per_sample, db.planes[p][2],
                            db.planes[p][3], db.bytes_per_sample))
            equal = False
            continue
        width, height = da.planes[p][2], da.planes[p][3]
        if da.data is not None and db.data is not None:
            rows_a, rows_b = da.rows(p), db.rows(p)
            if rows_a == rows_b:
                lines.append("    %s: equal" % PLANE_NAMES[p])
                continue
            equal = False
            bad_rows = sum(1 for y in range(height) if rows_a[y] != rows_b[y])
            lines.append("    %s: DIFFERS in %d of %d rows" % (PLANE_NAMES[p], bad_rows, height))
            if detail:
                diff_pattern(rows_a, rows_b, width, da.bytes_per_sample, da.kind == 1, max_samples, lines.append)
        else:
            sub = []
            if hash_pattern(da, db, p, sub.append):
                equal = False
                lines.append("    %s: DIFFERS" % PLANE_NAMES[p])
                if detail:
                    lines.extend(sub)
                else:
                    lines.append(sub[0])
            else:
                lines.append("    %s: equal" % PLANE_NAMES[p])
    for line in lines:
        out(line)
    return equal


def cmd_list(args):
    for path in args.paths:
        for key, name in sorted(stage_files(path).items(), key=lambda kv: (kv[0][0], kv[0][1])):
            dumps = read_file(name)
            print("%s: %d dumps" % (name, len(dumps)))
            shown = None
            for d in dumps:
                if d.info != shown:
                    print("  frame %d%s: %s" % (d.frame, " (sef)" if d.is_sef else "", d.info))
                    shown = d.info
                planes = " ".join("%s %dx%d stride %d at %d" % (PLANE_NAMES[p], d.planes[p][2], d.planes[p][3],
                                                               d.planes[p][1], d.planes[p][0])
                                  for p in range(3) if d.has_plane(p))
                print("    %2d %-13s %s, %d bytes a sample, %d-bit%s: %s"
                      % (d.index, d.label(), "residuals" if d.kind == 1 else "samples", d.bytes_per_sample,
                         d.bit_depth, " (digest)" if d.data is None else "", planes))
    return 0


def pair_files(a, b):
    fa, fb = stage_files(a), stage_files(b)
    streams_a = sorted(set(k[0] for k in fa))
    streams_b = sorted(set(k[0] for k in fb))
    pairs = []
    missing = []
    if len(streams_a) == 1 and len(streams_b) == 1:
        # One stream on each side: the two programs may call it differently, the frame number pairs the files.
        by_frame_b = dict((k[1], v) for k, v in fb.items())
        for k, v in sorted(fa.items(), key=lambda kv: kv[0][1]):
            if k[1] in by_frame_b:
                pairs.append((v, by_frame_b.pop(k[1])))
            else:
                missing.append("%s has no counterpart in B" % v)
        missing.extend("%s has no counterpart in A" % v for v in by_frame_b.values())
    else:
        for k, v in sorted(fa.items()):
            if k in fb:
                pairs.append((v, fb[k]))
            else:
                missing.append("%s has no counterpart in B" % v)
        missing.extend("%s has no counterpart in A" % v for k, v in sorted(fb.items()) if k not in fa)
    return pairs, missing


def cmd_compare(args):
    pairs, missing = pair_files(args.a, args.b)
    all_equal = not missing
    for line in missing:
        print("MISSING %s" % line)
    if not pairs:
        print("nothing to compare")
        return 2
    for file_a, file_b in pairs:
        dumps_a, dumps_b = read_file(file_a), read_file(file_b)
        print("A %s\nB %s" % (file_a, file_b))
        if dumps_a:
            print("  frame %d: %s" % (dumps_a[0].frame, dumps_a[0].info))
        if dumps_a and dumps_b and dumps_a[0].info != dumps_b[0].info:
            print("  NOTE the frame's info line differs in B: %s" % dumps_b[0].info)
            all_equal = False

        # Pair the dumps by stage name and by how often the name came before (two superres dumps, sef outputs).
        def keyed(dumps):
            seen = {}
            out = []
            for d in dumps:
                k = (d.stage, d.is_sef)
                seen[k] = seen.get(k, 0) + 1
                out.append(((d.stage, d.is_sef, seen[k]), d))
            return out

        ka, kb = keyed(dumps_a), keyed(dumps_b)
        map_b = dict(kb)
        first_bad = None
        for key, da in ka:
            db = map_b.pop(key, None)
            if db is None:
                print("  stage %-13s only in A" % da.label())
                all_equal = False
                continue
            detail = args.all or first_bad is None
            lines = []
            equal = compare_dumps(da, db, detail, args.samples, lines.append)
            print("  stage %-13s %s" % (da.label(), "equal" if equal else "DIFFERS"))
            if not equal:
                all_equal = False
                if first_bad is None:
                    first_bad = da.label()
                    print("  ^ first stage that differs; its pattern:")
                for line in lines:
                    print(line)
        for key, db in kb:
            if key in map_b:
                print("  stage %-13s only in B" % db.label())
                all_equal = False
        print("  frame %d: %s" % (dumps_a[0].frame if dumps_a else -1,
                                 "all stages equal" if first_bad is None else "first difference at stage " + first_bad))
    print("RESULT %s" % ("equal" if all_equal else "DIFFERENT"))
    return 0 if all_equal else 1


def cmd_digest(args):
    os.makedirs(args.out, exist_ok=True)
    for key, name in sorted(stage_files(args.path).items()):
        if name.endswith(".digest"):
            continue
        dumps = read_file(name)
        target = os.path.join(args.out, os.path.basename(name)[:-len(".stages")] + ".digest")
        write_digest(dumps, target)
        print("%s: %d dumps, %d bytes -> %s, %d bytes" % (name, len(dumps), os.path.getsize(name), target,
                                                         os.path.getsize(target)))
    return 0


def find_dump(path, stage, occurrence):
    found = [d for d in read_file(path) if d.stage == stage]
    if occurrence >= len(found):
        raise SystemExit("%s has %d dumps of stage %s" % (path, len(found), stage))
    if found[occurrence].data is None:
        raise SystemExit("%s is a digest: it has no samples" % path)
    return found[occurrence]


def cmd_extract(args):
    d = find_dump(args.file, args.stage, args.occurrence)
    with open(args.out, "wb") as f:
        for p in range(3):
            if d.has_plane(p):
                for row in d.rows(p):
                    f.write(row)
    print("%s stage %s: %s -> %s" % (args.file, d.label(), " ".join(
        "%s %dx%d" % (PLANE_NAMES[p], d.planes[p][2], d.planes[p][3]) for p in range(3) if d.has_plane(p)), args.out))
    return 0


def cmd_compare_yuv(args):
    d = find_dump(args.file, args.stage, args.occurrence)
    bps = d.bytes_per_sample
    width = args.width or d.planes[0][2]
    height = args.height or d.planes[0][3]
    dims = [(width, height), ((width + 1) // 2, (height + 1) // 2), ((width + 1) // 2, (height + 1) // 2)]
    frame_bytes = sum(w * h for w, h in dims) * bps
    with open(args.yuv, "rb") as f:
        f.seek(frame_bytes * args.frame)
        ref = f.read(frame_bytes)
    if len(ref) != frame_bytes:
        raise SystemExit("%s has no frame %d of %dx%d, %d bytes a sample" % (args.yuv, args.frame, width, height, bps))
    print("A %s stage %s\nB %s frame %d\n  %s" % (args.file, d.label(), args.yuv, args.frame, d.info))
    equal = True
    pos = 0
    for p in range(3):
        w, h = dims[p]
        rows_b = [ref[pos + y * w * bps:pos + (y + 1) * w * bps] for y in range(h)]
        pos += w * h * bps
        if not d.has_plane(p):
            continue
        if (d.planes[p][2], d.planes[p][3]) != (w, h):
            print("    %s: sizes differ, dump %dx%d, file %dx%d" % (PLANE_NAMES[p], d.planes[p][2], d.planes[p][3], w, h))
            equal = False
            continue
        rows_a = d.rows(p)
        if rows_a == rows_b:
            print("    %s: equal" % PLANE_NAMES[p])
            continue
        equal = False
        print("    %s: DIFFERS in %d of %d rows" % (PLANE_NAMES[p], sum(1 for y in range(h) if rows_a[y] != rows_b[y]), h))
        diff_pattern(rows_a, rows_b, w, bps, False, args.samples, print)
    print("RESULT %s" % ("equal" if equal else "DIFFERENT"))
    return 0 if equal else 1


def main_with(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("list")
    p.add_argument("paths", nargs="+")
    p.set_defaults(func=cmd_list)
    p = sub.add_parser("compare")
    p.add_argument("a")
    p.add_argument("b")
    p.add_argument("--all", action="store_true", help="the pattern of every stage that differs, not only the first")
    p.add_argument("--samples", type=int, default=12, help="how many differing samples to list (default 12)")
    p.set_defaults(func=cmd_compare)
    p = sub.add_parser("digest")
    p.add_argument("path")
    p.add_argument("out")
    p.set_defaults(func=cmd_digest)
    p = sub.add_parser("extract")
    p.add_argument("file")
    p.add_argument("stage")
    p.add_argument("out")
    p.add_argument("--occurrence", type=int, default=0)
    p.set_defaults(func=cmd_extract)
    p = sub.add_parser("compare-yuv")
    p.add_argument("file")
    p.add_argument("stage")
    p.add_argument("yuv")
    p.add_argument("--frame", type=int, default=0)
    p.add_argument("--width", type=int, default=0)
    p.add_argument("--height", type=int, default=0)
    p.add_argument("--occurrence", type=int, default=0)
    p.add_argument("--samples", type=int, default=12)
    p.set_defaults(func=cmd_compare_yuv)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main_with(sys.argv[1:]))
