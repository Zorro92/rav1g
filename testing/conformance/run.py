#!/usr/bin/env python3
"""Conformance run: decodes every stream of a prepared directory with warp_decoder and compares the result
with testing/conformance/expected.txt.

  python testing/conformance/run.py --tool <warp_decoder.exe> --streams <dir> --out <dir>

<dir> of --streams is what testing/conformance/prepare_streams.sh wrote: the streams, their md5 references
and manifest.txt, one warp_decoder task per line. The tasks run in one decoder process, in the order of the
manifest; a task that ends in a decode error, a timeout or a crash ends its process, and the run goes on
with the next task in a new one. A stream that fails after other streams in the same process is decoded
again alone, in a process of its own, and must fail the same way. One case is not a failure of the stream:
the D3D12 device is removed while a stream is decoded after others, and the stream is right alone. It is
reported ("D3D12 device removed") and the result of the stream is the one it has alone.

Written to --out:
  results.tsv              one line per stream
  summary.md               the tables (also appended to $GITHUB_STEP_SUMMARY when that is set)
  expected.suggested.txt   the failing streams in the format of expected.txt
  logs/<stream>.log        the decoder's output for the stream; <stream>.alone.log for the repeat
  md5/<stream>.md5         the md5 of every output frame, in the test player's format

Exit code: 0 when every stream that fails is listed in expected.txt with the way it fails, and every stream
listed there fails; 1 otherwise (an unexpected failure, a failure of another kind, a listed stream that
passes or is not part of the run); 2 when the run itself could not be done.
"""
import argparse
import os
import re
import subprocess
import sys
import time

FAIL_KINDS = ('mismatch', 'error', 'timeout', 'crash', 'unstable')
DEVICE_REMOVED = 'the D3D12 device was removed'


class Result:
    def __init__(self, name):
        self.name = name
        self.status = 'crash'
        self.frames = 0
        self.matched = 0
        self.first_bad = -1
        self.ref_frames = 0
        self.decode_ms = 0.0
        self.create_ms = 0.0
        self.size = ''
        self.bitdepth = ''
        self.detail = ''
        self.first_in_process = False
        self.done = False
        self.note = ''

    def same_outcome(self, other):
        return (self.status, self.frames, self.matched, self.first_bad) == \
               (other.status, other.frames, other.matched, other.first_bad)

    def outcome(self):
        text = '%s, %d of %d frames match' % (self.status, self.matched, self.frames)
        if self.first_bad >= 0:
            text += ', first bad frame %d' % self.first_bad
        return text


def task_name(line):
    match = re.search(r'(?:^|\s)-name\s+(\S+)', line)
    if not match:
        raise SystemExit('manifest line without -name: ' + line)
    return match.group(1)


def task_timeout(line):
    match = re.search(r'(?:^|\s)-timeout\s+(\d+)', line)
    return int(match.group(1)) if match else 0


def parse_result(line, result):
    """A 'RESULT key=value ... detail="..."' line of warp_decoder."""
    detail = re.search(r' detail="(.*)"\s*$', line)
    head = line[:detail.start()] if detail else line
    fields = dict(part.split('=', 1) for part in head.split()[1:] if '=' in part)
    result.status = fields.get('status', 'error')
    result.frames = int(fields.get('frames', 0))
    result.matched = int(fields.get('matched', 0))
    result.first_bad = int(fields.get('first_bad', -1))
    result.ref_frames = int(fields.get('ref_frames', 0))
    result.decode_ms = float(fields.get('decode_ms', 0))
    result.create_ms = float(fields.get('create_ms', 0))
    result.size = fields.get('size', '')
    result.bitdepth = fields.get('bitdepth', '')
    result.detail = detail.group(1) if detail else ''
    if result.status == 'nocheck':
        result.status = 'error'
        result.detail = 'decoded without a reference; ' + result.detail


def run_process(args, tasks, tag):
    """Runs the tasks (name, manifest line) in one decoder process. Returns (results in task order for the
    tasks the process got to, header lines, fatal message or None)."""
    list_path = os.path.join(args.out, 'lists', tag + '.list')
    log_path = os.path.join(args.out, 'lists', tag + '.log')
    with open(list_path, 'w', encoding='utf-8', newline='\n') as f:
        for name, line in tasks:
            md5_path = os.path.join(args.out, 'md5', name + '.md5')
            f.write('%s -save_md5 "%s"\n' % (line, md5_path))
    command = [args.tool, '-list', list_path, '-t', str(args.threads)] + args.tool_args
    # What the decoder's own -timeout cannot end is ended here.
    limit = sum(task_timeout(line) or args.default_timeout for _, line in tasks) + 600
    killed = False
    started = time.time()
    with open(log_path, 'wb') as log:
        try:
            process = subprocess.Popen(command, cwd=args.streams, stdout=log, stderr=subprocess.STDOUT,
                                       stdin=subprocess.DEVNULL)
        except OSError as error:
            return [], [], 'cannot start %s: %s' % (args.tool, error)
        try:
            code = process.wait(timeout=limit)
        except subprocess.TimeoutExpired:
            process.kill()
            code = process.wait()
            killed = True
    elapsed = time.time() - started
    with open(log_path, encoding='utf-8', errors='replace') as f:
        lines = f.read().splitlines()

    results = []
    header = []
    sections = {}
    current = None
    for line in lines:
        begin = re.match(r'BEGIN stream=(\S+)', line)
        if begin:
            current = Result(begin.group(1))
            current.first_in_process = not results
            results.append(current)
            sections[current.name] = [line]
            continue
        if current is None:
            header.append(line)
            continue
        sections[current.name].append(line)
        if line.startswith('RESULT stream=' + current.name + ' '):
            parse_result(line, current)
            current.done = True
    for result in results:
        if not result.done:
            # The process ended inside this task without a result line.
            tail = [l for l in sections[result.name][1:] if l.strip()][-3:]
            if killed:
                result.status = 'timeout'
                result.detail = 'no result after %d s, process ended by the run' % int(elapsed)
            else:
                result.status = 'crash'
                result.detail = 'process ended with exit code %s without a result' % exit_code_text(code)
            if tail:
                result.detail += '; last output: ' + ' | '.join(tail)
        suffix = '.alone.log' if tag.startswith('alone') else '.log'
        with open(os.path.join(args.out, 'logs', result.name + suffix), 'w', encoding='utf-8', newline='\n') as f:
            f.write('\n'.join(header + sections[result.name]) + '\n')
    fatal = None
    if not results:
        fatal = 'the decoder ran no task (exit code %s):\n%s' % (exit_code_text(code), '\n'.join(lines[-30:]))
    return results, header, fatal


def exit_code_text(code):
    if code is None:
        return '?'
    if code < 0 or code > 255:
        return '0x%08X' % (code & 0xFFFFFFFF)
    return str(code)


def read_expected(path):
    expected = {}
    if not path or not os.path.exists(path):
        return expected
    with open(path, encoding='utf-8') as f:
        for number, line in enumerate(f, 1):
            line = line.strip()
            if not line or line.startswith('#'):
                continue
            parts = line.split(None, 2)
            if len(parts) < 3 or parts[1] not in FAIL_KINDS:
                raise SystemExit('%s, line %d: expected "<stream> <%s> <reason>"' % (path, number, '|'.join(FAIL_KINDS)))
            if parts[0] in expected:
                raise SystemExit('%s, line %d: %s is listed twice' % (path, number, parts[0]))
            expected[parts[0]] = (parts[1], parts[2])
    return expected


def cell(text):
    return str(text).replace('|', '\\|').replace('\n', ' ')


def seconds(ms):
    return '%.1f' % (ms / 1000.0)


def main():
    parser = argparse.ArgumentParser(description='Conformance run of warp_decoder.')
    parser.add_argument('--tool', required=True, help='warp_decoder.exe')
    parser.add_argument('--streams', required=True, help='directory written by prepare_streams.sh')
    parser.add_argument('--out', required=True, help='directory for the results')
    parser.add_argument('--expected', default=os.path.join(os.path.dirname(os.path.abspath(__file__)), 'expected.txt'))
    parser.add_argument('--threads', type=int, default=4, help='decoder threads (-t)')
    parser.add_argument('--only', default='', help='only the streams whose name matches this regular expression')
    parser.add_argument('--default-timeout', type=int, default=600, help='seconds for a task without -timeout')
    parser.add_argument('--no-repeat', action='store_true', help='do not decode failing streams again alone')
    parser.add_argument('--tool-args', default='', help='further arguments for every task, e.g. "-cdefv1 -hw"')
    args = parser.parse_args()
    args.tool = os.path.abspath(args.tool)
    args.streams = os.path.abspath(args.streams)
    args.out = os.path.abspath(args.out)
    args.tool_args = args.tool_args.split()
    for sub in ('lists', 'logs', 'md5'):
        os.makedirs(os.path.join(args.out, sub), exist_ok=True)

    manifest = os.path.join(args.streams, 'manifest.txt')
    try:
        with open(manifest, encoding='utf-8') as f:
            lines = [l.strip() for l in f if l.strip() and not l.lstrip().startswith('#')]
    except OSError as error:
        print('cannot read %s: %s' % (manifest, error))
        return 2
    tasks = [(task_name(line), line) for line in lines]
    all_names = [name for name, _ in tasks]
    if len(set(all_names)) != len(all_names):
        print('%s: a stream name is used twice' % manifest)
        return 2
    if args.only:
        tasks = [t for t in tasks if re.search(args.only, t[0])]
    if not tasks:
        print('no streams to run')
        return 2
    expected = read_expected(args.expected)

    started = time.time()
    results = {}
    header = []
    remaining = list(tasks)
    processes = 0
    while remaining:
        processes += 1
        got, head, fatal = run_process(args, remaining, 'run%03d' % processes)
        if fatal:
            print(fatal)
            return 2
        header = header or head
        if [r.name for r in got] != [name for name, _ in remaining[:len(got)]]:
            print('the decoder did not run the tasks in the order of its list')
            return 2
        for result in got:
            results[result.name] = result
            print('%-52s %s' % (result.name, result.outcome()), flush=True)
        # The process stops at the first task that does not end in pass or mismatch.
        remaining = remaining[len(got):]

    # A failure after other streams in the same process is repeated in a process of its own: what is put
    # into expected.txt must not depend on what was decoded before.
    repeats = 0
    device_lost = []
    if not args.no_repeat:
        by_name = dict(tasks)
        for name, _ in tasks:
            result = results[name]
            if result.status == 'pass' or result.status == 'timeout' or result.first_in_process:
                continue
            repeats += 1
            got, _, fatal = run_process(args, [(name, by_name[name])], 'alone%03d' % repeats)
            if fatal:
                print(fatal)
                return 2
            alone = got[0]
            print('%-52s alone: %s' % (name, alone.outcome()), flush=True)
            if alone.same_outcome(result):
                result.note = 'the same alone'
            elif alone.status == 'pass' and DEVICE_REMOVED in result.detail:
                # The device was lost while the stream was decoded after others in the same process, and the
                # stream is right when it is decoded alone. Which stream this hits changes from run to run
                # (it is not the stream's fault and cannot be listed in expected.txt), so the stream counts
                # as decoded, and the loss is reported on its own.
                device_lost.append('%s: %s (%s)' % (name, result.outcome(), result.detail))
                alone.note = 'lost the D3D12 device after other streams in the same process; this is the result alone'
                alone.first_in_process = True
                results[name] = alone
            else:
                result.detail = 'after other streams: %s (%s); alone: %s (%s)' % (
                    result.outcome(), result.detail, alone.outcome(), alone.detail)
                result.status = 'unstable'
    total_s = time.time() - started

    # Verdicts.
    problems = []
    rows = []
    for name, _ in tasks:
        result = results[name]
        listed = expected.get(name)
        if result.status == 'pass':
            verdict = 'ok'
            if listed:
                verdict = 'FIXED'
                problems.append('%s passes but is listed in expected.txt as %s: remove it from the list' % (name, listed[0]))
        elif not listed:
            verdict = 'UNEXPECTED'
            problems.append('%s: %s, not listed in expected.txt (%s)' % (name, result.outcome(), result.detail))
        elif listed[0] != result.status:
            verdict = 'CHANGED'
            problems.append('%s: %s, but expected.txt lists it as %s' % (name, result.outcome(), listed[0]))
        else:
            verdict = 'expected'
        rows.append((result, verdict, listed))
    if not args.only:
        for name in expected:
            if name not in results:
                problems.append('%s is listed in expected.txt but is not a stream of this run' % name)

    count = lambda verdict: sum(1 for _, v, _ in rows if v == verdict)
    passed = sum(1 for r, _, _ in rows if r.status == 'pass')
    failed = len(rows) - passed
    decode_s = sum(r.decode_ms for r, _, _ in rows) / 1000.0

    with open(os.path.join(args.out, 'results.tsv'), 'w', encoding='utf-8', newline='\n') as f:
        f.write('stream\tstatus\tverdict\tframes\tmatched\tfirst_bad\tref_frames\tdecode_ms\tcreate_ms\tsize\tbitdepth\tdetail\n')
        for result, verdict, _ in rows:
            f.write('\t'.join(str(x) for x in (
                result.name, result.status, verdict, result.frames, result.matched, result.first_bad,
                result.ref_frames, int(result.decode_ms), int(result.create_ms), result.size, result.bitdepth,
                result.detail.replace('\t', ' '))) + '\n')
    with open(os.path.join(args.out, 'expected.suggested.txt'), 'w', encoding='utf-8', newline='\n') as f:
        for result, _, listed in rows:
            if result.status != 'pass':
                reason = listed[1] if listed and listed[0] == result.status else result.detail
                f.write('%s %s %s\n' % (result.name, result.status, reason))

    summary = ['## Conformance run', '']
    for line in header:
        if line.startswith(('ADAPTER', 'PIPELINE')):
            summary.append('- `%s`' % line)
    summary += [
        '- %d streams: **%d pass, %d fail** (%d expected, %d unexpected, %d of another kind than listed); '
        '%d listed in expected.txt pass now' % (len(rows), passed, failed, count('expected'), count('UNEXPECTED'),
                                                count('CHANGED'), count('FIXED')),
        '- Time: %.1f min for the whole run (%d decoder processes, %d failing streams decoded again alone); '
        '%.1f min of it decoding' % (total_s / 60.0, processes, repeats, decode_s / 60.0),
        '- Decoder threads: %d%s' % (args.threads, ('; further arguments: ' + ' '.join(args.tool_args)) if args.tool_args else ''),
        '']
    if device_lost:
        summary += ['### D3D12 device removed', '',
                    'These streams lost the D3D12 device when they were decoded after other streams in the same '
                    'process, and are right when decoded alone, which is the result in the tables. The run does '
                    'not fail on it (docs/warp-decoder.md).', ''] + ['- ' + cell(p) for p in device_lost] + ['']
    if problems:
        summary += ['### Not as expected', ''] + ['- ' + cell(p) for p in problems] + ['']
    table_head = ['| stream | size | bits | frames | matching | first bad frame | decode s | result | expected.txt | detail |',
                  '| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |']

    def table_row(result, verdict, listed):
        return '| %s | %s | %s | %d | %d | %s | %s | %s | %s | %s |' % (
            cell(result.name), result.size, result.bitdepth, result.frames, result.matched,
            result.first_bad if result.first_bad >= 0 else '', seconds(result.decode_ms),
            result.status if result.status == 'pass' else '**%s**' % result.status,
            '' if verdict == 'ok' else verdict,
            cell(result.detail + ('; ' + result.note if result.note else '')) if result.status != 'pass' else '')

    failing = [row for row in rows if row[0].status != 'pass']
    if failing:
        summary += ['### Streams that do not pass', ''] + table_head + [table_row(*row) for row in failing] + ['']
        summary += ['Reasons listed in expected.txt:', '']
        summary += ['- `%s`: %s' % (r.name, cell(listed[1])) for r, _, listed in failing if listed] + ['']
    summary += ['### All streams', ''] + table_head + [table_row(*row) for row in rows] + ['']
    text = '\n'.join(summary)
    with open(os.path.join(args.out, 'summary.md'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(text)
    step_summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if step_summary:
        with open(step_summary, 'a', encoding='utf-8', newline='\n') as f:
            f.write(text)

    print()
    print('%d streams: %d pass, %d fail (%d expected, %d unexpected, %d of another kind), %d listed ones pass now'
          % (len(rows), passed, failed, count('expected'), count('UNEXPECTED'), count('CHANGED'), count('FIXED')))
    print('%.1f min for the whole run, %.1f min of it decoding' % (total_s / 60.0, decode_s / 60.0))
    for lost in device_lost:
        print('D3D12 DEVICE REMOVED after other streams in the same process, right alone: ' + lost)
    for problem in problems:
        print('NOT AS EXPECTED: ' + problem)
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
