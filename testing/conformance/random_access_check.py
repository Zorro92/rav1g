"""A start in the middle of a stream (AV1 7.6, random access): the decoder skips the frames whose
references it never decoded, without an error, and every frame it puts out is right.

Cuts streams of the conformance set at a temporal unit in the middle and puts the stream's sequence
header in front of it when that unit has none (what an application does when it starts there and gives
the decoder the codec data of its container first, Matroska's av1C configOBUs). The cut points, the
first of each kind in a stream:

- intra_only: a temporal unit that starts with a hidden intra-only frame (the refresh point of an open
  GOP: SVT-AV1 with irefresh-type=1);
- hidden_key: one that starts with a key frame that is not shown (a delayed random access point, 7.6.2;
  none of the streams of the set has one, Argon's streams that start with one are decoded whole by the
  Argon job);
- show_existing: one that starts with a show_existing_frame (of a slot no frame has been decoded into);
- inter: one that starts with an inter frame;
- key: one that starts with a shown key frame (a key frame random access point: nothing is skipped).

Each cut that has no sequence header of its own, and one later, is also decoded as it is, without one in
front: its frame headers up to that sequence header cannot be read and are skipped too (a decoder starts
at a temporal unit with a sequence header, 7.5).

What the decoder has to put out is worked out here, independently of the decoder, from the headers: the
spec's RefValid (6.8.2, 7.20) of the eight reference slots, none valid at the start, and a frame is
decoded when it is intra, or when the slots it names (ref_frame_idx, all eight with
frame_refs_short_signaling) are valid; the slots a skipped frame would have refreshed are not valid from
there on, a show_existing_frame of a slot that is not valid shows nothing, and one of a key frame makes
every slot valid (7.21). The frames a decoded frame shows are right: their md5 is that of the same
temporal unit in the reference of the whole stream (one frame a temporal unit). Checked, each cut a task
of rav1g-dec with -anystart (the decoder gets every temporal unit, not only those from the first key
frame on) and -save_md5 (no reference: a cut may put out nothing):

- the task ends without a decode error (status nocheck);
- it puts out exactly the frames worked out here, in order, each with the md5 of its temporal unit in
  the reference of the whole stream;
- the decoder says how many frames it skipped (the log line SKIPPED, which rav1g-dec prints);
- no task loses the D3D12 device.

With --frames-in-parse N the tasks get -parseframes N -t N (more than one frame in parse: a skipped
frame takes no frame context and nothing waits for it).

usage: python random_access_check.py --streams <conformance streams> --out <directory>
           --tool <rav1g-dec.exe> [--tool-args "<args>"] [--frames-in-parse N]
       python random_access_check.py --model <stream.ivf> [--start N]
           (prints what the model says of every temporal unit: no decoder)

The cut streams are written into <streams>/racut and removed at the end; --out gets logs and md5 lists
only, no stream.

Exit code: 0 everything as it should be, 1 a check failed, 2 the check could not be done.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cut_tile  # noqa: E402

# The streams of the set that are cut, and the kinds of cut point each must have.
CASES = [
    # A key frame every 16 frames (a key frame random access point with its sequence header), hidden
    # frames, show_existing_frame every other temporal unit.
    ('svt/syn-tiles-10b-1080p.ivf', ('show_existing', 'inter', 'key')),
    # Open GOP: hidden intra-only refresh points, no key frame after the first.
    ('svt/syn-opengop-10b-1080p.ivf', ('intra_only', 'show_existing', 'inter')),
]
KINDS = ('intra_only', 'hidden_key', 'show_existing', 'inter', 'key')
WORK = 'racut'
DEVICE_REMOVED = 'the D3D12 device was removed'

OBU_SEQUENCE_HEADER = 1
OBU_TEMPORAL_DELIMITER = 2
OBU_FRAME_HEADER = 3
OBU_FRAME = 6

KEY_FRAME, INTER_FRAME, INTRA_ONLY_FRAME, SWITCH_FRAME = range(4)
SELECT = 2
ALL_FRAMES = 0xff


class Failed(Exception):
    pass


class Bits:
    """f(n) of the specification (4.10.2), most significant bit first."""

    def __init__(self, data):
        self.data = data
        self.pos = 0

    def f(self, n):
        value = 0
        for _ in range(n):
            byte = self.pos >> 3
            if byte >= len(self.data):
                raise cut_tile.Bad('a header goes past the end of its OBU')
            value = (value << 1) | ((self.data[byte] >> (7 - (self.pos & 7))) & 1)
            self.pos += 1
        return value

    def uvlc(self):
        zeros = 0
        while not self.f(1):
            zeros += 1
            if zeros >= 32:
                raise cut_tile.Bad('a uvlc of 32 leading zeros')
        return self.f(zeros) + (1 << zeros) - 1


def sequence_header(payload):
    """The fields of sequence_header_obu (5.5) that the frame header reads up to ref_frame_idx."""
    b = Bits(payload)
    s = {}
    b.f(3)  # seq_profile
    b.f(1)  # still_picture
    s['reduced'] = b.f(1)
    s['decoder_model_info'] = 0
    s['equal_picture_interval'] = 0
    s['op_idc'] = [0]
    s['op_model'] = [0]
    if s['reduced']:
        b.f(5)  # seq_level_idx[0]
    else:
        if b.f(1):  # timing_info_present_flag
            b.f(32)  # num_units_in_display_tick
            b.f(32)  # time_scale
            s['equal_picture_interval'] = b.f(1)
            if s['equal_picture_interval']:
                b.uvlc()  # num_ticks_per_picture_minus_1
            s['decoder_model_info'] = b.f(1)
            if s['decoder_model_info']:
                s['buffer_delay_length'] = b.f(5) + 1
                b.f(32)  # num_units_in_decoding_tick
                s['buffer_removal_time_length'] = b.f(5) + 1
                s['frame_presentation_time_length'] = b.f(5) + 1
        initial_display_delay = b.f(1)
        count = b.f(5) + 1
        s['op_idc'] = []
        s['op_model'] = []
        for _ in range(count):
            s['op_idc'].append(b.f(12))
            if b.f(5) > 7:  # seq_level_idx
                b.f(1)  # seq_tier
            model = 0
            if s['decoder_model_info']:
                model = b.f(1)
                if model:
                    b.f(s['buffer_delay_length'])  # decoder_buffer_delay
                    b.f(s['buffer_delay_length'])  # encoder_buffer_delay
                    b.f(1)  # low_delay_mode_flag
            s['op_model'].append(model)
            if initial_display_delay and b.f(1):
                b.f(4)  # initial_display_delay_minus_1
    width_bits = b.f(4) + 1
    height_bits = b.f(4) + 1
    b.f(width_bits)
    b.f(height_bits)
    s['frame_id'] = 0 if s['reduced'] else b.f(1)
    if s['frame_id']:
        s['delta_frame_id_length'] = b.f(4) + 2
        s['frame_id_length'] = b.f(3) + 1 + s['delta_frame_id_length']
    b.f(3)  # use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    s['order_hint_bits'] = 0
    s['force_screen_content_tools'] = SELECT
    s['force_integer_mv'] = SELECT
    if not s['reduced']:
        b.f(4)  # enable_interintra_compound, enable_masked_compound, enable_warped_motion, enable_dual_filter
        order_hint = b.f(1)
        if order_hint:
            b.f(2)  # enable_jnt_comp, enable_ref_frame_mvs
        s['force_screen_content_tools'] = SELECT if b.f(1) else b.f(1)
        if s['force_screen_content_tools'] > 0:
            s['force_integer_mv'] = SELECT if b.f(1) else b.f(1)
        if order_hint:
            s['order_hint_bits'] = b.f(3) + 1
    return s


def frame_header(s, payload, temporal_id, spatial_id):
    """uncompressed_header (5.9.2) up to ref_frame_idx: what decides whether the frame can be decoded."""
    b = Bits(payload)
    h = {'show_existing': 0, 'refresh': 0, 'refidx': None, 'short': 0}
    if s['reduced']:
        h.update(frame_type=KEY_FRAME, show_frame=1, refresh=ALL_FRAMES)
        return h
    h['show_existing'] = b.f(1)
    if h['show_existing']:
        h['slot'] = b.f(3)
        return h
    h['frame_type'] = b.f(2)
    intra = h['frame_type'] in (KEY_FRAME, INTRA_ONLY_FRAME)
    h['show_frame'] = b.f(1)
    if h['show_frame'] and s['decoder_model_info'] and not s['equal_picture_interval']:
        b.f(s['frame_presentation_time_length'])
    if not h['show_frame']:
        b.f(1)  # showable_frame
    if h['frame_type'] == SWITCH_FRAME or (h['frame_type'] == KEY_FRAME and h['show_frame']):
        error_resilient = 1
    else:
        error_resilient = b.f(1)
    b.f(1)  # disable_cdf_update
    screen_content = b.f(1) if s['force_screen_content_tools'] == SELECT else s['force_screen_content_tools']
    if screen_content and s['force_integer_mv'] == SELECT:
        b.f(1)  # force_integer_mv
    if s['frame_id']:
        b.f(s['frame_id_length'])  # current_frame_id
    if h['frame_type'] != SWITCH_FRAME:
        b.f(1)  # frame_size_override_flag
    b.f(s['order_hint_bits'])  # order_hint
    if not intra and not error_resilient:
        b.f(3)  # primary_ref_frame
    if s['decoder_model_info']:
        if b.f(1):  # buffer_removal_time_present_flag
            for idc, model in zip(s['op_idc'], s['op_model']):
                if model and (idc == 0 or ((idc >> temporal_id) & 1 and (idc >> (spatial_id + 8)) & 1)):
                    b.f(s['buffer_removal_time_length'])
    if h['frame_type'] == SWITCH_FRAME or (h['frame_type'] == KEY_FRAME and h['show_frame']):
        h['refresh'] = ALL_FRAMES
    else:
        h['refresh'] = b.f(8)
    if (not intra or h['refresh'] != ALL_FRAMES) and error_resilient and s['order_hint_bits']:
        for _ in range(8):
            b.f(s['order_hint_bits'])  # ref_order_hint
    if intra:
        return h
    if s['order_hint_bits']:
        h['short'] = b.f(1)
    if h['short']:
        # The references follow from the order hints of all eight slots (7.8): which ones does not
        # matter here, every slot has to be valid.
        return h
    h['refidx'] = []
    for _ in range(7):
        h['refidx'].append(b.f(3))
        if s['frame_id']:
            b.f(s['delta_frame_id_length'])  # delta_frame_id_minus_1
    return h


def units_of(path):
    """(the IVF header, [(pts, [(type, OBU header, payload)])])"""
    header, units = cut_tile.read_ivf(path)
    return header, [(pts, cut_tile.split_obus(data)) for pts, data in units]


def ids(head):
    """temporal_id, spatial_id of an OBU header (0, 0 without an extension)."""
    if len(head) < 2:
        return 0, 0
    return head[1] >> 5, (head[1] >> 3) & 3


def first_frame(obus, seq):
    """The kind of cut point the first frame header of a temporal unit makes, or None."""
    for obu_type, head, payload in obus:
        if obu_type == OBU_SEQUENCE_HEADER:
            seq = sequence_header(payload)
        if obu_type in (OBU_FRAME_HEADER, OBU_FRAME) and seq is not None:
            h = frame_header(seq, payload, *ids(head))
            if h['show_existing']:
                return 'show_existing'
            if h['frame_type'] == INTRA_ONLY_FRAME and not h['show_frame']:
                return 'intra_only'
            if h['frame_type'] == KEY_FRAME:
                return 'key' if h['show_frame'] else 'hidden_key'
            if h['frame_type'] == INTER_FRAME:
                return 'inter'
            return None
    return None


def model(units, seq_bytes=None):
    """For every temporal unit: whether the decoder puts a frame out for it, and the frame headers it
    skips there. RefValid of the eight slots (6.8.2, 7.20), none valid at the start. seq_bytes: the
    payload of the sequence header the decoder has at the start (one a flush kept), or None."""
    valid = 0
    key_in = 0  # the slots that hold a key frame
    seq = None if seq_bytes is None else sequence_header(seq_bytes)
    out = []
    for _, obus in units:
        shown = False
        skipped = 0
        for obu_type, head, payload in obus:
            if obu_type == OBU_SEQUENCE_HEADER:
                if seq_bytes is not None and payload != seq_bytes:
                    # A sequence header that differs starts a new video sequence: no slot holds a frame of it.
                    valid = key_in = 0
                seq = sequence_header(payload)
                seq_bytes = payload
                continue
            if obu_type not in (OBU_FRAME_HEADER, OBU_FRAME):
                continue
            if seq is None:
                skipped += 1
                continue
            h = frame_header(seq, payload, *ids(head))
            if h['show_existing']:
                slot = 1 << h['slot']
                if not valid & slot:
                    skipped += 1
                    continue
                shown = True
                if key_in & slot:
                    valid, key_in = ALL_FRAMES, ALL_FRAMES
                continue
            if h['frame_type'] in (KEY_FRAME, INTRA_ONLY_FRAME):
                decoded = True
            elif h['short']:
                decoded = valid == ALL_FRAMES
            else:
                decoded = all(valid & (1 << slot) for slot in h['refidx'])
            if decoded:
                valid |= h['refresh']
                if h['frame_type'] == KEY_FRAME:
                    key_in |= h['refresh']
                else:
                    key_in &= ~h['refresh']
                shown = shown or bool(h['show_frame'])
            else:
                skipped += 1
                valid &= ~h['refresh']
                key_in &= ~h['refresh']
        out.append((shown, skipped))
    return out


def cut(units, start):
    """The temporal units from start on, with the last sequence header before them put in front (behind
    the temporal delimiter) when the first of them has none."""
    obus = units[start][1]
    if any(obu_type == OBU_SEQUENCE_HEADER for obu_type, _, _ in obus):
        return units[start:]
    header = None
    for _, earlier in units[:start]:
        for obu in earlier:
            if obu[0] == OBU_SEQUENCE_HEADER:
                header = obu
    if header is None:
        raise Failed('no sequence header before temporal unit %d' % start)
    at = 1 if obus and obus[0][0] == OBU_TEMPORAL_DELIMITER else 0
    return [(units[start][0], obus[:at] + [header] + obus[at:])] + units[start + 1:]


def write_units(path, header, units):
    cut_tile.write_ivf(path, header, [
        (pts, b''.join(head + cut_tile.write_leb128(len(payload)) + payload for _, head, payload in obus))
        for pts, obus in units])


def md5_list(path):
    out = []
    with open(path, 'rb') as f:
        for line in f.read().decode('ascii', 'replace').splitlines():
            if re.match(r'^[0-9a-fA-F]{32}', line):
                out.append(line[:32].lower())
    return out


def prepare(streams):
    """Writes the cut streams; returns the cases: name, cut, the md5 list the decoder has to put out."""
    work = os.path.join(streams, WORK)
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    cases = []
    for source, kinds in CASES:
        path = os.path.join(streams, source)
        if not os.path.isfile(path) or not os.path.isfile(path + '.md5'):
            raise Failed('%s or its .md5 is not in the stream set' % source)
        reference = md5_list(path + '.md5')
        header, units = units_of(path)
        if any(len(head) > 1 for _, obus in units for _, head, _ in obus):
            raise Failed('%s has OBU extension headers: one frame a temporal unit is what the check needs' % source)
        whole = model(units)
        if len(reference) != len(units) or not all(shown for shown, _ in whole) or any(n for _, n in whole):
            raise Failed('%s: %d temporal units, %d frames in its reference, the model puts out %d and skips %d: '
                         'the whole stream has to put out one frame a temporal unit' % (
                             source, len(units), len(reference), sum(shown for shown, _ in whole),
                             sum(n for _, n in whole)))
        points = {}
        seq = None
        for index, (_, obus) in enumerate(units):
            for obu_type, _, payload in obus:
                if obu_type == OBU_SEQUENCE_HEADER:
                    seq = sequence_header(payload)
            kind = first_frame(obus, seq) if index else None
            if kind and kind not in points:
                points[kind] = index
        missing = [kind for kind in kinds if kind not in points]
        if missing:
            raise Failed('%s has no temporal unit that starts with %s (it has %s)' % (
                source, ', '.join(missing), ', '.join('%s at %d' % item for item in sorted(points.items()))))
        stem = os.path.splitext(os.path.basename(source))[0]
        for kind in kinds:
            start = points[kind]
            variants = [('', cut(units, start))]
            # Without one in front, when a later temporal unit has one (rav1g-dec creates the decoder for
            # the first sequence header it finds).
            later = [obu_type for _, obus in units[start + 1:] for obu_type, _, _ in obus]
            if not any(obu_type == OBU_SEQUENCE_HEADER for obu_type, _, _ in units[start][1]) and \
                    OBU_SEQUENCE_HEADER in later:
                variants.append(('-noseq', units[start:]))
            for variant, part in variants:
                said = model(part)
                expected = [reference[start + i] for i, (shown, _) in enumerate(said) if shown]
                skipped = sum(n for _, n in said)
                first = next((i for i, (shown, _) in enumerate(said) if shown), None)
                name = '%s-%s%s' % (stem, kind, variant)
                path_cut = '%s/%s.ivf' % (WORK, name)
                write_units(os.path.join(streams, path_cut), header, part)
                print('%s: %s cut at temporal unit %d (%s%s) of %d: %d frames out, the first at temporal unit %s; '
                      '%d frame headers skipped' % (
                          name, source, start, kind, ', no sequence header in front' if variant else '', len(units),
                          len(expected), 'none' if first is None else start + first, skipped))
                cases.append({'name': name, 'cut': path_cut, 'expected': expected, 'skipped': skipped})
    return cases


def run_tool(args, cases, problems):
    several = args.frames_in_parse > 1
    suffix = ('-' + re.sub(r'\W+', '', args.tool_args) if args.tool_args else '') + \
        ('-frames%d' % args.frames_in_parse if several else '')
    out = os.path.join(args.out, 'tool' + suffix)
    os.makedirs(out, exist_ok=True)
    threads = '-parseframes %d -t %d' % (args.frames_in_parse, args.frames_in_parse) if several else '-t 1'
    tasks = ['-name %s -i %s -anystart -save_md5 %s/%s.md5 %s -timeout 300' % (
        case['name'], case['cut'], WORK, case['name'], threads) for case in cases]
    with open(os.path.join(args.streams, WORK, 'tool.list'), 'w', encoding='ascii') as f:
        f.write('\n'.join(tasks) + '\n')
    command = [os.path.abspath(args.tool), '-keep_going', '-list', '%s/tool.list' % WORK] + args.tool_args.split()
    try:
        done = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=1800)
    except subprocess.TimeoutExpired:
        raise Failed('%s did not end within 30 minutes' % args.tool)
    text = done.stdout.decode('utf-8', 'replace')
    with open(os.path.join(out, 'tool.log'), 'w', encoding='utf-8') as f:
        f.write(text)
    label = os.path.basename(args.tool) + (' ' + args.tool_args if args.tool_args else '') + \
        (' -parseframes %d' % args.frames_in_parse if several else '')
    if several and not re.search(r'^CONFIGURATION .*, frames in parse %d ' % args.frames_in_parse, text, re.M):
        problems.append('%s: no decoder of the run says that it has %d frames in parse' % (label, args.frames_in_parse))
    if DEVICE_REMOVED in text:
        problems.append('%s: a task lost the D3D12 device' % label)
    if done.returncode != 0:
        problems.append('%s: exit code %d, 0 was expected (no decode error)' % (label, done.returncode))
    results = {}
    current = None
    skipped = {}
    for line in text.splitlines():
        found = re.match(r'RESULT stream=(\S+) status=(\S+) frames=(\d+) .* detail="(.*)"\s*$', line)
        if found:
            results[found.group(1)] = (found.group(2), int(found.group(3)), found.group(4))
            current = None
            continue
        started = re.match(r'^BEGIN stream=(\S+)', line)
        if started:
            current = started.group(1)
        counted = re.match(r'^SKIPPED (\d+) frames', line)
        if counted and current:
            # The lines of a task come between its BEGIN and its RESULT.
            skipped[current] = skipped.get(current, 0) + int(counted.group(1))
    for case in cases:
        name = case['name']
        status, frames, detail = results.get(name, ('none', 0, ''))
        if status != 'nocheck':
            problems.append('%s: %s ended as %s (%s); a decode without an error was expected' % (
                label, name, status, detail))
            continue
        saved = os.path.join(args.streams, WORK, name + '.md5')
        got = md5_list(saved) if os.path.isfile(saved) else []
        if os.path.isfile(saved):
            shutil.copyfile(saved, os.path.join(out, name + '.md5'))
        if got != case['expected']:
            first = next((i for i, (a, b) in enumerate(zip(got, case['expected'])) if a != b),
                         min(len(got), len(case['expected'])))
            problems.append('%s: %s: %d frames out, %d expected; the first that differs is output frame %d' % (
                label, name, len(got), len(case['expected']), first))
            continue
        said = skipped.get(name, 0)
        if said != case['skipped']:
            problems.append('%s: %s: the decoder says it skipped %d frames (SKIPPED lines), %d were to be skipped' % (
                label, name, said, case['skipped']))
            continue
        print('%s: %s: no error, the %d frames out are those of the whole stream; %d frame headers skipped, as the '
              'decoder says' % (label, name, len(got), case['skipped']))
    print('%s: D3D12 DEVICE REMOVALS: %d' % (label, text.count(DEVICE_REMOVED)))


def print_model(path, start):
    _, units = units_of(path)
    part = cut(units, start) if start else units
    for index, (shown, skipped) in enumerate(model(part)):
        print('%d %s%s' % (start + index, 'out' if shown else '-', ' skipped %d' % skipped if skipped else ''))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--streams', help='the directory of the conformance streams')
    parser.add_argument('--out', help='logs and md5 lists (no stream)')
    parser.add_argument('--tool', help='rav1g-dec.exe')
    parser.add_argument('--tool-args', default='', help='further arguments for it, e.g. "-capi"')
    parser.add_argument('--frames-in-parse', type=int, default=1,
                        help='the tasks get -parseframes N -t N: more than one frame in parse')
    parser.add_argument('--model', help='an IVF stream: print what the model says of it, decode nothing')
    parser.add_argument('--start', type=int, default=0, help='with --model: cut the stream here first')
    args = parser.parse_args()
    if args.model:
        print_model(args.model, args.start)
        return 0
    if not (args.streams and args.out and args.tool):
        parser.error('--streams, --out and --tool (or --model)')
    os.makedirs(args.out, exist_ok=True)
    problems = []
    try:
        try:
            cases = prepare(args.streams)
            run_tool(args, cases, problems)
        finally:
            shutil.rmtree(os.path.join(args.streams, WORK), ignore_errors=True)
    except (Failed, cut_tile.Bad, OSError) as error:
        print('random_access_check.py: %s' % error)
        return 2
    for problem in problems:
        print('FAILED %s' % problem)
    summary = 'random_access_check.py: %s' % (
        '%d checks failed' % len(problems) if problems else
        'a start in the middle of a stream skips the frames without references, with no error, and the frames '
        'out are right')
    print(summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n- %s%s\n' % (summary, ''.join('\n  - ' + problem for problem in problems)))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
