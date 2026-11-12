#!/usr/bin/env python3
"""Order check: streams decoded one after the other in ONE decoder process, every one against its md5
reference, and nothing is decoded again alone.

  python testing/conformance/order_check.py --tool <warp_decoder.exe|rav1g-dec.exe> --streams <dir> --out <dir>

The conformance run (run.py) decodes a failing stream again in a process of its own and takes a stream that
then passes for unstable. That is right for a list of failing streams and wrong as a test of the decoder: a
decoder whose output depends on what the process decoded before is broken, and decoding the stream alone is
exactly what hides it. A read of struct padding in gen_pred_blocks.hlsl stayed in the tree that way
(docs/gpu-av1-decoder.md, "Padding of the mode info record"): the memory of a destroyed decoder went to the
next one, and the shader read bytes of it that the new decoder never wrote.

The sequences are the ones that showed it, each in a new process and in this order:

  pair     av1-1-b8-00-quantizer-00, then av1-1-b10-00-quantizer-00              -t 1 and -t 4
  prefix   av1-1-b8-00-quantizer-00 .. -09, then av1-1-b10-00-quantizer-00       -t 1 and -t 4
  svc      av1-1-b8-22-svc-L2T1, then av1-1-b8-22-svc-L2T1-2                     -t 4

Every task of a sequence must pass. A failure is final: no stream is decoded again alone. One case is not a
result of the decoder: the D3D12 device is removed while a sequence runs, which happens now and then on WARP
(docs/warp-decoder.md). Then the whole sequence runs again in a new process, in the same order, up to three
times in all; a sequence that loses the device every time fails.

Written to --out: <sequence>/lists, logs and md5 as run.py writes them, and summary.md (also appended to
$GITHUB_STEP_SUMMARY when that is set).

Exit code: 0 when every sequence passes, 1 when one does not, 2 when the check itself could not be done.
"""
import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import run as conformance  # noqa: E402  (run.py of this directory: the task lists and the RESULT lines)

Q8 = ['av1-1-b8-00-quantizer-%02d.ivf' % i for i in range(10)]
Q10 = 'av1-1-b10-00-quantizer-00.ivf'
SVC = ['av1-1-b8-22-svc-L2T1.ivf', 'av1-1-b8-22-svc-L2T1-2.ivf']
# (name, threads, streams in the order they are decoded)
SEQUENCES = [
    ('pair-t1', 1, [Q8[0], Q10]),
    ('pair-t4', 4, [Q8[0], Q10]),
    ('prefix-t1', 1, Q8 + [Q10]),
    ('prefix-t4', 4, Q8 + [Q10]),
    ('svc-t4', 4, SVC),
]
DEVICE_LOSS_RUNS = 3


def run_sequence(args, name, threads, tasks):
    """One process for the sequence. Returns (passed, lines for the summary, fatal message or None)."""
    lines = []
    for attempt in range(1, DEVICE_LOSS_RUNS + 1):
        tag = name if attempt == 1 else '%s-run%d' % (name, attempt)
        out = os.path.join(args.out, tag)
        for sub in ('lists', 'logs', 'md5'):
            os.makedirs(os.path.join(out, sub), exist_ok=True)
        process_args = argparse.Namespace(tool=args.tool, streams=args.streams, out=out, threads=threads,
                                          tool_args=args.tool_args, default_timeout=args.default_timeout)
        results, _, fatal = conformance.run_process(process_args, tasks, 'sequence')
        if fatal:
            return False, lines, fatal
        for result in results:
            print('%-12s %-36s %s' % (tag, result.name, result.outcome()), flush=True)
        # The process ends at the first task that does not end in pass or mismatch.
        failed = ['%s: %s (%s)' % (r.name, r.outcome(), r.detail) for r in results if r.status != 'pass']
        missing = [n for n, _ in tasks[len(results):]]
        if not failed and not missing:
            lines.append('%s: pass, %d streams%s' % (name, len(results),
                                                    ', in run %d of the sequence' % attempt if attempt > 1 else ''))
            return True, lines, None
        if any(conformance.device_removed(r) for r in results):
            lines.append('%s, run %d: the D3D12 device was removed (%s)' % (name, attempt, '; '.join(failed)))
            continue
        lines.append('%s: FAILED: %s%s' % (name, '; '.join(failed),
                                           '; not decoded: ' + ', '.join(missing) if missing else ''))
        return False, lines, None
    lines.append('%s: FAILED: the D3D12 device was removed in each of %d runs of the sequence' % (name, DEVICE_LOSS_RUNS))
    return False, lines, None


def main():
    parser = argparse.ArgumentParser(description='Streams one after the other in one decoder process, no repeats alone.')
    parser.add_argument('--tool', required=True, help='warp_decoder.exe or rav1g-dec.exe')
    parser.add_argument('--streams', required=True, help='directory written by prepare_streams.sh')
    parser.add_argument('--out', required=True, help='directory for the results')
    parser.add_argument('--default-timeout', type=int, default=600, help='seconds for a task without -timeout')
    parser.add_argument('--tool-args', default='', help='further arguments for every task, e.g. "-poisonbuf"')
    parser.add_argument('--title', default='', help='what this run is, for the heading of the summary')
    args = parser.parse_args()
    args.tool = os.path.abspath(args.tool)
    args.streams = os.path.abspath(args.streams)
    args.out = os.path.abspath(args.out)
    args.tool_args = args.tool_args.split()

    manifest = os.path.join(args.streams, 'manifest.txt')
    try:
        with open(manifest, encoding='utf-8') as f:
            lines = [l.strip() for l in f if l.strip() and not l.lstrip().startswith('#')]
    except OSError as error:
        print('cannot read %s: %s' % (manifest, error))
        return 2
    by_name = {conformance.task_name(line): line for line in lines}
    needed = sorted({stream for _, _, streams in SEQUENCES for stream in streams})
    absent = [stream for stream in needed if stream not in by_name]
    if absent:
        print('%s does not have: %s' % (manifest, ', '.join(absent)))
        return 2

    started = time.time()
    summary = []
    failed = 0
    for name, threads, streams in SEQUENCES:
        passed, lines, fatal = run_sequence(args, name, threads, [(stream, by_name[stream]) for stream in streams])
        if fatal:
            print(fatal)
            return 2
        summary += lines
        failed += 0 if passed else 1
    total_s = time.time() - started

    text = ['## Order check' + (': ' + args.title if args.title else ''), '',
            'Streams one after the other in one decoder process, each against its md5 reference; no stream is '
            'decoded again alone (testing/conformance/order_check.py).', '',
            '- %d sequences: **%d pass, %d fail**, %.0f s%s' % (
                len(SEQUENCES), len(SEQUENCES) - failed, failed, total_s,
                ('; further arguments: ' + ' '.join(args.tool_args)) if args.tool_args else '')]
    text += ['- ' + conformance.cell(line) for line in summary] + ['']
    text = '\n'.join(text)
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, 'summary.md'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(text)
    step_summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if step_summary:
        with open(step_summary, 'a', encoding='utf-8', newline='\n') as f:
            f.write(text)
    print()
    for line in summary:
        print(line)
    print('%d sequences: %d pass, %d fail, %.0f s' % (len(SEQUENCES), len(SEQUENCES) - failed, failed, total_s))
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
