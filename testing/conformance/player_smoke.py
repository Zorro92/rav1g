#!/usr/bin/env python3
"""The decoder code of the test player on the Rust library, on the device of rav1g-dec.

  python testing/conformance/player_smoke.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>

The player is a UWP app and does not run in CI. Its decoder code (testing/player_common: the task file, the
decoder creation with a device and no queue, the output pool, the md5 check, the timing run) is compiled
into rav1g-dec as it is compiled into the player, and `rav1g-dec -playersmoke <dir>` runs it as the player
runs it, through the functions of rav1g.h on the Rust library of that program: the same files and the same
functions as in the packages. This script gives it the task file the player reads from its LocalState
(<streams>/args/args_av1.txt, removed at the end) and reads the player's log:

- the log names the Rust library ("LIBRARY rav1g ... the Rust host");
- every task ends with "MD5 is OK";
- the log has no error line;
- the launch ends (the player says so), and no device is lost;
- a launch without a task file ends as well: it logs that it has no tasks and does not sit idle.

The cases with a decode error in the middle of a stream (a cut tile, -continue_on_error) are in
cut_tile_check.py --player-smoke.

Exit code: 0 everything as it should be, 1 a check failed, 2 the check could not be done.
"""
import argparse
import os
import re
import shutil
import subprocess
import sys

# The tasks, as lines of the player's task file: a small 8-bit vector, 10-bit with four threads and three
# frame slots, a stream whose frames change their size (Matroska, the reduced back buffer, -r), spatial
# layers with the timing run and the asynchronous clear.
TASKS = [
    '-i /aom/av1-1-b8-01-size-66x66.ivf -md5_frame_check -nopresent -frate -1',
    '-i /aom/av1-1-b10-00-quantizer-00.ivf -md5_frame_check -nopresent -frate -1 -t 4 -slots 3',
    '-i /aom/av1-1-b8-03-sizeup.mkv -md5_frame_check -nopresent -frate -1 -t 4 -r -lowmem',
    '-i /aom/av1-1-b8-22-svc-L2T1.ivf -md5_frame_check -nopresent -frate -1 -gputime -asyncclear',
]
ENDED = 'PLAYERSMOKE the player ended its launch'
SHOWN = re.compile(r'LIBRARY |MD5 |ERROR|PLAYERSMOKE|Frame slots|Bye|No args')


def launch(tool, tool_args, root, log, timeout):
    """Runs the player's decoder code on |root|; its log as text, and the exit code."""
    command = [os.path.abspath(tool)] + tool_args + ['-playersmoke', os.path.abspath(root)]
    try:
        done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
    except subprocess.TimeoutExpired:
        print('player_smoke.py: %s did not end within %d s' % (' '.join(command), timeout))
        sys.exit(2)
    text = done.stdout.decode('utf-8', 'replace').replace('\r', '')
    with open(log, 'w', encoding='utf-8') as f:
        f.write(text)
    for line in text.split('\n'):
        if SHOWN.search(line):
            print(line)
    return text, done.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--tool', required=True, help='rav1g-dec.exe')
    parser.add_argument('--tool-args', default='', help='further arguments for it, e.g. "-hw"')
    parser.add_argument('--player-args', default='', help='further flags for every task of the player, e.g. '
                        '"-parser c" or "-parseopt none,time"')
    parser.add_argument('--streams', required=True, help='the directory of the conformance streams')
    parser.add_argument('--out', required=True, help='the logs')
    parser.add_argument('--title', default='the Rust library')
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)
    tool_args = args.tool_args.split()
    problems = []

    task_dir = os.path.join(args.streams, 'args')
    if os.path.exists(task_dir):
        print('player_smoke.py: %s is there already' % task_dir)
        return 2
    os.makedirs(task_dir)
    try:
        with open(os.path.join(task_dir, 'args_av1.txt'), 'w', encoding='ascii') as f:
            f.write('\n'.join(task + (' ' + args.player_args if args.player_args else '') for task in TASKS) + '\n')
        text, code = launch(args.tool, tool_args, args.streams, os.path.join(args.out, 'player.log'), 1800)
    finally:
        shutil.rmtree(task_dir, ignore_errors=True)
    if code != 0 or ENDED not in text:
        problems.append('exit code %d: the player did not end its launch' % code)
    library = re.search(r'LIBRARY (rav1g .*)', text)
    if not library or 'the Rust host' not in library.group(1):
        problems.append('the log has no LIBRARY line of the Rust library')
    errors = [line for line in text.split('\n') if ':ERROR:' in line or ':FATAL:' in line]
    if errors:
        problems.append('%d error lines in the player log; the first: %s' % (len(errors), errors[0]))
    if re.search(r'(?i)device.{0,16}removed|DXGI_ERROR_DEVICE', text):
        problems.append('a task lost the D3D12 device')
    # What reads the streams: the parse pass of rav1d with its two default switches unless the tasks say
    # otherwise (-parser c; -parseopt none).
    if '-parser c' in args.player_args:
        if 'Parser: the entropy decoder of libaom' not in text or 'Parser: the parse pass of rav1d' in text:
            problems.append('the log does not say that the entropy decoder of libaom reads the streams')
    else:
        if 'Parser: the parse pass of rav1d' not in text or 'Parser: the entropy decoder of libaom' in text:
            problems.append('the log does not say that the parse pass of rav1d reads the streams')
        switches = 'none' if '-parseopt none' in args.player_args else 'nolfmask tilethread'
        if 'Parser switches (-parseopt): ' + switches not in text:
            problems.append('the log does not say that the switches of the parser are: ' + switches)
    ok = text.count('MD5 is OK')
    if ok != len(TASKS):
        problems.append('%d of %d tasks of the player ended with "MD5 is OK"' % (ok, len(TASKS)))

    # Without a task file: the player says that it has no tasks, and the launch ends.
    empty = os.path.join(args.out, 'empty')
    os.makedirs(empty, exist_ok=True)
    text, code = launch(args.tool, tool_args, empty, os.path.join(args.out, 'no-tasks.log'), 300)
    if code != 0 or ENDED not in text:
        problems.append('without a task file: exit code %d: the player did not end its launch' % code)
    if 'No args were found' not in text:
        problems.append('without a task file: the player did not say that it has no tasks')

    for problem in problems:
        print('FAILED %s' % problem)
    summary = ('%d checks failed' % len(problems) if problems else
               '%d of %d tasks: MD5 is OK; the launch ended; a launch without a task file ended' % (ok, len(TASKS)))
    print('player_smoke.py: %s' % summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('## Test player decoder code through rav1g.h on %s\n\n' % args.title)
            if library:
                f.write('`%s`\n\n' % library.group(1).strip())
            f.write('- %s%s\n' % (summary, ''.join('\n  - ' + problem for problem in problems)))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
