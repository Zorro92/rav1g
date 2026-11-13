"""Makes a damaged copy of an AV1 stream: the tile data of one temporal unit cut short.

Reads an IVF file, takes one temporal unit, finds its largest OBU with tile data (a tile group, or a
frame OBU, which is a frame header with its tile group), keeps the given fraction of that OBU's
payload and writes the stream with the sizes of the OBU and of the IVF frame set right. The stream
then parses up to the cut: the decoder sets the frame up and fails in the tile data ("Failed to
decode tile data" for one tile, "Truncated packet or corrupt tile size" where the cut takes a tile's
size field or a later tile). It is the test stream for a decode error between the setup of a frame
and its submission: the decoder has to take the frame's pipeline slot back, and go on.

usage: python cut_tile.py <in.ivf> <out.ivf> [--unit N] [--fraction F] [--then-whole]

--unit        the temporal unit to damage, from 0 (default 1: behind the first key frame)
--fraction    the part of the OBU's payload that stays (default 0.5)
--then-whole  the whole undamaged stream follows the damaged one in the same file: it starts with its
              key frame, so the frames of that second part are those of the stream, and a decoder that
              went on behind the error has to put them out again unchanged

No stream is in the repository: CI cuts vectors of the conformance set (cut_tile_check.py).
"""

import argparse
import struct
import sys

OBU_TILE_GROUP = 4
OBU_FRAME = 6
OBU_NAMES = {1: 'sequence header', 2: 'temporal delimiter', 3: 'frame header', 4: 'tile group', 5: 'metadata',
             6: 'frame', 7: 'redundant frame header', 8: 'tile list', 15: 'padding'}


class Bad(Exception):
    pass


def read_ivf(path):
    """(the 32-byte header, [(pts, data)])"""
    with open(path, 'rb') as f:
        data = f.read()
    if len(data) < 32 or data[:4] != b'DKIF':
        raise Bad('%s is not an IVF file' % path)
    header_size = struct.unpack_from('<H', data, 6)[0]
    if header_size < 32 or header_size > len(data):
        raise Bad('%s: IVF header of %d bytes' % (path, header_size))
    units = []
    pos = header_size
    while pos + 12 <= len(data):
        size, pts = struct.unpack_from('<IQ', data, pos)
        pos += 12
        if pos + size > len(data):
            raise Bad('%s: frame %d of %d bytes goes past the end of the file' % (path, len(units), size))
        units.append((pts, data[pos:pos + size]))
        pos += size
    return data[:header_size], units


def write_ivf(path, header, units):
    header = bytearray(header)
    struct.pack_into('<I', header, 24, len(units))
    with open(path, 'wb') as f:
        f.write(header)
        for pts, data in units:
            f.write(struct.pack('<IQ', len(data), pts))
            f.write(data)


def read_leb128(data, pos):
    """(value, bytes read)"""
    value = 0
    for i in range(8):
        if pos + i >= len(data):
            raise Bad('a size field goes past the end of the temporal unit')
        byte = data[pos + i]
        value |= (byte & 0x7f) << (7 * i)
        if not byte & 0x80:
            return value, i + 1
    raise Bad('a size field of more than 8 bytes')


def write_leb128(value):
    out = bytearray()
    while True:
        byte = value & 0x7f
        value >>= 7
        out.append(byte | (0x80 if value else 0))
        if not value:
            return bytes(out)


def split_obus(unit):
    """[(type, the bytes of the header in front of the size field, payload)] of a temporal unit."""
    obus = []
    pos = 0
    while pos < len(unit):
        first = unit[pos]
        if first & 0x80:
            raise Bad('an OBU header with the forbidden bit set')
        obu_type = (first >> 3) & 15
        head = 2 if first & 4 else 1
        if not first & 2:
            raise Bad('an OBU without a size field (not the low overhead format of IVF)')
        size, size_bytes = read_leb128(unit, pos + head)
        start = pos + head + size_bytes
        if start + size > len(unit):
            raise Bad('an OBU of %d bytes goes past the end of its temporal unit' % size)
        obus.append((obu_type, unit[pos:pos + head], unit[start:start + size]))
        pos = start + size
    return obus


def cut_unit(unit, fraction):
    """(the unit with its largest tile data OBU cut, a text about what was cut)"""
    obus = split_obus(unit)
    with_tiles = [i for i, (obu_type, _, payload) in enumerate(obus) if obu_type in (OBU_TILE_GROUP, OBU_FRAME)]
    if not with_tiles:
        raise Bad('the temporal unit has no OBU with tile data (%s)' % ', '.join(
            OBU_NAMES.get(obu_type, str(obu_type)) for obu_type, _, _ in obus))
    target = max(with_tiles, key=lambda i: len(obus[i][2]))
    obu_type, head, payload = obus[target]
    keep = max(1, min(len(payload) - 1, int(len(payload) * fraction)))
    if len(payload) < 2:
        raise Bad('the OBU with the tile data has %d bytes: nothing to cut' % len(payload))
    obus[target] = (obu_type, head, payload[:keep])
    out = b''.join(head + write_leb128(len(payload)) + payload for _, head, payload in obus)
    return out, '%s OBU, %d of %d bytes kept; the temporal unit has %d OBUs, %d bytes before and %d after' % (
        OBU_NAMES[obu_type], keep, len(payload), len(obus), len(unit), len(out))


def cut_stream(source, target, unit, fraction, then_whole):
    """Writes the damaged stream; returns (temporal units of the source, a text about the cut)."""
    header, units = read_ivf(source)
    if not 0 <= unit < len(units):
        raise Bad('%s has %d temporal units, there is no unit %d' % (source, len(units), unit))
    damaged, what = cut_unit(units[unit][1], fraction)
    out = list(units)
    out[unit] = (units[unit][0], damaged)
    if then_whole:
        # Time stamps that go on behind the first part.
        step = units[-1][0] - units[-2][0] if len(units) > 1 else 1
        offset = units[-1][0] + max(step, 1) - units[0][0]
        out += [(pts + offset, data) for pts, data in units]
    write_ivf(target, header, out)
    return len(units), what


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('source', help='an IVF file with AV1')
    parser.add_argument('target', help='the damaged stream to write')
    parser.add_argument('--unit', type=int, default=1, help='the temporal unit to damage, from 0 (default 1)')
    parser.add_argument('--fraction', type=float, default=0.5, help='the part of the payload that stays (default 0.5)')
    parser.add_argument('--then-whole', action='store_true', help='append the whole undamaged stream')
    args = parser.parse_args()
    if not 0.0 <= args.fraction < 1.0:
        parser.error('--fraction is at least 0 and less than 1')
    try:
        count, what = cut_stream(args.source, args.target, args.unit, args.fraction, args.then_whole)
    except (Bad, OSError) as error:
        print('cut_tile.py: %s' % error)
        return 2
    print('cut_tile.py: %s: temporal unit %d of %d: %s%s' % (
        args.target, args.unit, count, what, '; the whole stream follows' if args.then_whole else ''))
    return 0


if __name__ == '__main__':
    sys.exit(main())
