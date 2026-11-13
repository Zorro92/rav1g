#!/usr/bin/env python3
# Copyright (c) 2020, Alliance for Open Media. All rights reserved
#
# This source code is subject to the terms of the BSD 2 Clause License and
# the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
# was not distributed with this source code in the LICENSE file, you can
# obtain it at www.aomedia.org/license/software. If the Alliance for Open
# Media Patent License 1.0 was not distributed with this source code in the
# PATENTS file, you can obtain it at www.aomedia.org/license/patent.
"""Stage dumps of the conformance job: runs warp_decoder -dumpstages for the streams of a task list.

  ci_dumps.py --tool <warp_decoder.exe> --streams <directory of prepare_streams.sh> --tasks <dump_tasks.txt>
              --out <output directory> [--threads N]

Per line "<stream> <frames> [<temporal units>]" of the task list: decodes the stream (its line of
<streams>/manifest.txt, without the reference check) with -dumpstages <frames> into <out>/<stream>/, prints
what was dumped, compares the dumps with themselves (a check of the reader), and
  - keeps the dumps of an aom or svt stream, and copies an svt stream (a test pattern encoded in the job)
    to <out>/streams/;
  - replaces the dumps of a cut stream (published media) by their digests.
Exit code 1 when a decode failed or wrote no dump.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys

import stage_dump


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--tool', required=True)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--tasks', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--threads', type=int, default=4)
    parser.add_argument('--tool-args', default='', help='further arguments for the tool, e.g. "-parser rav1d"')
    args = parser.parse_args()
    tool = os.path.abspath(args.tool)
    out = os.path.abspath(args.out)

    inputs = {}
    with open(os.path.join(args.streams, 'manifest.txt'), encoding='utf-8') as f:
        for line in f:
            name = re.search(r'(?:^|\s)-name\s+(\S+)', line)
            path = re.search(r'(?:^|\s)-i\s+(\S+)', line)
            if name and path:
                inputs[name.group(1)] = (path.group(1), ' -aom_md5' in line)

    failed = 0
    with open(args.tasks, encoding='utf-8') as f:
        tasks = [line.split() for line in f if line.strip() and not line.startswith('#')]
    for parts in tasks:
        if len(parts) < 2 or parts[0] not in inputs:
            print('ERROR %s: not "<stream> <frames> [<temporal units>]" with a stream of the manifest' % ' '.join(parts))
            failed += 1
            continue
        name, frames = parts[0], parts[1]
        path, aom_md5 = inputs[name]
        kind = path.replace('\\', '/').split('/')[0]
        target = os.path.join(out, name)
        os.makedirs(target, exist_ok=True)
        command = [tool, '-name', name, '-i', path, '-t', str(args.threads), '-timeout', '1200',
                   '-dumpstages', frames, '-dumpdir', target, '-save_md5', os.path.join(target, name + '.md5')]
        command += args.tool_args.split()
        if aom_md5:
            command.append('-aom_md5')
        if len(parts) > 2:
            command += ['-limit', parts[2]]
        print('== %s (%s): frames %s' % (name, kind, frames), flush=True)
        try:
            run = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 stdin=subprocess.DEVNULL, timeout=1800)
            text = run.stdout.decode('utf-8', 'replace')
            code = run.returncode
        except (OSError, subprocess.TimeoutExpired) as error:
            text, code = str(error), -1
        with open(os.path.join(target, name + '.log'), 'w', encoding='utf-8', newline='\n') as log:
            log.write(text)
        for line in text.splitlines():
            if line.startswith(('RESULT', 'ERROR', 'DECODER ERROR', 'DUMPSTAGES ' + name)):
                print('  ' + line)
        dumps = sorted(n for n in os.listdir(target) if n.endswith('.stages'))
        if code != 0 or not dumps:
            print('ERROR %s: exit code %d, %d dump files' % (name, code, len(dumps)))
            failed += 1
            continue
        sys.stdout.flush()
        stage_dump.main_with(['list', target])
        # Every buffer of every dump read the way a comparison with another device reads it.
        if stage_dump.quiet_compare(target, target) != 0:
            print('ERROR %s: the dumps do not compare equal with themselves' % name)
            failed += 1
        if kind == 'cut':
            stage_dump.main_with(['digest', target, target])
            for n in dumps:
                os.remove(os.path.join(target, n))
            print('  %s is published media: digests only' % name)
        elif kind == 'svt':
            os.makedirs(os.path.join(out, 'streams'), exist_ok=True)
            shutil.copy(os.path.join(args.streams, path), os.path.join(out, 'streams', name))
            print('  %s is a test pattern encoded in this job: the stream is in streams/' % name)
        sys.stdout.flush()
    print('%d of %d dump tasks failed' % (failed, len(tasks)))
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
