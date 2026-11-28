"""A seek inside one decoder (rav1g_decoder_flush, interface version 4; AV1 7.5 and 7.6, random access;
7.20, the reference frame update): after a flush the decoder puts out exactly what a new decoder started
at the same point puts out, and nothing of before.

Each case is a stream that jumps: the first N temporal units of a stream of the conformance set, then
the units from a cut point M on (forward, N < M, or backward, N > M), and rav1g-dec flushes the decoder
before the unit N (-flush N) as an application that seeks does. The cut points, from the streams of
random_access_check.py: a shown key frame, the hidden intra-only refresh point of an open GOP, a
show_existing_frame and an inter frame (a hidden key frame, 7.6.2, where the stream has one). The
variants:

- inflight: the flush with frames in parse and on the GPU, nothing drained;
- seq: the same with the sequence header put in front of the unit M (the container's configOBUs passed
  again); without it the decoder goes on with the sequence header the flush kept;
- fork: a drain first, and every image refused from the start of the drain until the flush has returned
  (-flushdrain -flushrefuse: Bale's fork, whose get_image returns non-zero while it seeks);
- refuse: the images refused during the flush only, no drain;
- back: a backward seek with frames in flight;
- twice: two flushes in a row (-flush N,N).

What is checked, each case a task of rav1g-dec with -save_md5:

- no decode error (status nocheck), no lost device;
- the frames out before the flush (the FLUSH line says how many) are the first frames of the stream, at
  most N; a frame given before a flush that comes out behind it is a fault of the task;
- the frames out after it are exactly the ones a decoder that starts at M with the sequence header puts
  out (the model of random_access_check.py: RefValid of the eight slots none at the start, and a frame is
  decoded when its references are), each with the md5 of its temporal unit in the reference of the whole
  stream, and the decoder says it skipped as many frames after the flush (SKIPPED lines) as the model;
- every image the decoder took came back (rav1g-dec checks it at the end of a task with -flush);
- a new decoder on the cut stream (-anystart) puts out the same frames: the flushed decoder is one that
  starts there.

Two more tasks: the whole stream with -flush 0,0,<end> (a flush right after the creation, twice, and one
behind the last unit with frames in flight: the frames out are a prefix of the stream), and, with one
frame in parse, images refused for three temporal units without a flush (-refuse, -continue_on_error):
those frames are decoded and not output, each such call returns "Failed to allocate output frame.", and
the frames that follow are right (a refused image costs the output of its frame, not its references).

With --frames-in-parse N the tasks get -parseframes N -t N.

usage: python flush_check.py --streams <conformance streams> --out <directory> --tool <rav1g-dec.exe>
           [--tool-args "<args>"] [--frames-in-parse N]

The streams are written into <streams>/flushcut and removed at the end; --out gets logs and md5 lists
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
import random_access_check as rac  # noqa: E402

# The streams and the cut points each must have (random_access_check.CASES).
CASES = rac.CASES
WORK = 'flushcut'
DEVICE_REMOVED = rac.DEVICE_REMOVED
# The cut point: the first of its kind at or after this unit, so that frames are in flight at a flush
# before it; the units of a case behind the cut point.
FROM = 8
POST = 16
# Every cut point gets the fork's case and one of the others, in turn (each on at least one cut point).
OTHERS = ('inflight', 'seq', 'refuse', 'back', 'twice')
FLAGS = {'inflight': '', 'seq': '', 'fork': '-flushdrain -flushrefuse', 'refuse': '-flushrefuse', 'back': '',
         'twice': ''}
# -refuse: the units whose images are refused (one frame in parse: those units' frames).
REFUSED = (4, 9, 16)


class Failed(Exception):
    pass


def last_sequence(units, end):
    """The payload of the last sequence header in the units before end."""
    payload = None
    for _, obus in units[:end]:
        for obu_type, _, data in obus:
            if obu_type == rac.OBU_SEQUENCE_HEADER:
                payload = data
    return payload


def prepare(streams):
    work = os.path.join(streams, WORK)
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    cases = []
    turn = 0
    for source, kinds in CASES:
        path = os.path.join(streams, source)
        if not os.path.isfile(path) or not os.path.isfile(path + '.md5'):
            raise Failed('%s or its .md5 is not in the stream set' % source)
        reference = rac.md5_list(path + '.md5')
        header, units = rac.units_of(path)
        if len(reference) != len(units):
            raise Failed('%s: %d temporal units, %d frames in its reference: one a unit is what the check needs' % (
                source, len(units), len(reference)))
        stem = os.path.splitext(os.path.basename(source))[0]
        points = {}
        seq = None
        for index, (_, obus) in enumerate(units):
            for obu_type, _, payload in obus:
                if obu_type == rac.OBU_SEQUENCE_HEADER:
                    seq = rac.sequence_header(payload)
            kind = rac.first_frame(obus, seq) if index else None
            if kind and (kind not in points or (points[kind] < FROM and index >= FROM)):
                points[kind] = index
        missing = [kind for kind in kinds if kind not in points]
        if missing:
            raise Failed('%s has no temporal unit that starts with %s' % (source, ', '.join(missing)))
        # A hidden key frame where the stream has one (7.6.2).
        for kind in list(kinds) + (['hidden_key'] if 'hidden_key' in points and 'hidden_key' not in kinds else []):
            start = points[kind]
            for variant in ('fork', OTHERS[turn % len(OTHERS)]):
                at = min(len(units), start + 6) if variant == 'back' else max(1, start - 5)
                post = units[start:start + POST]
                if variant == 'seq':
                    post = rac.cut(units, start)[:POST]
                kept = last_sequence(units, at)
                said = rac.model(post, kept)
                expected = [reference[start + i] for i, (shown, _) in enumerate(said) if shown]
                skipped = sum(n for _, n in said)
                name = '%s-flush-%s-%s' % (stem, kind, variant)
                path_cut = '%s/%s.ivf' % (WORK, name)
                rac.write_units(os.path.join(streams, path_cut), header, units[:at] + post)
                flush = '%d,%d' % (at, at) if variant == 'twice' else '%d' % at
                print('%s: %s, units 0..%d then %d.. (%s), flush before unit %d: %d frames out after it, %d frame '
                      'headers skipped' % (name, source, at - 1, start, kind, at, len(expected), skipped))
                cases.append({'name': name, 'cut': path_cut, 'args': '-flush %s %s' % (flush, FLAGS[variant]),
                              'at': at, 'before': reference, 'expected': expected, 'skipped': skipped,
                              'flushes': 2 if variant == 'twice' else 1})
            turn += 1
            # A new decoder at the same point, with the sequence header in front.
            post = rac.cut(units, start)[:POST]
            said = rac.model(post)
            name = '%s-new-%s' % (stem, kind)
            path_cut = '%s/%s.ivf' % (WORK, name)
            rac.write_units(os.path.join(streams, path_cut), header, post)
            cases.append({'name': name, 'cut': path_cut, 'args': '-anystart', 'at': 0, 'before': [],
                          'expected': [reference[start + i] for i, (shown, _) in enumerate(said) if shown],
                          'skipped': sum(n for _, n in said), 'flushes': 0})
        # Flushes right after the creation, twice, and behind the last unit.
        name = '%s-flush-ends' % stem
        cases.append({'name': name, 'cut': source, 'args': '-flush 0,0,%d' % len(units), 'at': len(units),
                      'before': reference, 'expected': [], 'skipped': 0, 'flushes': 3})
        if source == CASES[0][0]:
            cases.append({'name': '%s-refuse' % stem, 'cut': source, 'refused': REFUSED,
                          'args': '-refuse %s -continue_on_error' % ','.join(map(str, REFUSED)),
                          'expected': [md5 for i, md5 in enumerate(reference) if i not in REFUSED]})
    return cases


def check_case(case, label, status, detail, text, got, problems):
    """The checks of one task; text: its lines between BEGIN and RESULT."""
    name = case['name']
    if status != 'nocheck':
        problems.append('%s: %s ended as %s (%s); a decode without an error was expected' % (
            label, name, status, detail))
        return
    if 'refused' in case:
        errors = re.findall(r'^CONTINUE \S+: decode error \d+ at temporal unit (\d+): (.*)$', text, re.M)
        units = sorted(int(unit) for unit, _ in errors)
        if units != sorted(case['refused']) or not all('Failed to allocate output frame' in why for _, why in errors):
            problems.append('%s: %s: the decode calls that failed are %s, %s with "Failed to allocate output frame" '
                            'were expected' % (label, name, errors, list(case['refused'])))
            return
        if got != case['expected']:
            problems.append('%s: %s: %d frames out, %d expected (the stream without the refused units); the first '
                            'that differs is output frame %d' % (
                                label, name, len(got), len(case['expected']), first_difference(got, case['expected'])))
            return
        print('%s: %s: the frames of the refused images are not out, every other frame is right' % (label, name))
        return
    flushes = re.findall(r'^FLUSH stream=\S+ before temporal unit (\d+): (\d+) frames out before it', text, re.M)
    if len(flushes) != case['flushes']:
        problems.append('%s: %s: %d FLUSH lines, %d flushes were asked for' % (label, name, len(flushes),
                                                                            case['flushes']))
        return
    out = int(flushes[-1][1]) if flushes else 0
    if out > case['at'] or got[:out] != case['before'][:out]:
        problems.append('%s: %s: of the %d frames out before the flush at unit %d, output frame %d is not the '
                        'stream\'s' % (label, name, out, case['at'], first_difference(got[:out], case['before'])))
        return
    after = got[out:]
    if after != case['expected']:
        problems.append('%s: %s: %d frames out after the flush, %d expected; the first that differs is output frame '
                        '%d after it' % (label, name, len(after), len(case['expected']),
                                         first_difference(after, case['expected'])))
        return
    tail = text[text.rfind('FLUSH stream='):] if flushes else text
    said = sum(int(n) for n in re.findall(r'^SKIPPED (\d+) frames', tail, re.M))
    if said != case['skipped']:
        problems.append('%s: %s: the decoder says it skipped %d frames after the flush (SKIPPED lines), %d were to '
                        'be skipped' % (label, name, said, case['skipped']))
        return
    print('%s: %s: %d frames out before the flush, the %d after it are those of a new decoder at the cut point; %d '
          'frame headers skipped, as the decoder says' % (label, name, out, len(after), said))


def first_difference(a, b):
    return next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))


def run_tool(args, cases, problems):
    several = args.frames_in_parse > 1
    suffix = ('-' + re.sub(r'\W+', '', args.tool_args) if args.tool_args else '') + \
        ('-frames%d' % args.frames_in_parse if several else '')
    out = os.path.join(args.out, 'tool' + suffix)
    os.makedirs(out, exist_ok=True)
    threads = '-parseframes %d -t %d' % (args.frames_in_parse, args.frames_in_parse) if several else '-t 1'
    # The refused images of -refuse are those of the decode calls of the units only with one frame in parse.
    run = [case for case in cases if 'refused' not in case or not several]
    tasks = ['-name %s -i %s %s -save_md5 %s/%s.md5 %s -timeout 300' % (
        case['name'], case['cut'], case['args'], WORK, case['name'], threads) for case in run]
    with open(os.path.join(args.streams, WORK, 'tool.list'), 'w', encoding='ascii') as f:
        f.write('\n'.join(tasks) + '\n')
    command = [os.path.abspath(args.tool), '-keep_going', '-list', '%s/tool.list' % WORK] + args.tool_args.split()
    try:
        done = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=3600)
    except subprocess.TimeoutExpired:
        raise Failed('%s did not end within 60 minutes' % args.tool)
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
    # The lines of a task come between its BEGIN and its RESULT.
    blocks = {}
    current, lines = None, []
    for line in text.splitlines():
        started = re.match(r'^BEGIN stream=(\S+)', line)
        if started:
            current, lines = started.group(1), []
            continue
        found = re.match(r'^RESULT stream=(\S+) status=(\S+) .* detail="(.*)"\s*$', line)
        if found:
            if found.group(1) == current:
                blocks[current] = (found.group(2), found.group(3), '\n'.join(lines) + '\n')
            current = None
            continue
        if current:
            lines.append(line.rstrip('\r'))
    for case in run:
        status, detail, lines = blocks.get(case['name'], ('none', '', ''))
        saved = os.path.join(args.streams, WORK, case['name'] + '.md5')
        got = rac.md5_list(saved) if os.path.isfile(saved) else []
        if os.path.isfile(saved):
            shutil.copyfile(saved, os.path.join(out, case['name'] + '.md5'))
        check_case(case, label, status, detail, lines, got, problems)
    times = [float(ms) for ms in re.findall(r'^FLUSH stream=.* ([0-9.]+) ms', text, re.M)]
    if times:
        print('%s: %d flushes, %.3f ms at most, %.3f ms on average (CPU time of the call on this machine)' % (
            label, len(times), max(times), sum(times) / len(times)))
    print('%s: D3D12 DEVICE REMOVALS: %d' % (label, text.count(DEVICE_REMOVED)))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--streams', required=True, help='the directory of the conformance streams')
    parser.add_argument('--out', required=True, help='logs and md5 lists (no stream)')
    parser.add_argument('--tool', required=True, help='rav1g-dec.exe')
    parser.add_argument('--tool-args', default='', help='further arguments for it, e.g. "-capi"')
    parser.add_argument('--frames-in-parse', type=int, default=1,
                        help='the tasks get -parseframes N -t N: more than one frame in parse')
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)
    problems = []
    try:
        try:
            cases = prepare(args.streams)
            run_tool(args, cases, problems)
        finally:
            shutil.rmtree(os.path.join(args.streams, WORK), ignore_errors=True)
    except (Failed, rac.Failed, cut_tile.Bad, OSError) as error:
        print('flush_check.py: %s' % error)
        return 2
    for problem in problems:
        print('FAILED %s' % problem)
    summary = 'flush_check.py: %s' % (
        '%d checks failed' % len(problems) if problems else
        'after a flush the decoder puts out what a new decoder at the cut point does, and nothing of before')
    print(summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n- %s%s\n' % (summary, ''.join('\n  - ' + problem for problem in problems)))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
