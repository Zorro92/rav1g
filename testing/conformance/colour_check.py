#!/usr/bin/env python3
"""What the decoder reports about the colour of a stream, against two references that are not the decoder.

  python3 testing/conformance/colour_check.py --streams <dir> --logs <name>=<dir> [--logs ...] --out <dir>

The decoder passes on what a stream says about its colour (docs/color.md): the colour description of the
sequence header (Rav1gStreamInfo::color, and with every Rav1gFrame) and the HDR metadata OBUs (mastering
display colour volume, content light level; with the frames). rav1g-dec prints it:

  COLOUR <stream> sequence: <description>                    the sequence header a decoder is created for
  COLOUR <stream> frame <n>: <description>; <MDCV>; <CLL>    the first frame of a decoder, and every frame
                                                             that says something else than the one before

(A stream whose frames grow gets a second decoder in rav1g-dec, and with it a second pair of lines.)

<dir> of --streams is what prepare_streams.sh wrote. <dir> of --logs is the logs directory of a conformance
run (run.py: logs/<stream>.log), <name> what the report calls the run: the runs through the Rust interface
and through rav1g.h (-capi) must say the same.

The references, for every stream of the manifest:

  ffmpeg -bsf:v trace_headers   the syntax elements of the first sequence header as FFmpeg's own AV1 reader
                                (cbs_av1) reads them: color_description_present_flag, the three code points
                                as numbers, color_range, chroma_sample_position. Held against the
                                "sequence" line.
  ffprobe -show_frames          (libdav1d): the colour properties and the side data (mastering display,
                                content light level) of every output frame. The frames at which they change
                                are the "frame" lines the decoder has to print, with these values.

And a list of what some streams are known to carry (KNOWN below), which both the references and the decoder
have to agree with: so the check cannot pass on a set, or with references, that say nothing.

Written to --out: summary.md (also appended to $GITHUB_STEP_SUMMARY), reference.tsv (what the references
say, per stream).

Exit code: 0 all equal, 1 a difference, 2 the check could not be done.
"""
import argparse
import json
import os
import re
import subprocess
import sys
from fractions import Fraction

# ffprobe's names (libavutil/pixdesc.c) to the code points of the AV1 specification, which are those of
# ITU-T H.273. 'unknown' is 2 (unspecified); 'reserved' has no number of its own and is not compared.
PRIMARIES = {'bt709': 1, 'unknown': 2, 'bt470m': 4, 'bt470bg': 5, 'smpte170m': 6, 'smpte240m': 7, 'film': 8,
             'bt2020': 9, 'smpte428': 10, 'smpte431': 11, 'smpte432': 12, 'ebu3213': 22}
TRANSFER = {'bt709': 1, 'unknown': 2, 'bt470m': 4, 'bt470bg': 5, 'smpte170m': 6, 'smpte240m': 7, 'linear': 8,
            'log100': 9, 'log316': 10, 'iec61966-2-4': 11, 'bt1361e': 12, 'iec61966-2-1': 13, 'bt2020-10': 14,
            'bt2020-12': 15, 'smpte2084': 16, 'smpte428': 17, 'arib-std-b67': 18}
MATRIX = {'gbr': 0, 'bt709': 1, 'unknown': 2, 'fcc': 4, 'bt470bg': 5, 'smpte170m': 6, 'smpte240m': 7, 'ycgco': 8,
          'bt2020nc': 9, 'bt2020c': 10, 'smpte2085': 11, 'chroma-derived-nc': 12, 'chroma-derived-c': 13,
          'ictcp': 14}
RANGE = {'tv': 0, 'pc': 1}
# chroma_sample_position: 1 (vertical) is "left", 2 (colocated) is "topleft" (libavcodec/libdav1d.c).
CHROMA = {'unspecified': 0, 'left': 1, 'topleft': 2}

# What these streams carry: (color_description_present_flag, primaries, transfer, matrix, full range, chroma
# sample position, mastering display or None, content light level or None). The mastering display is the
# numbers of the OBU: x,y of red, green, blue and the white point, the largest and the smallest luminance.
KNOWN = {
    # 10-bit 2160p of the DVB-DASH set: BT.2020 with the HLG transfer, and so no HDR10 metadata.
    'nf-dvbdash-d2-10b-2160p.ivf': (1, 9, 18, 9, 0, 1, None, None),
    # 8-bit 2160p of the same set: BT.709.
    'nf-dvbdash-a1-8b-2160p.ivf': (1, 1, 1, 1, 0, 1, None, None),
    # Encoded for this check (streams.txt): HDR10 with a mastering display and light levels.
    'syn-hdr10-10b-1080p.ivf': (1, 9, 16, 9, 0, 2,
                                (44564, 20972, 17367, 45220, 9830, 3932, 20493, 21561, 256000, 82), (1000, 400)),
    # No AOM test vector has a colour description; one of them, by name.
    'av1-1-b8-01-size-16x16.ivf': (0, 2, 2, 2, 0, 0, None, None),
}

DESCRIPTION = (r'primaries (\d+) \([^)]*\) transfer (\d+) \([^)]*\) matrix (\d+) \([^)]*\) range (limited|full) '
               r'chroma pos (\d+) \(\w+\)(, no colour description)?')
SEQUENCE_LINE = re.compile(r'^COLOUR (\S+) sequence: ' + DESCRIPTION + r'$')
FRAME_LINE = re.compile(r'^COLOUR (\S+) frame (\d+): ' + DESCRIPTION + r'; (no MDCV|MDCV [^;]*); (no CLL|CLL (\d+)/(\d+))$')
MDCV_NUMBERS = re.compile(r'\[(\d+),(\d+) (\d+),(\d+) (\d+),(\d+) (\d+),(\d+) (\d+) (\d+)\]$')


class CannotCheck(Exception):
    pass


def run(command):
    try:
        done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    except OSError as error:
        raise CannotCheck('%s: %s' % (command[0], error))
    return done.returncode, done.stdout.decode('utf-8', 'replace'), done.stderr.decode('utf-8', 'replace')


def description(match, first):
    """(present, primaries, transfer, matrix, full range, chroma position) of a DESCRIPTION match whose first
    group is `first`."""
    primaries, transfer, matrix, colour_range, chroma, absent = match.group(*range(first, first + 6))
    return (0 if absent else 1, int(primaries), int(transfer), int(matrix), 1 if colour_range == 'full' else 0,
            int(chroma))


def decoder_lines(path):
    """The COLOUR lines of a stream's log: (sequence descriptions, [(frame, description, mdcv, cll)]), with
    the lines that are not understood."""
    sequences, frames, bad = [], [], []
    with open(path, encoding='utf-8', errors='replace') as f:
        for line in f.read().replace('\r', '').split('\n'):
            if not line.startswith('COLOUR '):
                continue
            match = SEQUENCE_LINE.match(line)
            if match:
                sequences.append(description(match, 2))
                continue
            match = FRAME_LINE.match(line)
            if not match:
                bad.append(line)
                continue
            mdcv = None
            if match.group(9) != 'no MDCV':
                numbers = MDCV_NUMBERS.search(match.group(9))
                if not numbers:
                    bad.append(line)
                    continue
                mdcv = tuple(int(n) for n in numbers.groups())
            cll = None if match.group(10) == 'no CLL' else (int(match.group(11)), int(match.group(12)))
            frames.append((int(match.group(2)), description(match, 3), mdcv, cll))
    return sequences, frames, bad


def traced_sequence(ffmpeg, path):
    """The colour description of the first sequence header of a stream, from FFmpeg's trace_headers:
    (present, primaries, transfer, matrix, full range, chroma position)."""
    code, _, err = run([ffmpeg, '-nostdin', '-hide_banner', '-i', path, '-map', '0:v:0', '-frames:v', '1', '-c',
                        'copy', '-bsf:v', 'trace_headers', '-f', 'null', '-'])
    values = {}
    in_sequence = False
    for line in err.split('\n'):
        match = re.search(r'\]\s+\d+\s+(\S+)\s+[01]+ = (\d+)\s*$', line)
        if not match:
            continue
        name, value = match.group(1), int(match.group(2))
        if name == 'obu_type':
            if in_sequence:
                break  # the OBU behind the first sequence header
            in_sequence = value == 1
        elif in_sequence and name not in values:
            values[name] = value
    if 'color_description_present_flag' not in values:
        raise CannotCheck('%s -bsf:v trace_headers shows no sequence header of %s (exit code %d): %s'
                          % (ffmpeg, path, code, err.strip().split('\n')[-1]))
    present = values['color_description_present_flag']
    # BT.709 primaries with the sRGB transfer and the identity matrix have no range bit: full range.
    full = values.get('color_range', 1)
    return (present, values.get('color_primaries', 2), values.get('transfer_characteristics', 2),
            values.get('matrix_coefficients', 2), full, values.get('chroma_sample_position', 0))


def numerator(value, denominator):
    """A rational of ffprobe ("44564/65536") as the number of the OBU, whose denominator is fixed."""
    number = Fraction(value) * denominator
    if number.denominator != 1:
        raise CannotCheck('%s is not a multiple of 1/%d' % (value, denominator))
    return int(number)


def probed_frames(ffprobe, path):
    """Per output frame, from ffprobe with libdav1d: ((primaries, transfer, matrix, full range, chroma
    position), mdcv, cll); a property ffprobe has no name for is None."""
    # No decoder is named: ffprobe 8.0, which the runner has, has no option for it (8.1 has -c:v), and
    # libdav1d is the AV1 decoder FFmpeg takes when it has it (the job checks that it has). A decoder that showed no side data
    # would fail on the stream that is known to have it.
    code, out, err = run([ffprobe, '-v', 'error', '-select_streams', 'v:0', '-show_frames', '-of', 'json', path])
    if code != 0:
        raise CannotCheck('%s on %s: exit code %d: %s' % (ffprobe, path, code, err.strip()[-300:]))
    frames = []
    for frame in json.loads(out).get('frames', []):
        colour = (PRIMARIES.get(frame.get('color_primaries', 'unknown')),
                  TRANSFER.get(frame.get('color_transfer', 'unknown')),
                  MATRIX.get(frame.get('color_space', 'unknown')),
                  RANGE.get(frame.get('color_range', 'tv')),
                  CHROMA.get(frame.get('chroma_location', 'unspecified')))
        mdcv = cll = None
        for side in frame.get('side_data_list', []):
            kind = side.get('side_data_type', '')
            if kind == 'Mastering display metadata':
                mdcv = tuple(numerator(side[key], 65536) for key in
                             ('red_x', 'red_y', 'green_x', 'green_y', 'blue_x', 'blue_y', 'white_point_x',
                              'white_point_y')) + (numerator(side['max_luminance'], 256),
                                                   numerator(side['min_luminance'], 16384))
            elif kind == 'Content light level metadata':
                cll = (int(side['max_content']), int(side['max_average']))
        frames.append((colour, mdcv, cll))
    if not frames:
        raise CannotCheck('%s shows no frame of %s' % (ffprobe, path))
    return frames


def changes(frames):
    """The frames at which the values are not those of the frame before: [(index, values)]."""
    out = []
    for index, values in enumerate(frames):
        if not out or out[-1][1] != values:
            out.append((index, values))
    return out


def same_colour(decoder, reference):
    """A description of the decoder, without its flag, against ffprobe's, where ffprobe has a name."""
    return all(want is None or have == want for have, want in zip(decoder, reference))


def text(present, colour, mdcv, cll):
    words = 'primaries %s transfer %s matrix %s range %s chroma pos %s' % (
        colour[0], colour[1], colour[2], 'full' if colour[3] else 'limited', colour[4])
    if present is not None:
        words += ', description present' if present else ', no description'
    words += '; MDCV %s' % ' '.join(str(n) for n in mdcv) if mdcv else '; no MDCV'
    words += '; CLL %d/%d' % cll if cll else '; no CLL'
    return words


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--logs', action='append', required=True, metavar='NAME=DIR')
    parser.add_argument('--out', required=True)
    parser.add_argument('--ffmpeg', default='ffmpeg')
    parser.add_argument('--ffprobe', default='ffprobe')
    parser.add_argument('--only', default='', help='a regular expression: the streams to check (the known '
                        'streams are then not required)')
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)

    runs = []
    for item in args.logs:
        name, _, directory = item.partition('=')
        if not directory or not os.path.isdir(directory):
            print('colour_check: --logs %s: no such directory' % item)
            return 2
        runs.append((name, directory))
    streams = []
    with open(os.path.join(args.streams, 'manifest.txt'), encoding='utf-8') as f:
        for line in f:
            words = line.split()
            if '-name' in words and '-i' in words:
                name = words[words.index('-name') + 1]
                if not args.only or re.search(args.only, name):
                    streams.append((name, words[words.index('-i') + 1]))
    if not streams:
        print('colour_check: no stream in the manifest')
        return 2

    failures = []
    reference_rows = []
    kinds = {}
    shown = {}
    try:
        for name, relative in streams:
            path = os.path.join(args.streams, relative)
            sequence = traced_sequence(args.ffmpeg, path)
            frames = probed_frames(args.ffprobe, path)
            expected = changes(frames)
            reference_rows.append('%s\t%s\t%s' % (name, ' '.join(str(n) for n in sequence), ' | '.join(
                'frame %d: %s' % (index, text(None, *values)) for index, values in expected)))

            # The two references against each other, and against what the stream is known to carry.
            if not same_colour(sequence[1:], frames[0][0]):
                failures.append('%s: the references disagree: trace_headers %s, ffprobe %s'
                                % (name, sequence[1:], frames[0][0]))
            if name in KNOWN:
                known = KNOWN[name]
                if sequence != known[:6] or frames[0][1] != known[6] or frames[0][2] != known[7]:
                    failures.append('%s: the references say "%s", the stream is known to carry "%s"' % (
                        name, text(sequence[0], sequence[1:], frames[0][1], frames[0][2]),
                        text(known[0], known[1:6], known[6], known[7])))
            kind = text(sequence[0], sequence[1:], frames[0][1], frames[0][2])
            kinds[kind] = kinds.get(kind, 0) + 1

            for run_name, directory in runs:
                where = '%s, %s' % (name, run_name)
                log = os.path.join(directory, name + '.log')
                if not os.path.isfile(log):
                    failures.append('%s: no log' % where)
                    continue
                sequences, lines, bad = decoder_lines(log)
                for line in bad:
                    failures.append('%s: a COLOUR line that is not understood: %s' % (where, line))
                if not sequences:
                    failures.append('%s: no "sequence" line' % where)
                elif sequences[0] != sequence:
                    failures.append('%s: the sequence header: decoder %s, trace_headers %s'
                                    % (where, sequences[0], sequence))
                # rav1g-dec prints the line again with the first frame of a decoder it creates in the
                # middle of a stream (a sequence with larger frames): a line that says what the line before
                # it said is that, and no change.
                lines = [line for at, line in enumerate(lines) if at == 0 or line[1:] != lines[at - 1][1:]]
                if len(lines) != len(expected):
                    failures.append('%s: %d "frame" lines (frames %s), the frames change %d times in ffprobe '
                                    '(frames %s)' % (where, len(lines), [line[0] for line in lines], len(expected),
                                                     [index for index, _ in expected]))
                    continue
                for (frame, colour, mdcv, cll), (index, (ref_colour, ref_mdcv, ref_cll)) in zip(lines, expected):
                    if frame != index or not same_colour(colour[1:], ref_colour) or mdcv != ref_mdcv \
                            or cll != ref_cll:
                        failures.append('%s: decoder frame %d "%s", ffprobe frame %d "%s"' % (
                            where, frame, text(colour[0], colour[1:], mdcv, cll), index,
                            text(None, ref_colour, ref_mdcv, ref_cll)))
                    # A frame has the description of its sequence header: the flag, which ffprobe does not
                    # show, and the code points as numbers.
                    if frame == 0 and colour != sequence:
                        failures.append('%s: frame 0 has the description %s, the sequence header %s'
                                        % (where, colour, sequence))
                if name in KNOWN and lines:
                    known = KNOWN[name]
                    frame, colour, mdcv, cll = lines[0]
                    if colour != known[:6] or mdcv != known[6] or cll != known[7]:
                        failures.append('%s: the decoder says "%s", the stream is known to carry "%s"' % (
                            where, text(colour[0], colour[1:], mdcv, cll),
                            text(known[0], known[1:6], known[6], known[7])))
                    shown.setdefault(name, []).append((run_name, text(colour[0], colour[1:], mdcv, cll)))
    except CannotCheck as error:
        print('colour_check: %s' % error)
        return 2
    if not args.only:
        for name in KNOWN:
            if name not in dict(streams):
                failures.append('%s: not in the manifest, and the check needs it' % name)

    with open(os.path.join(args.out, 'reference.tsv'), 'w', encoding='utf-8', newline='\n') as f:
        f.write('\n'.join(reference_rows) + '\n')
    summary = ['## Colour description and HDR metadata', '',
               '%d streams, %d runs of the decoder (%s), against FFmpeg\'s `trace_headers` (the sequence header) '
               'and `ffprobe` with libdav1d (every output frame): **%s**.' % (
                   len(streams), len(runs), ', '.join(name for name, _ in runs),
                   'equal' if not failures else '%d differences' % len(failures)), '',
               '| Streams | What the references say (first sequence header, first frame) |', '| --- | --- |']
    summary += ['| %d | %s |' % (count, kind) for kind, count in sorted(kinds.items(), key=lambda item: -item[1])]
    if shown:
        summary += ['', '| Stream | Run | The decoder |', '| --- | --- | --- |']
        summary += ['| `%s` | %s | %s |' % (name, run_name, line) for name in shown for run_name, line in shown[name]]
    if failures:
        summary += ['', '```'] + failures[:60] + ['```']
    summary_text = '\n'.join(summary) + '\n'
    with open(os.path.join(args.out, 'summary.md'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(summary_text)
    step_summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if step_summary:
        with open(step_summary, 'a', encoding='utf-8', newline='\n') as f:
            f.write(summary_text)
    for failure in failures[:60]:
        print('DIFFERENT ' + failure)
    for kind, count in sorted(kinds.items(), key=lambda item: -item[1]):
        print('%4d streams: %s' % (count, kind))
    print('colour_check: %d streams, %d runs: %s' % (
        len(streams), len(runs), 'equal' if not failures else '%d differences' % len(failures)))
    return 1 if failures else 0


if __name__ == '__main__':
    sys.exit(main())
