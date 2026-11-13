#!/usr/bin/env python3
"""Memory check: decoders created, used and released one after the other in ONE process must give their
memory back, and the budget guard must decide as expected.

  python testing/conformance/memory_check.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>

Two things went wrong on the console that the conformance runs could not see: a destroyed decoder left
memory behind, and the player lost the memory credit of the budget guard when a creation was refused, so
the next, smaller decoder was refused as well. Both are states a process gets into over several decoders.
This check runs two sequences that would show them again.

1. The warm sequence, with rav1g-dec (-memstate -keep_going). One process runs the same tasks ITERATIONS
   times: the largest stream of the set (3840x2160 8-bit), a decoder of the 4K 10-bit size (-nofitsize: the
   set has no 4K 10-bit stream), 1080p 10-bit with three slots, 1080p 10-bit with -lowmem, a creation the
   guard must refuse (-budget below what the decoder needs), and a smaller decoder the same budget must
   accept. A few temporal units each. rav1g-dec prints the private bytes of the process and the video
   memory the adapter reports for it ("MEMSTATE ...") when everything of a task is released.

   What WARP does with the memory of the process decides what can be checked (measured on the runners of
   CI, docs/warp-decoder.md, "Memory check"):

   - The video memory the adapter reports is exact: it is 0 bytes after every release, and a buffer that
     is not released stays in it to the byte.
   - The private bytes after a release are not what the process had before its first decoder: WARP and
     the D3D12 runtime keep memory of released buffers for the next ones, more or less of it from one
     release to the next (the level after the same task differs by up to 170 MiB between two iterations
     of one run). Only the release of the largest decoder makes them give nearly all of it back, so the
     lowest level of an iteration, its floor, is the number that repeats: within 3 MiB between runs.
   - The floor rises over the first three iterations (192, 270, 291 MiB) and then stays (292 to 300 MiB),
     apart from steps of about 10 MiB that it takes now and then and keeps.

   So the checks are:

   - video memory: after every release the adapter reports at most DXGI_TOLERANCE more than before the
     first task;
   - no growth: the floor of every iteration behind the WARMUP + 1 st is at most GROWTH_TOLERANCE above
     the floor of that one. With ITERATIONS = 8 a decoder that leaves 2 MiB of host memory behind on
     average fails it, and a host pool that is not freed (4 to 20 MiB a decoder) by far;
   - returned: after every release behind the warm-up the private bytes are at most RETURN_TOLERANCE
     above the highest level a release of the warm-up iterations left. A loose bound, for what the floor
     cannot see: memory that is only given back with the next large decoder;
   - the guard: "MEMORY REFUSED" for the task that must be refused and for no other, "accepted" for the
     tasks with a budget that fits, "not checked" for the tasks without one;
   - md5: every task that decodes ends with status=pass.

   Not seen by this check: host memory below these tolerances that every decoder leaves, such as the
   decoder's own struct (69 KiB).

2. The credit sequence, with the test player's decoder code (rav1g-dec -playersmoke) and its model of an
   app with a memory limit (the player's -budget and -retain, testing/player_common/decoder.cpp): a decoder
   is created, used and destroyed and leaves memory charged to the app, as on the console; a creation that
   does not fit is refused; then a smaller decoder must be accepted, which it only is when the player still
   counts the charged memory as available to it (the credit). Twice, with different sizes. Checked: refused
   exactly where expected, accepted behind each refusal, "MD5 is OK" for every task that decodes, no other
   error line, the launch ends.

The tolerances are measured on WARP, on the runners of CI (docs/warp-decoder.md, "Memory check"). The
numbers of every run are printed and go to the step summary.

A D3D12 device that is removed while a sequence runs is not a result of this check (it happens now and
then on WARP); the sequence then runs again in a new process, up to three times.

Exit code: 0 both sequences as they should be, 1 a check failed, 2 the check could not be done.
"""
import argparse
import os
import re
import shutil
import subprocess
import sys

MIB = 1024.0 * 1024.0
ITERATIONS = 8
# The iterations that warm the process up; the one behind them is what the later ones are held against.
WARMUP = 3
# The floor of an iteration (its lowest private bytes after a release) against the floor of iteration
# WARMUP + 1. Measured: at most 15.6 MiB up to iteration 8 (six runs) and 19.1 MiB up to iteration 12 (four
# runs, two builds). A host pool that is never freed gives 58 MiB an iteration.
GROWTH_TOLERANCE = 40 << 20
# Private bytes after a release against the highest level after a release of the first WARMUP + 1
# iterations. Measured: at most 27.5 MiB up to iteration 8, 32.2 MiB up to iteration 12.
RETURN_TOLERANCE = 96 << 20
# Video memory the adapter reports after a release, against before the first task. Measured: 0 bytes
# after each of 312 releases.
DXGI_TOLERANCE = 64 << 10
DEVICE_LOSS_RUNS = 3
# The budget of the two tasks that have one, in MiB: below what the 2160p decoder needs with the margin of
# the guard (470.7 + 48), above what the 720p decoder needs (58.9 + 48).
BUDGET = 300

# The warm sequence: (label, stream of the manifest, flags, what the guard must do).
SEQUENCE = [
    ('2160p-8bit', 'nf-dvbdash-a1-8b-2160p.ivf', ['-limit', '3'], 'not checked'),
    ('4k-10bit-decoder', 'av1-1-b10-00-quantizer-00.ivf', ['-nofitsize', '-limit', '3'], 'not checked'),
    ('1080p-10bit-slots3', 'nf-chimera-10b-1080p.ivf', ['-slots', '3', '-limit', '4'], 'not checked'),
    ('1080p-10bit-lowmem', 'nf-sparks-10b-1080p.ivf', ['-lowmem', '-limit', '4'], 'not checked'),
    ('2160p-8bit-refused', 'nf-dvbdash-a1-8b-2160p.ivf', ['-limit', '3', '-budget', str(BUDGET)], 'refused'),
    ('720p-8bit-accepted', 'nf-chimera-8b-720p.ivf', ['-limit', '4', '-budget', str(BUDGET)], 'accepted'),
]

# The credit sequence, as lines of the player's task file: (label, stream of the manifest, flags, 'decode'
# or 'refused'). The model: the app may use PLAYER_BUDGET MiB more than without a decoder, and a destroyed
# decoder leaves up to PLAYER_RETAIN MiB of its device buffers charged until the next decoder exists. The
# player holds back the textures of its output pool (4 images, -queue_size 4) from what the decoder may
# take: 25.2 MiB for 1080p 10-bit, 50.4 for 2160p 8-bit, 5.6 for 720p 8-bit.
#
#   1080p 10-bit      166.9 MiB of device buffers + 48 margin = 214.9 fit the 250 - 25.2. Destroyed for the
#                     next task: 160 MiB stay charged, so the app has 90 MiB left, and 160 MiB of credit.
#   2160p 8-bit       470.7 + 48 do not fit, with the credit or without: refused.
#   720p 8-bit        58.9 + 48 = 106.9 MiB: more than the 90 - 5.6 the app has left for it, less than the
#                     250 - 5.6 with the credit. Accepted only when the refusal before it kept the credit.
#   2160p 8-bit       refused again; the 720p decoder was destroyed and left its 58.9 MiB charged.
#   270p 8-bit        14.7 + 48 fit either way: the task behind the second refusal runs.
PLAYER_BUDGET = 250
PLAYER_RETAIN = 160
PLAYER_COMMON = ('-md5_frame_check -nopresent -frate -1 -t 4 -queue_size 4 -budget %d -retain %d'
                 % (PLAYER_BUDGET, PLAYER_RETAIN))
PLAYER_SEQUENCE = [
    ('1080p-10bit', 'nf-chimera-10b-1080p.ivf', '-limit 6', 'decode'),
    ('2160p-8bit-refused', 'nf-dvbdash-a1-8b-2160p.ivf', '-limit 3', 'refused'),
    ('720p-8bit-with-credit', 'nf-chimera-8b-720p.ivf', '-limit 6', 'decode'),
    ('2160p-8bit-refused-again', 'nf-dvbdash-a1-8b-2160p.ivf', '-limit 3', 'refused'),
    ('270p-8bit', 'nf-chimera-8b-270p.ivf', '-limit 6', 'decode'),
]
# The error lines a refused creation writes to the player's log; any other error line fails the check.
PLAYER_EXPECTED_ERRORS = re.compile(
    r'MEMORY REFUSED decoder|MEMORY dxgi REFUSED|MEMORY app after the failed decoder creation|'
    r'Failed to initialize decoder|Unable to create decoder')
REMOVED = re.compile(r'(?i)device.{0,24}removed|DXGI_ERROR_DEVICE|0x887A0005|0x887A0006')


def mib(value):
    return '%.1f' % (value / MIB)


def read_manifest(streams):
    manifest = {}
    with open(os.path.join(streams, 'manifest.txt'), encoding='utf-8') as f:
        for line in f:
            name = re.search(r'(?:^|\s)-name\s+(\S+)', line)
            if name:
                manifest[name.group(1)] = line.split()
    return manifest


def stream_path(manifest, stream):
    """The path of a stream relative to the streams directory (-i of its manifest line)."""
    words = manifest[stream]
    return words[words.index('-i') + 1]


def run_tool(command, cwd, log, timeout):
    """The output of a program as text and its exit code; None when it did not end in time."""
    try:
        done = subprocess.run(command, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                              stdin=subprocess.DEVNULL, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        text = (error.stdout or b'').decode('utf-8', 'replace').replace('\r', '')
        with open(log, 'w', encoding='utf-8') as f:
            f.write(text)
        return text, None
    text = done.stdout.decode('utf-8', 'replace').replace('\r', '')
    with open(log, 'w', encoding='utf-8') as f:
        f.write(text)
    return text, done.returncode


# ---------------------------------------------------------------------------------------------------------
# 1. The warm sequence.
# ---------------------------------------------------------------------------------------------------------

def parse_warm(text):
    """The tasks of a rav1g-dec log in order: name, the MEMSTATE fields by state, the RESULT status and
    detail, and what the guard did."""
    tasks = []
    current = None
    for line in text.split('\n'):
        state = re.match(r'MEMSTATE stream=(\S+) at=(\w+) (.*)$', line)
        if state:
            fields = dict((key, int(value)) for key, value in
                          (part.split('=', 1) for part in state.group(3).split()))
            # The commit charge of the process: the private bytes, where the system reports them.
            fields['bytes'] = fields['private'] or fields['commit']
            if state.group(2) == 'before':
                current = {'name': state.group(1), 'states': {}, 'status': 'no RESULT line', 'detail': '',
                           'guard': 'no MEMORY budget line'}
                tasks.append(current)
            if current is not None:
                current['states'][state.group(2)] = fields
            continue
        if current is None:
            continue
        if 'MEMORY REFUSED decoder' in line:
            current['guard'] = 'refused'
        elif 'MEMORY budget: not checked' in line:
            current['guard'] = 'not checked'
        elif 'MEMORY budget:' in line and ': accepted' in line:
            current['guard'] = 'accepted'
        result = re.match(r'RESULT stream=\S+ status=(\w+) .*? detail="(.*)"\s*$', line)
        if result:
            current['status'] = result.group(1)
            current['detail'] = result.group(2)
    return tasks


def check_warm(tasks, iterations):
    """The problems of a warm sequence and the rows of its table."""
    problems = []
    rows = []
    count = len(SEQUENCE)
    if len(tasks) != count * iterations:
        return ['%d tasks in the log, %d expected' % (len(tasks), count * iterations)], rows
    for index, task in enumerate(tasks):
        label, _, _, guard = SEQUENCE[index % count]
        where = 'iteration %d, %s' % (index // count + 1, label)
        for state in ('before', 'released') + (() if guard == 'refused' else ('decoded',)):
            if state not in task['states']:
                problems.append('%s: no MEMSTATE line "%s"' % (where, state))
        if task['guard'] != guard:
            problems.append('%s: the budget guard: %s, expected: %s' % (where, task['guard'], guard))
        if guard == 'refused':
            if task['status'] != 'error' or 'MEMORY REFUSED' not in task['detail']:
                problems.append('%s: status %s, expected the error of a refused creation (%s)'
                                % (where, task['status'], task['detail'][:160]))
        elif task['status'] != 'pass':
            problems.append('%s: status %s, expected pass (%s)' % (where, task['status'], task['detail'][:160]))
    if problems:
        return problems, rows

    released = [task['states']['released'] for task in tasks]
    start = tasks[0]['states']['before']
    have_dxgi = start['dxgi_local'] >= 0
    floors = [min(state['bytes'] for state in released[i * count:(i + 1) * count]) for i in range(iterations)]
    base_floor = floors[WARMUP]
    warmed = max(state['bytes'] for state in released[:(WARMUP + 1) * count])
    for index, task in enumerate(tasks):
        iteration = index // count + 1
        label = SEQUENCE[index % count][0]
        state = released[index]
        decoded = task['states'].get('decoded')
        rows.append((iteration, label, decoded['bytes'] if decoded else None, state['bytes'],
                     state['dxgi_local'] - start['dxgi_local'] if have_dxgi else None, state['settle_ms'],
                     floors[iteration - 1], floors[iteration - 1] - base_floor, state['bytes'] - warmed))
        where = 'iteration %d, %s' % (iteration, label)
        if iteration > WARMUP + 1 and state['bytes'] > warmed + RETURN_TOLERANCE:
            problems.append('%s: %s MiB private after the release, %s MiB above the highest level after a release '
                            'of the first %d iterations (%s MiB); the tolerance is %s MiB'
                            % (where, mib(state['bytes']), mib(state['bytes'] - warmed), WARMUP + 1, mib(warmed),
                               mib(RETURN_TOLERANCE)))
        if have_dxgi and state['dxgi_local'] > start['dxgi_local'] + DXGI_TOLERANCE:
            problems.append('%s: the adapter reports %d bytes (%s MiB) of video memory after the release, %d '
                            'before the first task; the tolerance is %d bytes'
                            % (where, state['dxgi_local'], mib(state['dxgi_local']), start['dxgi_local'],
                               DXGI_TOLERANCE))
    for iteration in range(WARMUP + 2, iterations + 1):
        floor = floors[iteration - 1]
        if floor > base_floor + GROWTH_TOLERANCE:
            problems.append('iteration %d: its lowest private bytes after a release are %s MiB, %s MiB more than '
                            'those of iteration %d (%s MiB); the tolerance is %s MiB'
                            % (iteration, mib(floor), mib(floor - base_floor), WARMUP + 1, mib(base_floor),
                               mib(GROWTH_TOLERANCE)))
    if not have_dxgi:
        problems.append('the adapter does not report its video memory (dxgi_local=-1)')
    return problems, rows


def warm_table(rows):
    """The table of the releases, and one line per iteration with its floor."""
    def cell(value):
        return '-' if value is None else mib(value)
    lines = ['| iteration | task | private with the decoder | private after the release | video memory after the '
             'release | settled in |', '| ---: | --- | ---: | ---: | ---: | ---: |']
    for iteration, label, decoded, bytes_, dxgi, settle, _, _, _ in rows:
        lines.append('| %d | %s | %s | %s | %s | %d ms |'
                     % (iteration, label, cell(decoded), mib(bytes_), cell(dxgi), settle))
    lines += ['', '| iteration | floor: lowest private after a release | against iteration %d | highest private '
              'after a release | against the highest of iterations 1 to %d |' % (WARMUP + 1, WARMUP + 1),
              '| ---: | ---: | ---: | ---: | ---: |']
    for iteration in sorted(set(row[0] for row in rows)):
        mine = [row for row in rows if row[0] == iteration]
        highest = max(mine, key=lambda row: row[3])
        lines.append('| %d | %s | %s | %s | %s |'
                     % (iteration, mib(mine[0][6]), mib(mine[0][7]) if iteration > WARMUP else '-',
                        mib(highest[3]), mib(highest[8]) if iteration > WARMUP + 1 else '-'))
    return lines


def warm_sequence(args, manifest):
    list_path = os.path.join(args.out, 'warm.list')
    with open(list_path, 'w', encoding='utf-8', newline='\n') as f:
        for _ in range(args.iterations):
            for label, stream, flags, _ in SEQUENCE:
                # The manifest line has its own -name; the one behind it is the task's name in this check.
                f.write(' '.join(manifest[stream] + flags + ['-name', label]) + '\n')
    command = [os.path.abspath(args.tool)] + args.tool_args + ['-memstate', '-keep_going', '-v', '-t', '4',
                                                                '-list', os.path.abspath(list_path)]
    for attempt in range(1, DEVICE_LOSS_RUNS + 1):
        log = os.path.join(args.out, 'warm.log' if attempt == 1 else 'warm-run%d.log' % attempt)
        text, code = run_tool(command, args.streams, log, 3600)
        if code is None:
            return ['the warm sequence did not end within an hour'], [], attempt
        if REMOVED.search(text) and attempt < DEVICE_LOSS_RUNS:
            print('memory_check.py: the D3D12 device was removed in run %d of the warm sequence; again' % attempt)
            continue
        problems, rows = check_warm(parse_warm(text), args.iterations)
        if REMOVED.search(text):
            problems.insert(0, 'the D3D12 device was removed in each of %d runs' % DEVICE_LOSS_RUNS)
        elif code not in (0, 2):
            # 2: the refused creations are decode errors of their tasks.
            problems.append('rav1g-dec ended with exit code %d' % code)
        return problems, rows, attempt
    return ['not reached'], [], DEVICE_LOSS_RUNS


# ---------------------------------------------------------------------------------------------------------
# 2. The credit sequence.
# ---------------------------------------------------------------------------------------------------------

def check_player(text, code):
    problems = []
    notes = []
    if code != 0 or 'PLAYERSMOKE the player ended its launch' not in text:
        problems.append('exit code %s: the player did not end its launch' % code)
    # The log by task: a task starts with "start loop 0 for <file>".
    parts = re.split(r'\n(?=[^\n]*start loop 0 for )', text)
    tasks = parts[1:]
    if len(tasks) != len(PLAYER_SEQUENCE):
        problems.append('%d tasks in the player log, %d expected' % (len(tasks), len(PLAYER_SEQUENCE)))
        return problems, notes
    for (label, _, _, expected), part in zip(PLAYER_SEQUENCE, tasks):
        refused = 'MEMORY REFUSED decoder' in part
        accepted = re.search(r'MEMORY budget: .*: accepted', part) is not None
        md5_ok = 'MD5 is OK' in part
        errors = [line for line in part.split('\n')
                  if (':ERROR:' in line or ':FATAL:' in line) and not PLAYER_EXPECTED_ERRORS.search(line)]
        if expected == 'refused':
            if not refused or accepted or md5_ok:
                problems.append('%s: the creation was not refused' % label)
        else:
            if refused or not accepted:
                problems.append('%s: the creation was %s, expected: accepted'
                                % (label, 'refused' if refused else 'not checked by the guard'))
            if not md5_ok:
                problems.append('%s: no "MD5 is OK"' % label)
        if errors and not (expected == 'refused' and not refused):
            problems.append('%s: %d unexpected error lines; the first: %s' % (label, len(errors), errors[0][:200]))
        for pattern in (r'MEMORY app settle after the release: .*', r'MEMORY budget: .*',
                        r'MEMORY REFUSED decoder [^:]*: .*?left for the decoder'):
            for match in re.finditer(pattern, part):
                note = '%s: %s' % (label, match.group(0))
                if note not in notes:
                    notes.append(note)
    return problems, notes


def player_sequence(args, manifest):
    task_dir = os.path.join(args.streams, 'args')
    if os.path.exists(task_dir):
        return ['%s is there already' % task_dir], [], 1
    command = [os.path.abspath(args.tool)] + args.tool_args + ['-playersmoke', os.path.abspath(args.streams)]
    for attempt in range(1, DEVICE_LOSS_RUNS + 1):
        os.makedirs(task_dir)
        try:
            with open(os.path.join(task_dir, 'args_av1.txt'), 'w', encoding='ascii') as f:
                for _, stream, flags, _ in PLAYER_SEQUENCE:
                    f.write('-i /%s %s %s\n' % (stream_path(manifest, stream), PLAYER_COMMON, flags))
            log = os.path.join(args.out, 'player.log' if attempt == 1 else 'player-run%d.log' % attempt)
            text, code = run_tool(command, args.streams, log, 1800)
        finally:
            shutil.rmtree(task_dir, ignore_errors=True)
        if code is None:
            return ['the player did not end its launch within 30 min'], [], attempt
        if REMOVED.search(text) and attempt < DEVICE_LOSS_RUNS:
            print('memory_check.py: the D3D12 device was removed in run %d of the credit sequence; again' % attempt)
            continue
        problems, notes = check_player(text, code)
        if REMOVED.search(text):
            problems.insert(0, 'the D3D12 device was removed in each of %d runs' % DEVICE_LOSS_RUNS)
        return problems, notes, attempt
    return ['not reached'], [], DEVICE_LOSS_RUNS


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--tool', required=True, help='rav1g-dec.exe')
    parser.add_argument('--tool-args', default='', help='further arguments for it, e.g. "-hw" or "-capi"')
    parser.add_argument('--streams', required=True, help='the directory of the conformance streams')
    parser.add_argument('--out', required=True, help='the logs')
    parser.add_argument('--iterations', type=int, default=ITERATIONS)
    parser.add_argument('--only', choices=['warm', 'credit'], help='one of the two sequences')
    parser.add_argument('--title', default='rav1g-dec')
    args = parser.parse_args()
    args.tool_args = args.tool_args.split()
    os.makedirs(args.out, exist_ok=True)
    manifest = read_manifest(args.streams)
    missing = sorted(set(stream for _, stream, _, _ in SEQUENCE + PLAYER_SEQUENCE if stream not in manifest))
    if missing:
        print('memory_check.py: the manifest has no stream %s' % ', '.join(missing))
        return 2
    if args.iterations < WARMUP + 2:
        print('memory_check.py: at least %d iterations (%d warm up, the next is what the later ones are held '
              'against)' % (WARMUP + 2, WARMUP))
        return 2

    warm_problems, rows, warm_runs = ([], [], 0) if args.only == 'credit' else warm_sequence(args, manifest)
    table = warm_table(rows)
    print('Warm sequence, %d iterations of %d tasks in one process (MiB; the first %d iterations warm up):'
          % (args.iterations, len(SEQUENCE), WARMUP))
    for line in table:
        print(line)
    player_problems, notes, player_runs = ([], [], 0) if args.only == 'warm' else player_sequence(args, manifest)
    print('Credit sequence, the player\'s decoder code with -budget %d -retain %d:' % (PLAYER_BUDGET, PLAYER_RETAIN))
    for note in notes:
        print('  ' + note)
    removals = max(warm_runs - 1, 0) + max(player_runs - 1, 0)
    print('D3D12 DEVICE REMOVALS: %d in %d decoder processes (memory check, %s)'
          % (removals, warm_runs + player_runs, args.title))

    problems = ['warm sequence: ' + problem for problem in warm_problems] + \
               ['credit sequence: ' + problem for problem in player_problems]
    for problem in problems:
        print('FAILED %s' % problem)
    refused = sum(1 for _, _, _, guard in SEQUENCE if guard == 'refused') * args.iterations
    decoded = (len(SEQUENCE) * args.iterations) - refused
    worst_return = max([row[8] for row in rows if row[0] > WARMUP + 1] or [0])
    worst_growth = max([row[7] for row in rows if row[0] > WARMUP + 1] or [0])
    worst_dxgi = max([row[4] for row in rows if row[4] is not None] or [0])
    if problems:
        summary = '%d checks failed' % len(problems)
    elif args.only:
        summary = 'the %s sequence is as it should be (the other one was not run)' % args.only
    else:
        summary = ('warm sequence: %d decoders md5 pass, %d creations refused and no other; video memory after a '
                   'release at most %d bytes above the start (tolerance %d); the floor of the private bytes at '
                   'most %s MiB above that of iteration %d (tolerance %s); private bytes after a release at most '
                   '%s MiB above the highest of iterations 1 to %d (tolerance %s). Credit sequence: %d refused, %d '
                   'accepted and MD5 is OK'
                   % (decoded, refused, worst_dxgi, DXGI_TOLERANCE, mib(worst_growth), WARMUP + 1,
                      mib(GROWTH_TOLERANCE), mib(worst_return), WARMUP + 1, mib(RETURN_TOLERANCE),
                      sum(1 for task in PLAYER_SEQUENCE if task[3] == 'refused'),
                      sum(1 for task in PLAYER_SEQUENCE if task[3] == 'decode')))
    print('memory_check.py: %s' % summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('## Memory check, %s\n\n' % args.title)
            f.write('- %s%s\n\n' % (summary, ''.join('\n  - ' + problem for problem in problems)))
            f.write('Warm sequence, MiB:\n\n%s\n\n' % '\n'.join(table))
            f.write('Credit sequence:\n\n%s\n' % '\n'.join('- `%s`' % note for note in notes))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
