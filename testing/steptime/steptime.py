#!/usr/bin/env python3
"""Intra step timing: the cost model of an intra wavefront step, from the STEPTIME lines of rav1g-dec or
of the test player (-intrasteptime, -intrastepmin; docs/test-player.md, "Intra step timing").

    steptime.py fit LOG... [--save MODEL.json] [--min-steps N]
        fits the cost of a step on the console from the timed steps: a fixed cost per step, plus terms for
        what the step holds (thread groups, the longest filter intra chain, and a cost for each kind of
        block the step has at all, since a step lasts as long as its slowest group), and a frame model
        from the counts the proxy tools give (steps, thread groups, dispatches). Runs of the intra_run
        shader (several iterations in one thread group, `iters=` of the step lines) are left out of the
        step fit and fitted apart: us = fixed + per iteration + per chunk of 512 lanes, with the largest
        iteration that is cheaper in a run than in a step of its own (for -intrarunmax). Prints the fits
        and, with --save, writes the models.
    steptime.py report LOG... [--model MODEL.json] [--frames N] [--slowest N]
        where the time of the heaviest timed frames goes: per frame its steps, the share of the fixed
        cost, the time of the steps by what dominates them, and the slowest steps.
    steptime.py predict --model MODEL.json LOG...
        estimates the intra time of each frame on the console: from STEPTIME lines of any run (the counts
        of a local run under WARP or lavapipe are exact, their times are not used), or from the
        'GPUTIME frame' lines of -gputime -progress (steps, thread groups, dispatches only).
    steptime.py selftest
        the parser and the fit on made-up steps with known costs (CI and testing/precheck.sh).

A LOG is a file with the lines anywhere in it (a player log, rav1g-dec's output); '-' is stdin. Needs
nothing but Python 3.
"""
import argparse
import json
import math
import random
import re
import sys

KINDS = ['dc', 'vh', 'zone1', 'zone2', 'zone3', 'smooth', 'paeth', 'cfl', 'filter', 'intrabc', 'interintra']
LISTS = ['64x64', '32x64', '32x32', '16x32', '16x16', '8x16', '8x8', '4x8', '4x4', 'filter']

FRAME_RE = re.compile(r'STEPTIME frame (.*)$')
STEP_RE = re.compile(r'STEPTIME step (.*)$')
GPUTIME_RE = re.compile(r'GPUTIME frame (\d+) (.*)$')


def fields(text):
    out = {}
    for item in text.split():
        if '=' in item:
            key, value = item.split('=', 1)
            out[key] = value
    return out


def ints(text):
    return [int(v) for v in text.split(',')] if text else []


def read_lines(paths):
    for path in paths:
        handle = sys.stdin if path == '-' else open(path, encoding='utf-8', errors='replace')
        with handle:
            for line in handle:
                yield line.rstrip('\n')


def parse(lines):
    """The timed frames: dicts with the fields of the frame line and 'steps', a list of step dicts."""
    frames = []
    current = None
    for line in lines:
        m = FRAME_RE.search(line)
        if m:
            f = fields(m.group(1))
            current = {
                'name': f.get('name', ''), 'no': int(f['no']), 'show': int(f.get('show', 0)),
                'intra': int(f.get('intra', 0)), 'bd': int(f.get('bd', 8)), 'tiles': int(f.get('tiles', 1)),
                'step_count': int(f['steps']), 'gpu_ms': float(f.get('gpu_ms', 0)), 'steps': [],
            }
            frames.append(current)
            continue
        m = STEP_RE.search(line)
        if m and current is not None:
            f = fields(m.group(1))
            if int(f['no']) != current['no']:
                continue
            current['steps'].append({
                'i': int(f['i']), 'us': float(f['us']), 'groups': int(f['groups']), 'disp': int(f['disp']),
                'lists': ints(f.get('lists', '')), 'y': ints(f.get('y', '')), 'c': ints(f.get('c', '')),
                'fdiag': int(f.get('fdiag', 0)),
                # Wavefront iterations of the step's dispatch: 1; a run of the intra_run shader is its last
                # iteration with the run's count, behind steps of 0 for the others (lines without it: 1).
                'iters': int(f.get('iters', 1)),
            })
    return frames


def parse_gputime(lines):
    """'GPUTIME frame' lines: per decoded frame its steps, dispatches and thread groups of intra."""
    out = []
    for line in lines:
        m = GPUTIME_RE.search(line)
        if not m:
            continue
        f = fields(m.group(2))
        if 'iters' not in f or int(f['iters']) < 0 or f.get('iv2', '0') != '0':
            continue
        out.append({'no': int(f['no']), 'show': int(f.get('show', 0)), 'steps': int(f['iters']) + 1,
                    'disp': int(f.get('idisp', 0)), 'groups': int(f.get('igroups', 0)),
                    'intra_ms': float(f['intra']) if 'intra' in f else None})
    return out


# ---------------------------------------------------------------------------------------------------------
# The models.
#
# Step model: us = fixed + per_group * groups + per_diagonal * fdiag + sum over kinds k present in the step
# of has[k] + ten_bit * (bit depth 10). "Present": the step has a block of the kind in either plane. The
# terms are what a step's latency follows: the slowest group of its dispatches sets it, so a kind costs
# what its blocks add to the slowest group, once, not per block; thread groups stand for the width of the
# step; a filter intra block runs its diagonals one after the other (fdiag).
#
# Frame model: ms = per_step * steps + per_group * groups + per_dispatch * dispatches, for the counts of
# 'GPUTIME frame' lines.
# ---------------------------------------------------------------------------------------------------------

STEP_TERMS = ['fixed', 'per_group', 'per_diagonal'] + ['has_' + k for k in KINDS] + ['ten_bit']
FRAME_TERMS = ['per_step', 'per_group', 'per_dispatch']


def step_row(step, bd):
    present = [1.0 if step['y'][k] + step['c'][k] > 0 else 0.0 for k in range(len(KINDS))]
    return [1.0, float(step['groups']), float(step['fdiag'])] + present + [1.0 if bd > 8 else 0.0]


def solve(m, v):
    """m x = v for a small square m (Gaussian elimination with partial pivoting); a singular direction
    gets 0."""
    n = len(v)
    a = [row[:] + [v[i]] for i, row in enumerate(m)]
    for col in range(n):
        pivot = max(range(col, n), key=lambda r: abs(a[r][col]))
        if abs(a[pivot][col]) < 1e-12:
            continue
        a[col], a[pivot] = a[pivot], a[col]
        for r in range(n):
            if r != col and a[r][col] != 0.0:
                f = a[r][col] / a[col][col]
                a[r] = [x - f * y for x, y in zip(a[r], a[col])]
    return [a[i][n] / a[i][i] if abs(a[i][i]) >= 1e-12 else 0.0 for i in range(n)]


def nnls(rows, y, iterations=200):
    """min |A x - y| with x >= 0 (the active set method of Lawson and Hanson), on the normal equations: the
    terms are few, the steps many."""
    n = len(rows[0])
    ata = [[0.0] * n for _ in range(n)]
    aty = [0.0] * n
    for row, value in zip(rows, y):
        for i in range(n):
            ri = row[i]
            if ri == 0.0:
                continue
            aty[i] += ri * value
            r = ata[i]
            for j in range(n):
                r[j] += ri * row[j]

    def gradient(x):
        return [aty[i] - sum(ata[i][j] * x[j] for j in range(n)) for i in range(n)]

    def solve_on(passive):
        idx = [i for i in range(n) if passive[i]]
        sub = solve([[ata[i][j] for j in idx] for i in idx], [aty[i] for i in idx])
        z = [0.0] * n
        for k, i in enumerate(idx):
            z[i] = sub[k]
        return z

    x = [0.0] * n
    passive = [False] * n
    scale = max(1.0, max(abs(v) for v in aty))
    for _ in range(iterations):
        w = gradient(x)
        free = [i for i in range(n) if not passive[i] and w[i] > 1e-9 * scale]
        if not free:
            break
        passive[max(free, key=lambda i: w[i])] = True
        for _ in range(iterations):
            z = solve_on(passive)
            if all(z[i] > 0 for i in range(n) if passive[i]):
                x = z
                break
            alpha = min(x[i] / (x[i] - z[i]) for i in range(n) if passive[i] and z[i] <= 0 and x[i] - z[i] > 0) \
                if any(passive[i] and z[i] <= 0 and x[i] - z[i] > 0 for i in range(n)) else 0.0
            x = [x[i] + alpha * (z[i] - x[i]) for i in range(n)]
            for i in range(n):
                if passive[i] and x[i] <= 1e-12:
                    passive[i] = False
                    x[i] = 0.0
    return x


def fit_steps(frames, min_steps=0, skip_first=True):
    rows, ys = [], []
    for f in frames:
        if f['step_count'] <= min_steps:
            continue
        for s in f['steps']:
            # The first step's time starts at the timestamp in front of the stage: it can hold the end of
            # the work before it (the GPU may take a timestamp before what was recorded before it has
            # drained), so it is left out of the fit.
            if skip_first and s['i'] == 0:
                continue
            # Runs (several iterations in one dispatch) are fitted apart (fit_runs).
            if s['iters'] != 1:
                continue
            rows.append(step_row(s, f['bd']))
            ys.append(s['us'])
    if not rows:
        raise SystemExit('steptime.py: no timed steps to fit')
    width = len(rows[0])
    # A term that has the same value in every step (one bit depth, a kind every step has) cannot be told
    # from the fixed cost: it is left out of the fit and reported as such.
    constant = [j > 0 and min(r[j] for r in rows) == max(r[j] for r in rows) for j in range(width)]
    keep = [j for j in range(width) if not constant[j]]
    fitted = nnls([[r[j] for j in keep] for r in rows], ys)
    x = [0.0] * width
    for k, j in enumerate(keep):
        x[j] = fitted[k]
    residual = [v - sum(a * b for a, b in zip(r, x)) for r, v in zip(rows, ys)]
    mean = sum(ys) / len(ys)
    total = sum((v - mean) ** 2 for v in ys)
    r2 = 1.0 - sum(e * e for e in residual) / total if total > 0 else 1.0
    return {
        'terms': dict(zip(STEP_TERMS, (float(v) for v in x))),
        'not_fitted': [STEP_TERMS[j] for j in range(len(STEP_TERMS)) if constant[j]],
        'steps': len(ys), 'r2': float(r2), 'mean_abs_us': sum(abs(e) for e in residual) / len(ys),
        'mean_us': mean,
    }


RUN_GROUP_LANES = 512  # threads of the intra_run group: an iteration takes one pass of the body per 512 lanes


def lanes(step):
    """The main shader's lanes of a step's iteration: one per four samples of a row (prediction.rs)."""
    lists = step['lists']
    return sum(lists[k] << (10 - k) for k in range(min(8, len(lists)))) + (lists[8] << 2 if len(lists) > 8 else 0)


def runs_of(frames):
    """The runs of the frames: per run its time, iterations and chunks (passes of the group's body)."""
    out = []
    for f in frames:
        steps = f['steps']
        for at, s in enumerate(steps):
            n = s['iters']
            if n < 2 or at + 1 < n:
                continue
            its = steps[at + 1 - n:at + 1]
            chunks = sum(-(-lanes(t) // RUN_GROUP_LANES) for t in its)
            # The steps in front of the last one were timed in front of the run: their time is the run's too.
            out.append({'us': sum(t['us'] for t in its), 'iters': n, 'chunks': chunks})
    return out


def fit_runs(frames):
    """The run model: us = fixed + per_iteration * iterations + per_chunk * chunks, or None without runs."""
    runs = runs_of(frames)
    if len(runs) < 3:
        return None
    rows = [[1.0, float(r['iters']), float(r['chunks'])] for r in runs]
    ys = [r['us'] for r in runs]
    x = nnls(rows, ys)
    mean = sum(ys) / len(ys)
    residual = [v - sum(a * b for a, b in zip(row, x)) for row, v in zip(rows, ys)]
    total = sum((v - mean) ** 2 for v in ys)
    return {'terms': {'fixed': x[0], 'per_iteration': x[1], 'per_chunk': x[2]}, 'runs': len(runs),
            'iterations': sum(r['iters'] for r in runs), 'mean_us': mean,
            'r2': 1.0 - sum(e * e for e in residual) / total if total > 0 else 1.0}


def fit_frames(frames, step_model):
    """The frame model, fitted to the frames' sums of the step model's predictions, which keeps the first
    steps and the frames with few steps out of it as well."""
    rows, ys = [], []
    for f in frames:
        if not f['steps']:
            continue
        groups = sum(s['groups'] for s in f['steps'])
        disp = sum(s['disp'] for s in f['steps'])
        rows.append([float(len(f['steps'])), float(groups), float(disp)])
        ys.append(sum(predict_step(step_model, s, f['bd']) for s in f['steps']) / 1000.0)
    if not rows:
        raise SystemExit('steptime.py: no timed frames to fit')
    x = nnls(rows, ys)
    return {'terms': dict(zip(FRAME_TERMS, (float(v) for v in x))), 'frames': len(ys)}


def predict_step(model, step, bd):
    terms = model['terms']
    return sum(terms[name] * value for name, value in zip(STEP_TERMS, step_row(step, bd)))


def predict_frame(model, steps, groups, disp):
    t = model['terms']
    return t['per_step'] * steps + t['per_group'] * groups + t['per_dispatch'] * disp


# ---------------------------------------------------------------------------------------------------------
# Output.
# ---------------------------------------------------------------------------------------------------------

def describe_step(step):
    parts = []
    for plane, counts in (('Y', step['y']), ('C', step['c'])):
        kinds = ['%s %d' % (KINDS[k], n) for k, n in enumerate(counts) if n]
        if kinds:
            parts.append('%s: %s' % (plane, ', '.join(kinds)))
    sizes = ['%s %d' % (LISTS[k], n) for k, n in enumerate(step['lists']) if n]
    return '%s | lists %s%s' % ('; '.join(parts), ', '.join(sizes),
                                ' | filter chain %d' % step['fdiag'] if step['fdiag'] else '')


def dominant(model, step, bd):
    """What adds the most to a step above its fixed cost, by the model; without a model the kind with the
    most blocks."""
    if model:
        t = model['terms']
        best, best_cost = 'fixed', 0.0
        for k, name in enumerate(KINDS):
            if step['y'][k] + step['c'][k] and t['has_' + name] > best_cost:
                best, best_cost = name, t['has_' + name]
        if t['per_diagonal'] * step['fdiag'] > best_cost:
            best = 'filter chain'
        return best
    counts = [step['y'][k] + step['c'][k] for k in range(len(KINDS))]
    return KINDS[max(range(len(KINDS)), key=lambda k: counts[k])] if any(counts) else 'empty'


def print_fit(step_model, frame_model):
    t = step_model['terms']
    skipped = step_model.get('not_fitted', [])
    print('step model: %d steps, mean %.1f us, R^2 %.3f, mean error %.1f us' %
          (step_model['steps'], step_model['mean_us'], step_model['r2'], step_model['mean_abs_us']))
    print('  fixed cost per step      %8.2f us' % t['fixed'])
    print('  per thread group         %8.3f us' % t['per_group'])
    print('  per filter intra diagonal%8.3f us (the longest filter intra chain of the step)' % t['per_diagonal'])
    for name in KINDS:
        note = ' (the same in every step: not fitted)' if 'has_' + name in skipped else ''
        print('  step has %-15s %8.2f us%s' % (name, t['has_' + name], note))
    print('  10-bit frame             %8.2f us%s' % (t['ten_bit'], ' (one bit depth: not fitted)' if 'ten_bit' in skipped else ''))
    f = frame_model['terms']
    print('frame model (GPUTIME counts): ms = %.4f * steps + %.6f * groups + %.5f * dispatches (%d frames)' %
          (f['per_step'], f['per_group'], f['per_dispatch'], frame_model['frames']))


def print_runs(run_model, step_model):
    if not run_model:
        print('runs: none in the timed frames (-intrarunlegacy, or no iteration narrow enough)')
        return
    t = run_model['terms']
    print('run model: %d runs of %d iterations, mean %.1f us a run, R^2 %.3f' %
          (run_model['runs'], run_model['iterations'], run_model['mean_us'], run_model['r2']))
    print('  us = %.2f + %.2f per iteration + %.2f per chunk of %d lanes' %
          (t['fixed'], t['per_iteration'], t['per_chunk'], RUN_GROUP_LANES))
    if step_model:
        # An iteration of k chunks in a run against a step of its own with one kind (the fixed cost): the
        # largest k for which the run is cheaper, for -intrarunmax (k * 512 lanes).
        own = step_model['terms']['fixed']
        if t['per_chunk'] > 0:
            k = int((own - t['per_iteration']) / t['per_chunk'])
            print('  an iteration is cheaper in a run up to about %d chunks (-intrarunmax %d) against the %.1f us a step '
                  'of its own has at least' % (k, k * RUN_GROUP_LANES, own))


def report(frames, model, top, slowest):
    heavy = sorted(frames, key=lambda f: -f['gpu_ms'])[:top]
    for f in heavy:
        steps = f['steps']
        total_us = sum(s['us'] for s in steps)
        print('frame no=%d %s show=%d intra=%d bd=%d tiles=%d: %d steps, %.2f ms (first to last timestamp %.2f ms)' %
              (f['no'], f['name'], f['show'], f['intra'], f['bd'], f['tiles'], len(steps), total_us / 1000.0,
               f['gpu_ms']))
        if not steps:
            continue
        times = sorted(s['us'] for s in steps)
        median = times[len(times) // 2]
        print('  step time: median %.1f us, p90 %.1f us, max %.1f us; first step %.1f us' %
              (median, times[int(len(times) * 0.9) - 1 if len(times) > 1 else 0], times[-1], steps[0]['us']))
        if model:
            fixed = model['terms']['fixed'] * len(steps)
            predicted = sum(predict_step(model, s, f['bd']) for s in steps)
            print('  fixed cost %.2f ms = %.0f%% of the steps\' time; the model predicts %.2f ms' %
                  (fixed / 1000.0, 100.0 * fixed / total_us if total_us else 0.0, predicted / 1000.0))
        by_kind = {}
        for s in steps:
            key = dominant(model, s, f['bd'])
            n, us = by_kind.get(key, (0, 0.0))
            by_kind[key] = (n + 1, us + s['us'])
        print('  steps by what dominates them:')
        for key, (n, us) in sorted(by_kind.items(), key=lambda kv: -kv[1][1]):
            print('    %-14s %5d steps %8.2f ms (%4.1f%%), %6.1f us a step' %
                  (key, n, us / 1000.0, 100.0 * us / total_us if total_us else 0.0, us / n))
        print('  slowest steps:')
        for s in sorted(steps, key=lambda s: -s['us'])[:slowest]:
            print('    i=%-5d %8.1f us  groups %-5d %s' % (s['i'], s['us'], s['groups'], describe_step(s)))


def predict(model, paths):
    lines = list(read_lines(paths))
    frames = parse(lines)
    if frames:
        print('frame                       steps   groups  predicted ms (step model)')
        for f in frames:
            ms = sum(predict_step(model['step'], s, f['bd']) for s in f['steps']) / 1000.0
            groups = sum(s['groups'] for s in f['steps'])
            print('no=%-6d show=%d %-12s %6d %8d  %8.2f' % (f['no'], f['show'], f['name'][:12], len(f['steps']), groups, ms))
        total = sum(sum(predict_step(model['step'], s, f['bd']) for s in f['steps']) for f in frames) / 1000.0
        print('total %.2f ms over %d frames' % (total, len(frames)))
        return
    rows = parse_gputime(lines)
    if not rows:
        raise SystemExit('steptime.py: no STEPTIME or GPUTIME frame lines')
    print('frame   show  steps   groups  disp  predicted ms (frame model)  measured intra ms (this run)')
    for r in rows:
        ms = predict_frame(model['frame'], r['steps'], r['groups'], r['disp'])
        print('no=%-6d %d  %6d %8d %5d  %8.2f                  %s' %
              (r['no'], r['show'], r['steps'], r['groups'], r['disp'], ms,
               '%.2f' % r['intra_ms'] if r['intra_ms'] is not None else '-'))
    heaviest = sorted(rows, key=lambda r: -predict_frame(model['frame'], r['steps'], r['groups'], r['disp']))[:5]
    print('heaviest by the model: ' + ', '.join('no=%d %.2f ms' % (
        r['no'], predict_frame(model['frame'], r['steps'], r['groups'], r['disp'])) for r in heaviest))


# ---------------------------------------------------------------------------------------------------------
# Self-test.
# ---------------------------------------------------------------------------------------------------------

def selftest():
    rng = random.Random(7)
    truth = {name: 0.0 for name in STEP_TERMS}
    truth.update({'fixed': 22.0, 'per_group': 0.05, 'per_diagonal': 1.5, 'has_zone2': 6.0, 'has_cfl': 3.0,
                  'has_filter': 2.0, 'ten_bit': 1.0})
    lines = []
    for no in range(6):
        bd = 10 if no % 2 else 8
        steps = 150 + 40 * no
        lines.append('STEPTIME frame name=t%d.ivf no=%d show=1 intra=%d bd=%d tiles=1 steps=%d gpu_ms=0' %
                     (no, no, no == 0, bd, steps))
        for i in range(steps):
            y = [rng.randint(0, 3) if rng.random() < 0.4 else 0 for _ in KINDS]
            c = [rng.randint(0, 2) if rng.random() < 0.3 else 0 for _ in KINDS]
            if not any(y) and not any(c):
                y[0] = 1
            fdiag = (rng.choice([2, 5, 11, 23]) if y[8] else 0)
            groups = rng.randint(1, 120)
            step = {'y': y, 'c': c, 'fdiag': fdiag, 'groups': groups}
            us = sum(truth[name] * value for name, value in zip(STEP_TERMS, step_row(step, bd)))
            us += rng.gauss(0.0, 0.3) + (500.0 if i == 0 else 0.0)  # the first step holds earlier work
            lists = [rng.randint(0, 4) for _ in LISTS]
            lines.append('STEPTIME step no=%d i=%d us=%.1f groups=%d disp=%d lists=%s y=%s c=%s fdiag=%d' %
                         (no, i, us, groups, 1 + (fdiag > 0), ','.join(map(str, lists)), ','.join(map(str, y)),
                          ','.join(map(str, c)), fdiag))
    lines.append('STEPTIME step no=99 i=0 us=1 groups=1 disp=1 lists= y= c= fdiag=0')  # of no frame: ignored
    frames = parse(lines)
    assert len(frames) == 6 and all(len(f['steps']) == f['step_count'] for f in frames), 'parse'
    model = fit_steps(frames)
    for name, value in truth.items():
        got = model['terms'][name]
        assert abs(got - value) < 0.25 + 0.02 * value, 'fit of %s: %.3f, made with %.3f' % (name, got, value)
    assert model['r2'] > 0.99, 'R^2 %.3f' % model['r2']
    frame_model = fit_frames(frames, model)
    # Runs: a run's time follows its iterations and chunks, and runs stay out of the step fit.
    run_lines = ['STEPTIME frame name=r.ivf no=50 show=1 intra=0 bd=10 tiles=1 steps=40 gpu_ms=0']
    for i in range(40):
        n = 4 if i % 4 == 3 else 0
        lists = [0, 0, 0, 0, 0, 0, 40 * ((i * 7) % 5), 0, 8 + i, 0]
        chunks_here = 0
        if n:
            for k in range(i - 3, i + 1):
                chunks_here += -(-(40 * ((k * 7) % 5) * 16 + (8 + k) * 4) // RUN_GROUP_LANES)
        us = 20.0 + 3.0 * n + 2.0 * chunks_here if n else 0.0
        run_lines.append('STEPTIME step no=50 i=%d us=%.1f groups=%d disp=%d lists=%s y=1,0,0,0,0,0,0,0,0,0,0 '
                         'c=0,0,0,0,0,0,0,0,0,0,0 fdiag=0 iters=%d' % (i, us, 1 if n else 0, 1 if n else 0,
                                                                     ','.join(map(str, lists)), n))
    runs = fit_runs(parse(run_lines))
    assert runs and runs['runs'] == 10, runs
    assert abs(runs['terms']['fixed'] + 4 * runs['terms']['per_iteration'] - 32.0) < 1.0, runs
    assert abs(runs['terms']['per_chunk'] - 2.0) < 0.1 and runs['r2'] > 0.99, runs
    # The frame model is close to the step model on the frames it was fitted to.
    for f in frames:
        groups = sum(s['groups'] for s in f['steps'])
        disp = sum(s['disp'] for s in f['steps'])
        by_steps = sum(predict_step(model, s, f['bd']) for s in f['steps']) / 1000.0
        by_frame = predict_frame(frame_model, len(f['steps']), groups, disp)
        assert abs(by_frame - by_steps) < 0.15 * by_steps, 'frame model %.2f, steps %.2f' % (by_frame, by_steps)
    rows = parse_gputime(['GPUTIME frame 3 no=2 show=1 sef=0 intra=1 iters=896 iv2=0 idisp=897 ibarr=897 '
                          'igroups=12664 ipasses=12664 tiles=1 | intra=27.10 cdef=3.0'])
    assert rows == [{'no': 2, 'show': 1, 'steps': 897, 'disp': 897, 'groups': 12664, 'intra_ms': 27.1}], rows
    print('steptime.py selftest: parse, step fit (R^2 %.4f), run fit and frame model OK' % model['r2'])


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('fit')
    p.add_argument('logs', nargs='+')
    p.add_argument('--save')
    p.add_argument('--min-steps', type=int, default=0, help='fit only frames with more steps than this')
    p = sub.add_parser('report')
    p.add_argument('logs', nargs='+')
    p.add_argument('--model')
    p.add_argument('--frames', type=int, default=5)
    p.add_argument('--slowest', type=int, default=10)
    p = sub.add_parser('predict')
    p.add_argument('logs', nargs='+')
    p.add_argument('--model', required=True)
    sub.add_parser('selftest')
    args = parser.parse_args()

    if args.command == 'selftest':
        selftest()
        return
    if args.command == 'fit':
        frames = parse(read_lines(args.logs))
        step_model = fit_steps(frames, args.min_steps)
        frame_model = fit_frames(frames, step_model)
        print_fit(step_model, frame_model)
        run_model = fit_runs(frames)
        print_runs(run_model, step_model)
        if args.save:
            with open(args.save, 'w', encoding='utf-8') as out:
                json.dump({'step': step_model, 'frame': frame_model, 'run': run_model}, out, indent=1)
            print('model written to %s' % args.save)
        return
    if args.command == 'report':
        frames = parse(read_lines(args.logs))
        model = None
        if args.model:
            with open(args.model, encoding='utf-8') as handle:
                model = json.load(handle)['step']
        report(frames, model, args.frames, args.slowest)
        return
    with open(args.model, encoding='utf-8') as handle:
        model = json.load(handle)
    predict(model, args.logs)


if __name__ == '__main__':
    main()
