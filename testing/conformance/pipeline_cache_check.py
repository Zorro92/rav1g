#!/usr/bin/env python3
"""The pipeline cache on disk (-pipelinecache; docs/integration.md, "The pipeline cache") on WARP.

  python testing/conformance/pipeline_cache_check.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>
                                                     [--stream <name>]

<dir> of --streams is what testing/conformance/prepare_streams.sh wrote; the stream (default
av1-1-b10-00-quantizer-00.ivf) is decoded against its reference with the task line of the manifest, each run
a process of its own with the same cache folder (<out>/cache, emptied first):

  1. through the Rust interface, into an empty folder that has a file of another key in it: the PIPELINES line
     says "cache miss" and the file is written; afterwards the folder has the one file of the key and the
     file of the other key is gone;
  2. through rav1g.h (-capi): "cache hit", every pipeline loaded from the file;
  3. the file cut to half its size: "cache rebuilt (...)" with the reason, and the file written again;
  4. "cache hit" again, from the file run 3 wrote;
  5. the file replaced by bytes that are no pipeline cache: "cache rebuilt (...)";
  6. with -pipelinecachekind library (a pipeline library, which is used only when asked for): "cache rebuilt
     (the file has cached pipeline states, ...)";
  7. the same: "cache hit" from the pipeline library;
  8. the default again: "cache rebuilt (the file has pipeline library, ...)".

Runs 1 to 5 and 8 have to keep the cached pipeline states (the default), 6 and 7 a pipeline library. Every
run has to give the frames of the reference, and no line may say that the device was removed. Written to --out: run<n>.log and
summary.md (also appended to $GITHUB_STEP_SUMMARY when that is set). Exit code 1 when a check fails, 2 when a
run could not be done.
"""
import argparse
import os
import re
import shutil
import subprocess
import sys

OTHER_KEY = 'rav1g-pipelines-0123456789abcdef.bin'


def cache_files(folder):
    return sorted(name for name in os.listdir(folder) if name.startswith('rav1g-pipelines-'))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--tool', required=True)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--stream', default='av1-1-b10-00-quantizer-00.ivf')
    args = parser.parse_args()
    tool = os.path.abspath(args.tool)
    out = os.path.abspath(args.out)
    os.makedirs(out, exist_ok=True)
    folder = os.path.join(out, 'cache')
    shutil.rmtree(folder, ignore_errors=True)
    os.makedirs(folder)

    task = None
    with open(os.path.join(args.streams, 'manifest.txt'), encoding='utf-8') as f:
        for line in f:
            name = re.search(r'(?:^|\s)-name\s+(\S+)', line)
            if name and name.group(1) == args.stream:
                task = line.split()
    if task is None:
        print('ERROR %s is not a stream of the manifest' % args.stream)
        return 2

    # A file of another key (another version of the library): the first write deletes it.
    with open(os.path.join(folder, OTHER_KEY), 'wb') as f:
        f.write(b'a pipeline cache of another key')

    problems = []
    lines = []

    def run(number, expect, extra=(), kind='cached pipeline states'):
        command = [tool] + task + ['-pipelinecache', folder] + list(extra)
        result = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        text = result.stdout.decode('utf-8', 'replace')
        with open(os.path.join(out, 'run%d.log' % number), 'w', encoding='utf-8') as f:
            f.write(' '.join(command) + '\n' + text)
        line = re.findall(r'^PIPELINES .*$', text, re.M)
        status = re.search(r'^RESULT\s.*?\bstatus=(\S+)', text, re.M)
        print('run %d (%s): exit code %d' % (number, expect, result.returncode))
        for found in line:
            print('  ' + found)
        lines.append('%d. expected `%s`%s: `%s`' % (number, expect, ' (%s)' % ' '.join(extra) if extra else '',
                                                    line[0] if line else 'no PIPELINES line'))
        if status is None:
            print('ERROR run %d: no RESULT line' % number)
            print(text[-3000:])
            return False
        if result.returncode != 0 or status.group(1) != 'pass':
            problems.append('run %d: the stream did not decode as its reference says: %s' % (number, status.group(0)))
        if len(line) != 1:
            problems.append('run %d: %d PIPELINES lines, not one' % (number, len(line)))
        elif not line[0].startswith('PIPELINES cache %s' % expect):
            problems.append('run %d: not "cache %s": %s' % (number, expect, line[0]))
        else:
            loaded = re.search(r'\((pipeline library|cached pipeline states), key ([0-9a-f]{16})', line[0])
            if not loaded:
                problems.append('run %d: the line does not say how the device keeps the pipelines' % number)
            elif loaded.group(1) != kind:
                problems.append('run %d: %s, not %s: %s' % (number, loaded.group(1), kind, line[0]))
            elif 'DEVICE REMOVED' in line[0]:
                problems.append('run %d: the device was removed: %s' % (number, line[0]))
            elif expect != 'hit' and 'written: ' not in line[0]:
                problems.append('run %d: the file was not written: %s' % (number, line[0]))
            elif expect == 'hit' and 'written' in line[0]:
                problems.append('run %d: a hit wrote the file: %s' % (number, line[0]))
        return True

    if not run(1, 'miss'):
        return 2
    files = cache_files(folder)
    if len(files) != 1 or files[0] == OTHER_KEY:
        problems.append('after run 1 the folder has %s, not the one file of the key' % files)
    if not run(2, 'hit', ['-capi']):
        return 2
    files = cache_files(folder)
    if len(files) == 1:
        path = os.path.join(folder, files[0])
        size = os.path.getsize(path)
        with open(path, 'r+b') as f:
            f.truncate(size // 2)
        print('cut %s from %d to %d bytes' % (files[0], size, size // 2))
    if not run(3, 'rebuilt (the file is damaged'):
        return 2
    if not run(4, 'hit'):
        return 2
    files = cache_files(folder)
    if len(files) == 1:
        with open(os.path.join(folder, files[0]), 'wb') as f:
            f.write(b'not a pipeline cache at all, but long enough to have a header of one')
    if not run(5, 'rebuilt (', ['-capi']):
        return 2
    library = ['-pipelinecachekind', 'library']
    if not run(6, 'rebuilt (the file has cached pipeline states', library, 'pipeline library'):
        return 2
    if not run(7, 'hit', library + ['-capi'], 'pipeline library'):
        return 2
    if not run(8, 'rebuilt (the file has pipeline library'):
        return 2
    files = cache_files(folder)
    if len(files) != 1:
        problems.append('at the end the folder has %s, not one file' % files)

    summary = ['## Pipeline cache on WARP (-pipelinecache)', '', 'Stream %s, one process per run, one cache folder.'
               % args.stream, ''] + lines + ['']
    summary += ['Problems:', ''] + ['- ' + problem for problem in problems] if problems else ['All checks passed.']
    text = '\n'.join(summary) + '\n'
    with open(os.path.join(out, 'summary.md'), 'w', encoding='utf-8') as f:
        f.write(text)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write(text)
    for problem in problems:
        print('PROBLEM ' + problem)
    print('pipeline_cache_check.py: %d problems' % len(problems))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
