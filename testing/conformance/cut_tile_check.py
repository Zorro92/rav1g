"""A decode error in the middle of a stream: the decoder reports it, takes the frame back, and goes on.

Cuts the tile data of one temporal unit of two streams of the conformance set (cut_tile.py): a vector
with one tile and the stream with eight tiles. Both errors come after the frame is set up (it has a
pipeline slot, counted frame buffers, perhaps an output image) and before it is submitted: the decoder
has to take all of that back.

Checked, with the decoder program (rav1g-dec) and with the decoder code of the test player on the same
library (rav1g-dec -playersmoke), all tasks in one process each:

- a damaged stream ends its task with the error, and the task has the frames before the damage;
- the stream decoded behind it in the same process has every frame right;
- with -continue_on_error the same decoder is given the temporal units behind the damaged one: it
  must not hang or lose the device, the frames before the damage and the frames from the next key
  frame on must be those of the reference (the frames in between may be wrong or missing);
- no task loses the D3D12 device.

The decoders are created with the parse pass of rav1d, the default of the library (--parser rav1d;
docs/phase2-rav1d.md, "A damaged stream"), or with --parser c with the entropy decoder of libaom; the
tasks say which with -parser. The words of rav1d for both errors are "Failed to decode tile data"; a
frame that fails leaves its reference slots empty, so the frames that name them fail as well, each with
a decode error of its own. The entropy decoder of libaom fails inside the one tile ("Failed to decode
tile data") and finds a tile whose size does not fit in the stream with eight ("Truncated packet or
corrupt tile size"), and decodes the frames behind from the references it had. The checks are the same
with both: the frames before the damage, the frames from the next key frame on, the stream behind, no
lost device.

With more than one frame in parse (--frames-in-parse N: the tasks get -parseframes N;
docs/phase2-rav1d.md, "Frames in parallel") the decoder does the same, and an application sees it later:
the error of a frame's parse comes back from a later decode call or from the drain, when frames behind
the damaged one that do not depend on it may already be out. So two of the checks are put the way that
holds there: a task that ends at its first error has at least the frames before the damage, all of them
right, and may have right frames behind them; and a task that goes on need not have decoded a temporal
unit behind the first error it was told about (a short stream is through by then). What is held as
before: the words of the error, the frames before the damage, the frames from the next key frame on, the
stream behind the damaged one, no lost device, and that nothing hangs.

usage: python cut_tile_check.py --streams <conformance streams> --out <directory>
           [--tool <rav1g-dec.exe>] [--tool-args "<args>"] [--player-smoke <rav1g-dec.exe>]
           [--parser c|rav1d] [--frames-in-parse N]

The damaged streams are written into <streams>/cuttest and removed at the end; --out gets logs and
md5 lists only, no stream.

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

# (name, stream of the set, the temporal unit to damage, the words of the entropy decoder of libaom for it)
CASES = [
    ('onetile', 'aom/av1-1-b8-23-film_grain-50.ivf', 1, 'Failed to decode tile data'),
    ('tiles', 'svt/syn-tiles-10b-1080p.ivf', 5, 'Truncated packet or corrupt tile size'),
]
CLEAN = 'aom/av1-1-b8-01-size-66x66.ivf'
DEVICE_REMOVED = 'the D3D12 device was removed'
WORK = 'cuttest'


class Failed(Exception):
    pass


def md5_list(path):
    """The md5 of every line of a reference or of a saved list that starts with 32 hex digits."""
    out = []
    with open(path, 'rb') as f:
        for line in f.read().decode('ascii', 'replace').splitlines():
            if re.match(r'^[0-9a-fA-F]{32}', line):
                out.append(line[:32].lower())
    return out


def key_units(path):
    """The temporal units of an IVF file that have a sequence header: where a decoder can start."""
    _, units = cut_tile.read_ivf(path)
    return [index for index, (_, data) in enumerate(units) if any(t == 1 for t, _, _ in cut_tile.split_obus(data))], \
        len(units)


# The words of the parse pass of rav1d for a frame whose tile data cannot be parsed, whatever is wrong
# with it.
RAV1D_WORDS = 'Failed to decode tile data'


def prepare(streams, parser='rav1d'):
    """Writes the damaged streams; returns what the checks need to know about each case."""
    work = os.path.join(streams, WORK)
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    cases = []
    for name, source, unit, words in CASES:
        if parser == 'rav1d':
            words = RAV1D_WORDS
        path = os.path.join(streams, source)
        if not os.path.isfile(path) or not os.path.isfile(path + '.md5'):
            raise Failed('%s or its .md5 is not in the stream set' % source)
        reference = md5_list(path + '.md5')
        keys, units = key_units(path)
        cut = '%s/%s-cut.ivf' % (WORK, name)
        whole = '%s/%s-cut-whole.ivf' % (WORK, name)
        _, what = cut_tile.cut_stream(path, os.path.join(streams, cut), unit, 0.5, False)
        cut_tile.cut_stream(path, os.path.join(streams, whole), unit, 0.5, True)
        # The player looks for the reference of a stream next to it.
        shutil.copyfile(path + '.md5', os.path.join(streams, cut + '.md5'))
        later_keys = [key for key in keys if key > unit]
        # With one frame per temporal unit the frames from a key frame on are the tail of the reference.
        resume = later_keys[0] if later_keys and units == len(reference) else None
        print('%s: %s, temporal unit %d of %d: %s; key frames at %s%s' % (
            name, source, unit, units, what, keys,
            '' if resume is None else '; the reference is checked again from temporal unit %d' % resume))
        cases.append({'name': name, 'source': source, 'unit': unit, 'words': words, 'cut': cut, 'whole': whole,
                      'reference': reference, 'units': units, 'resume': resume,
                      # The frames in front of the damaged temporal unit, where a temporal unit is a frame.
                      'before': unit if units == len(reference) else 1})
    return cases


def check_continued(label, got, case, whole, problems):
    """The md5 list of a task with -continue_on_error against the reference of its stream."""
    reference = case['reference']
    if whole:
        # The whole stream follows the damaged one: its frames are the last ones.
        if len(got) < len(reference) or got[-len(reference):] != reference:
            problems.append('%s: the last %d frames are not those of the stream that follows the damaged one '
                            '(%d frames out)' % (label, len(reference), len(got)))
        else:
            print('%s: %d frames out, the last %d are the frames of the stream' % (label, len(got), len(reference)))
        return
    if case['resume'] is None:
        return
    tail = reference[case['resume']:]
    head = reference[:case['unit']]
    if got[:len(head)] != head:
        problems.append('%s: the %d frames before the damaged temporal unit are not those of the reference' % (
            label, len(head)))
    if len(got) < len(head) + len(tail) or got[-len(tail):] != tail:
        problems.append('%s: the %d frames from the key frame at temporal unit %d on are not those of the reference '
                        '(%d frames out)' % (label, len(tail), case['resume'], len(got)))
    else:
        print('%s: %d frames out of %d; the %d before the damage and the %d from the next key frame on are right' % (
            label, len(got), len(reference), len(head), len(tail)))


def run_tool(args, cases, problems):
    """rav1g-dec: the tasks in one process with -keep_going."""
    several = args.frames_in_parse > 1
    out = os.path.join(args.out, 'tool' + ('-' + re.sub(r'\W+', '', args.tool_args) if args.tool_args else '') +
                       ('-c' if args.parser == 'c' else '') + ('-frames%d' % args.frames_in_parse if several else ''))
    os.makedirs(out, exist_ok=True)
    clean = '-i %s -check %s.md5 -timeout 300' % (CLEAN, CLEAN)
    tasks = []
    for case in cases:
        name = case['name']
        tasks += [
            '-name %s-cut -i %s -check %s.md5 -t 4 -timeout 300' % (name, case['cut'], case['source']),
            '-name clean-behind-%s-cut %s' % (name, clean),
            '-name %s-cut-continue -i %s -continue_on_error -save_md5 %s/tool-%s-cut.md5 -t 4 -timeout 300' % (
                name, case['cut'], WORK, name),
            '-name %s-whole-continue -i %s -continue_on_error -save_md5 %s/tool-%s-whole.md5 -t 4 -timeout 300' % (
                name, case['whole'], WORK, name),
            '-name clean-behind-%s-continue %s' % (name, clean),
        ]
    list_path = os.path.join(args.streams, WORK, 'tool.list')
    with open(list_path, 'w', encoding='ascii') as f:
        f.write('\n'.join(tasks) + '\n')
    parser_args = ['-parser', args.parser] + (['-parseframes', str(args.frames_in_parse)] if several else [])
    command = ([os.path.abspath(args.tool), '-keep_going', '-list', '%s/tool.list' % WORK] + args.tool_args.split() +
               parser_args)
    try:
        done = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=1800)
    except subprocess.TimeoutExpired:
        raise Failed('%s did not end within 30 minutes' % args.tool)
    text = done.stdout.decode('utf-8', 'replace')
    with open(os.path.join(out, 'tool.log'), 'w', encoding='utf-8') as f:
        f.write(text)
    label = os.path.basename(args.tool) + (' ' + args.tool_args if args.tool_args else '') + ' -parser ' + args.parser + \
        (' -parseframes %d' % args.frames_in_parse if several else '')
    said = '^PARSER .*%s' % ('the parse pass of rav1d' if args.parser == 'rav1d' else 'the entropy decoder of libaom')
    if not re.search(said, text, re.M):
        problems.append('%s: the log does not say that this parser reads the streams' % label)
    if several and not re.search(r'^CONFIGURATION .*, frames in parse %d ' % args.frames_in_parse, text, re.M):
        problems.append('%s: no decoder of the run says that it has %d frames in parse' % (label, args.frames_in_parse))
    results = {}
    for line in text.splitlines():
        found = re.match(r'RESULT stream=(\S+) status=(\S+) frames=(\d+) matched=(\d+).* detail="(.*)"\s*$', line)
        if found:
            results[found.group(1)] = (found.group(2), int(found.group(3)), int(found.group(4)), found.group(5))
    if DEVICE_REMOVED in text:
        problems.append('%s: a task lost the D3D12 device' % label)
    # A task with a decode error makes the exit code 2; nothing else may.
    if done.returncode != 2:
        problems.append('%s: exit code %d, 2 was expected (decode errors, and nothing worse)' % (label, done.returncode))
    for task in tasks:
        name = task.split()[1]
        if name not in results:
            problems.append('%s: the task %s has no result' % (label, name))
    for case in cases:
        name = case['name']
        status, frames, matched, detail = results.get(name + '-cut', ('none', 0, 0, ''))
        if status != 'error' or case['words'] not in detail:
            problems.append('%s: %s-cut ended as %s (%s); a decode error with "%s" was expected' % (
                label, name, status, detail, case['words']))
        elif several and (matched < case['before'] or frames < matched):
            # The error is told behind its frame: frames that do not depend on the damaged one may be out
            # behind the right ones, and count as wrong against a reference that has the damaged frame.
            problems.append('%s: %s-cut: %d right frames, the %d before the damage were expected (%d frames out)' % (
                label, name, matched, case['before'], frames))
        elif not several and (frames != matched or frames < 1):
            problems.append('%s: %s-cut: %d of the %d frames before the damage match' % (label, name, matched, frames))
        else:
            print('%s: %s-cut: decode error "%s" behind %d right frames%s' % (
                label, name, case['words'], matched,
                '' if frames == matched else ' (%d frames out when the error was told)' % frames))
        for behind in ('clean-behind-%s-cut' % name, 'clean-behind-%s-continue' % name):
            status, frames, matched, detail = results.get(behind, ('none', 0, 0, ''))
            if status != 'pass':
                problems.append('%s: %s, the stream behind the damaged one, ended as %s (%s)' % (
                    label, behind, status, detail))
        for kind, whole in (('cut', False), ('whole', True)):
            task = '%s-%s-continue' % (name, kind)
            status, frames, matched, detail = results.get(task, ('none', 0, 0, ''))
            errors = re.search(r'CONTINUE %s: (\d+) decode errors, (\d+) temporal units decoded behind the first' %
                               re.escape(task), text)
            if status not in ('nocheck', 'pass', 'mismatch') or not errors or int(errors.group(1)) < 1:
                problems.append('%s: %s ended as %s (%s) %s' % (
                    label, task, status, detail, 'without counted decode errors' if not errors else ''))
                continue
            if int(errors.group(2)) < 1 and not (several and kind == 'cut'):
                problems.append('%s: %s decoded no temporal unit behind its first error' % (label, task))
            saved = os.path.join(args.streams, WORK, 'tool-%s-%s.md5' % (name, kind))
            if not os.path.isfile(saved):
                problems.append('%s: %s saved no md5 list' % (label, task))
                continue
            shutil.copyfile(saved, os.path.join(out, os.path.basename(saved)))
            check_continued('%s: %s (%s decode errors)' % (label, task, errors.group(1)), md5_list(saved), case, whole,
                            problems)
    print('%s: D3D12 DEVICE REMOVALS: %d' % (label, text.count(DEVICE_REMOVED)))


def run_player(args, cases, problems):
    """The decoder code of the test player (rav1g-dec -playersmoke): the same, as tasks of its task file."""
    out = os.path.join(args.out, 'player' + ('-c' if args.parser == 'c' else ''))
    os.makedirs(out, exist_ok=True)
    common = '-nopresent -frate -1 -parser ' + args.parser
    clean = '-i /%s -md5_frame_check %s' % (CLEAN, common)
    tasks = []
    for case in cases:
        name = case['name']
        tasks += [
            '-i /%s -md5_frame_check %s -t 4' % (case['cut'], common),
            clean,
            '-i /%s -continue_on_error -save_md5 /%s/player-%s-cut.md5 %s -t 4' % (case['cut'], WORK, name, common),
            '-i /%s -continue_on_error -save_md5 /%s/player-%s-whole.md5 %s -t 4' % (case['whole'], WORK, name, common),
            clean,
        ]
    task_dir = os.path.join(args.streams, 'args')
    os.makedirs(task_dir, exist_ok=True)
    try:
        with open(os.path.join(task_dir, 'args_av1.txt'), 'w', encoding='ascii') as f:
            f.write('\n'.join(tasks) + '\n')
        command = [os.path.abspath(args.player_smoke), '-playersmoke', os.path.abspath(args.streams)]
        try:
            done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=1800)
        except subprocess.TimeoutExpired:
            raise Failed('%s -playersmoke did not end within 30 minutes' % args.player_smoke)
    finally:
        shutil.rmtree(task_dir, ignore_errors=True)
    text = done.stdout.decode('utf-8', 'replace')
    with open(os.path.join(out, 'player.log'), 'w', encoding='utf-8') as f:
        f.write(text)
    label = 'player -parser ' + args.parser
    said = 'Parser: the parse pass of rav1d' if args.parser == 'rav1d' else 'Parser: the entropy decoder of libaom'
    if said not in text:
        problems.append('%s: the log does not say that this parser reads the streams' % label)
    if done.returncode != 0 or 'PLAYERSMOKE the player ended its launch' not in text:
        problems.append('%s: exit code %d: the player did not end its launch' % (label, done.returncode))
    if re.search(r'(?i)device.{0,16}removed|DXGI_ERROR_DEVICE', text):
        problems.append('%s: a task lost the D3D12 device' % label)
    if not re.search(r'LIBRARY rav1g .*the Rust host', text):
        problems.append('%s: the log has no LIBRARY line of the Rust library' % label)
    ok = text.count('MD5 is OK')
    if ok != 2 * len(cases):
        problems.append('%s: %d of the %d streams behind a damaged one ended with "MD5 is OK"' % (
            label, ok, 2 * len(cases)))
    for case in cases:
        name = case['name']
        if case['words'] not in text:
            problems.append('%s: the log does not have the decode error "%s" of %s' % (label, case['words'], name))
        for kind, whole in (('cut', False), ('whole', True)):
            saved = os.path.join(args.streams, WORK, 'player-%s-%s.md5' % (name, kind))
            if not os.path.isfile(saved):
                problems.append('%s: the task of %s-%s with -continue_on_error saved no md5 list' % (label, name, kind))
                continue
            shutil.copyfile(saved, os.path.join(out, os.path.basename(saved)))
            check_continued('%s: %s-%s with -continue_on_error' % (label, name, kind), md5_list(saved), case, whole,
                            problems)
    ends = re.findall(r'CONTINUE_ON_ERROR: (\d+) decode errors, (\d+) temporal units decoded behind the first', text)
    if len(ends) != 2 * len(cases) or any(int(errors) < 1 or int(units) < 1 for errors, units in ends):
        problems.append('%s: %d tasks with -continue_on_error counted their errors and went on, %d were run (%s)' % (
            label, len([1 for errors, units in ends if int(errors) and int(units)]), 2 * len(cases), ends))
    else:
        print('%s: %d tasks ended with a decode error or went on behind one (%s decode errors); %d streams behind '
              'them: MD5 is OK; the launch ended' % (label, 3 * len(cases), '+'.join(e for e, _ in ends), ok))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--streams', required=True, help='the directory of the conformance streams')
    parser.add_argument('--out', required=True, help='logs and md5 lists (no stream)')
    parser.add_argument('--tool', help='rav1g-dec.exe')
    parser.add_argument('--tool-args', default='', help='further arguments for it, e.g. "-capi"')
    parser.add_argument('--player-smoke', help='rav1g-dec.exe, for its -playersmoke')
    parser.add_argument('--frames-in-parse', type=int, default=1,
                        help='the tasks get -parseframes N: more than one frame in parse (with --tool)')
    parser.add_argument('--parser', choices=['c', 'rav1d'], default='rav1d',
                        help='what reads the streams (default rav1d, as the library)')
    args = parser.parse_args()
    if not args.tool and not args.player_smoke:
        parser.error('--tool or --player-smoke')
    os.makedirs(args.out, exist_ok=True)
    problems = []
    try:
        try:
            cases = prepare(args.streams, args.parser)
            if args.tool:
                run_tool(args, cases, problems)
            if args.player_smoke:
                run_player(args, cases, problems)
        finally:
            shutil.rmtree(os.path.join(args.streams, WORK), ignore_errors=True)
    except (Failed, cut_tile.Bad, OSError) as error:
        print('cut_tile_check.py: %s' % error)
        return 2
    for problem in problems:
        print('FAILED %s' % problem)
    summary = 'cut_tile_check.py: %s' % ('%d checks failed' % len(problems) if problems else
                                         'a decode error ends its task or is passed over, and the decoder goes on')
    print(summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n- %s%s\n' % (summary, ''.join('\n  - ' + problem for problem in problems)))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
