#!/usr/bin/env python3
"""Equality of two conformance runs: the md5 of every output frame of every stream, and the stage dumps.

  python3 testing/conformance/compare_md5.py --reference <out of run.py> --candidate <out of run.py>
      [--manifest <manifest.txt>] [--reference-name warp_decoder] [--candidate-name rav1g-dec]
      [--reference-dumps <out of ci_dumps.py> --candidate-dumps <out of ci_dumps.py>]

Both directories are what testing/conformance/run.py wrote for the same streams with two decoder programs
(warp_decoder of the C++ decoder, branch cpp, and rav1g-dec: docs/rust-port.md, "Equality reference"). For every stream the two md5 files, one line per
output frame in the test player's format, must be the same bytes, and the two result lines must agree on the
status and the number of frames. With --manifest every stream of the manifest must be in both runs.

With the dump directories (testing/stage_dump/ci_dumps.py), the dumps of every stream that both have are
compared with stage_dump.py: the buffers behind every GPU stage of the dumped frames must be the same.

Exit code: 0 the runs are equal, 1 they differ, 2 a run is missing or incomplete.
"""
import argparse
import contextlib
import io
import os
import re
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'stage_dump'))


def read_results(directory):
    """stream -> (status, frames) from results.tsv, in the order of the run."""
    path = os.path.join(directory, 'results.tsv')
    results = {}
    with open(path, encoding='utf-8') as f:
        header = f.readline().rstrip('\n').split('\t')
        columns = {name: index for index, name in enumerate(header)}
        for line in f:
            fields = line.rstrip('\n').split('\t')
            if len(fields) < len(header):
                continue
            results[fields[columns['stream']]] = (fields[columns['status']], int(fields[columns['frames']]))
    return results


def read_md5(path):
    with open(path, 'rb') as f:
        return f.read()


def frame_lines(data):
    return [line for line in data.split(b'\n') if re.match(rb'[0-9a-f]{32}\s', line)]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--reference', required=True)
    parser.add_argument('--candidate', required=True)
    parser.add_argument('--reference-name', default='warp_decoder')
    parser.add_argument('--candidate-name', default='rav1g-dec')
    parser.add_argument('--manifest', help='manifest.txt of the streams: every stream must be in both runs')
    parser.add_argument('--reference-dumps')
    parser.add_argument('--candidate-dumps')
    args = parser.parse_args()
    a_name, b_name = args.reference_name, args.candidate_name

    try:
        a_results = read_results(args.reference)
        b_results = read_results(args.candidate)
    except (OSError, KeyError, ValueError) as error:
        print('compare_md5.py: a run has no readable results.tsv: %s' % error)
        return 2
    streams = list(a_results)
    problems = []
    incomplete = []
    for name in b_results:
        if name not in a_results:
            incomplete.append('%s: decoded by %s, not by %s' % (name, b_name, a_name))
    for name in streams:
        if name not in b_results:
            incomplete.append('%s: decoded by %s, not by %s' % (name, a_name, b_name))
    if args.manifest:
        with open(args.manifest, encoding='utf-8') as f:
            listed = [match.group(1) for match in (re.search(r'(?:^|\s)-name\s+(\S+)', line) for line in f) if match]
        if not listed:
            incomplete.append('%s lists no streams' % args.manifest)
        for name in listed:
            for results, program in ((a_results, a_name), (b_results, b_name)):
                if name not in results:
                    incomplete.append('%s: in the manifest, not decoded by %s' % (name, program))
    if not streams:
        incomplete.append('%s decoded no stream' % a_name)

    equal_streams = 0
    equal_frames = 0
    for name in streams:
        if name not in b_results:
            continue
        a_status, a_frames = a_results[name]
        b_status, b_frames = b_results[name]
        try:
            a_md5 = read_md5(os.path.join(args.reference, 'md5', name + '.md5'))
            b_md5 = read_md5(os.path.join(args.candidate, 'md5', name + '.md5'))
        except OSError as error:
            incomplete.append('%s: no md5 file: %s' % (name, error))
            continue
        a_lines, b_lines = frame_lines(a_md5), frame_lines(b_md5)
        if a_md5 != b_md5:
            first = next((i for i, (x, y) in enumerate(zip(a_lines, b_lines)) if x != y), min(len(a_lines), len(b_lines)))
            problems.append('%s: the md5 files differ, first at frame %d (%s: %d frames, %s: %d frames)' %
                            (name, first, a_name, len(a_lines), b_name, len(b_lines)))
        elif (a_status, a_frames) != (b_status, b_frames):
            problems.append('%s: %s says %s with %d frames, %s says %s with %d frames' %
                            (name, a_name, a_status, a_frames, b_name, b_status, b_frames))
        elif len(a_lines) != a_frames or not a_lines:
            problems.append('%s: %d md5 lines for %d frames' % (name, len(a_lines), a_frames))
        else:
            equal_streams += 1
            equal_frames += len(a_lines)

    dump_lines = []
    if args.reference_dumps and args.candidate_dumps:
        import stage_dump
        a_dirs = sorted(n for n in os.listdir(args.reference_dumps)
                        if n != 'streams' and os.path.isdir(os.path.join(args.reference_dumps, n)))
        b_dirs = sorted(n for n in os.listdir(args.candidate_dumps)
                        if n != 'streams' and os.path.isdir(os.path.join(args.candidate_dumps, n)))
        if not a_dirs or a_dirs != b_dirs:
            incomplete.append('the stage dumps are not of the same streams: %s has %s, %s has %s' %
                              (a_name, ', '.join(a_dirs) or 'none', b_name, ', '.join(b_dirs) or 'none'))
        for name in a_dirs:
            if name not in b_dirs:
                continue
            # stage_dump.py compare: its last line when the dumps are equal, all of its lines when not.
            text = io.StringIO()
            with contextlib.redirect_stdout(text):
                code = stage_dump.main_with(['compare', os.path.join(args.reference_dumps, name),
                                             os.path.join(args.candidate_dumps, name)])
            lines = text.getvalue().splitlines()
            print('== stage dumps of %s: %s' % (name, 'differ' if code else (lines[-1] if lines else 'nothing')))
            if code:
                for line in lines:
                    print('  ' + line)
                problems.append('%s: the stage dumps of %s and %s differ' % (name, a_name, b_name))
            else:
                dump_lines.append(name)

    summary = ['## %s against %s' % (b_name, a_name), '']
    summary.append('- md5 per output frame: %d of %d streams equal, %d frames' % (equal_streams, len(streams), equal_frames))
    if args.reference_dumps and args.candidate_dumps:
        summary.append('- stage dumps: equal for %d streams (%s)' % (len(dump_lines), ', '.join(dump_lines) or 'none'))
    for line in incomplete:
        summary.append('- incomplete: ' + line)
    for line in problems:
        summary.append('- differs: ' + line)
    text = '\n'.join(summary) + '\n'
    print(text)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write(text)
    if incomplete:
        return 2
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
