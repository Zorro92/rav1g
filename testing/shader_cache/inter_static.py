#!/usr/bin/env python3
"""The static view of the inter prediction shaders, from the FXC listings of a build.

    python3 testing/shader_cache/inter_static.py --listings <dir with *.asm> [--out inter-static.md]
    python3 testing/shader_cache/inter_static.py --self-test

For every inter shader two things are put side by side:

  * what the listing says: instruction slots, temp registers, group memory, the loads and stores it has
    (instructions and bytes, by buffer), its loops and group syncs. Static counts: instructions in the
    listing, not instructions executed.
  * what a thread of the shader does, read from the HLSL source (the table MODEL below): the samples it
    writes, the references it reads, the rows of a reference it loads and the bytes of the loads of one row.
    From that: reference loads and bytes per sample written.

Where a shader has no loop around its reference loads the two must agree (the loads of the frame buffer in
the listing are the rows of the model plus the loads of the destination), and the script says so per shader;
with --strict a difference is an error. The shaders for scaled references load their rows in a loop whose
count depends on the scale; for those the model's rows are the count for a reference of the frame's size.

docs/gpu-av1-decoder.md, "Inter prediction breakdown", has the table of the current shaders.
"""

import argparse
import glob
import os
import re
import sys

# Buffers of the inter shaders (inter_common.h).
BUFFERS = {'t0': 'blocks', 't1': 'residuals', 't2': 'mask', 't3': 'warp blocks', 't4': 'warp filter', 'u0': 'frame'}

# What one thread does, from the source. Keys: the shader without _hbd.
#   section  the section of the inter breakdown that runs it (docs/test-player.md), '-' when nothing does
#   out      samples one thread writes, as (width, height)
#   refs     references it reads
#   rows     rows of a reference one thread loads (for the scaled shaders: with a reference of the frame's size)
#   row      bytes of the loads of one row, (8-bit, 10-bit)
#   dst      bytes it loads from the destination (the frame it writes), (8-bit, 10-bit)
#   reuse    how the rows get from the loads to the samples
MODEL = {
    'inter_main': dict(section='single', out=(4, 4), refs=1, rows=11, row=(16, 24), dst=(0, 0),
                       reuse='none: each thread filters its own 11 rows of 4 samples (8 taps) and sums them '
                             'vertically into its 4 rows'),
    'inter_compound': dict(section='comp_avg', out=(4, 2), refs=2, rows=9, row=(16, 24), dst=(0, 0),
                           reuse='none: a thread is a 4x2 and loads 9 rows of each reference'),
    'inter_compound_masked': dict(section='comp_masked', out=(4, 2), refs=2, rows=9, row=(16, 24), dst=(0, 0),
                                  reuse='none; the wedge mask is two loads of the mask table'),
    'inter_compound_diff_luma': dict(section='comp_diff_y', out=(4, 2), refs=2, rows=9, row=(16, 24), dst=(0, 0),
                                     reuse='none for the rows; the mask sums of two threads go through the '
                                           'group memory into the chroma planes (one sync)'),
    'inter_compound_diff_chroma': dict(section='comp_diff_uv', out=(4, 2), refs=2, rows=9, row=(16, 24),
                                       dst=(8, 8),
                                       reuse='none; the mask is loaded from the plane the luma pass wrote it to'),
    'inter_2x2chroma': dict(section='chroma_2x2', out=(2, 2), refs=1, rows=9, row=(12, 20), dst=(8, 0),
                            reuse='none: a thread is a 2x2 and loads 9 rows; 8-bit merges its two samples '
                                  'into the word of the neighbour (group memory, one sync)'),
    'inter_compound_2x2': dict(section='-', out=(2, 2), refs=2, rows=9, row=(12, 20), dst=(8, 0),
                               reuse='none (no block is ever in its list: a block under 8x8 has one reference)'),
    'inter_obmc_above': dict(section='obmc_above', out=(4, 4), refs=1, rows=11, row=(16, 24), dst=(16, 32),
                             reuse='none; reads the 4 rows it blends into'),
    'inter_obmc_left': dict(section='obmc_left', out=(4, 4), refs=1, rows=11, row=(16, 24), dst=(16, 32),
                            reuse='none; reads the 4 rows it blends into'),
    'inter_warp': dict(section='warp', out=(4, 4), refs=1, rows=11, row=(16, 24), dst=(0, 0),
                       reuse='none: the 11 filtered rows go through the group memory, but each thread reads '
                             'only its own; 88 + 32 loads of the warp filter table per thread'),
    'inter_warp_compound': dict(section='-', out=(4, 1), refs=2, rows=2.75, row=(16, 24), dst=(4, 4),
                                reuse='across the 4 threads of a 4x4 (not in the program: the pipeline state '
                                      'is built from inter_scale_warp_compound)'),
    'inter_scale': dict(section='single/scaled', out=(4, 1), refs=1, rows=11, row=(12, 20), dst=(0, 0),
                        reuse='across the 4 threads of a 4x4: each filters one column of 11+ rows into the '
                              'group memory, after a sync each filters one output row from the 4 columns'),
    'inter_scale_compound': dict(section='comp_avg/scaled', out=(4, 1), refs=2, rows=11, row=(12, 20), dst=(0, 0),
                                 reuse='as inter_scale, once per reference (3 syncs)'),
    'inter_scale_compound_masked': dict(section='comp_masked/scaled', out=(4, 1), refs=2, rows=11, row=(12, 20),
                                        dst=(0, 0), reuse='as inter_scale, once per reference'),
    'inter_scale_compound_diff_luma': dict(section='comp_diff_y/scaled', out=(4, 1), refs=2, rows=11,
                                           row=(12, 20), dst=(0, 0),
                                           reuse='as inter_scale, once per reference; the mask through the '
                                                 'group memory (5 syncs)'),
    'inter_scale_compound_diff_chroma': dict(section='comp_diff_uv/scaled', out=(4, 1), refs=2, rows=11,
                                             row=(12, 20), dst=(4, 4), reuse='as inter_scale, once per reference'),
    'inter_scale_2x2chroma': dict(section='chroma_2x2/scaled', out=(2, 1), refs=1, rows=9, row=(12, 20), dst=(4, 0),
                                  reuse='across the 2 threads of a 2x2, as inter_scale'),
    'inter_scale_compound_2x2': dict(section='-', out=(2, 1), refs=2, rows=9, row=(12, 20), dst=(4, 0),
                                     reuse='as inter_scale_2x2chroma (not in the program)'),
    'inter_scale_obmc_above': dict(section='obmc_above/scaled', out=(4, 1), refs=1, rows=11, row=(12, 20),
                                   dst=(4, 8), reuse='as inter_scale; reads the row it blends into'),
    'inter_scale_obmc_left': dict(section='obmc_left/scaled', out=(4, 1), refs=1, rows=11, row=(12, 20),
                                  dst=(4, 8), reuse='as inter_scale; reads the row it blends into'),
    'inter_scale_warp_compound': dict(section='comp_warp, comp_warp_diff_uv (scaled or not)', out=(4, 1), refs=2,
                                      rows=11, row=(12, 20), dst=(4, 4),
                                      reuse='across the 4 threads of a 4x4: a column of 11 rows per thread '
                                            'and reference, warped (2 loads of the filter table per row) or '
                                            'translated; the difference weighted mask through the group memory'),
}


def fail(message):
    print('inter_static.py: ' + message, file=sys.stderr)
    sys.exit(2)


def components(mask):
    return len(set(mask))


def listing_counts(lines):
    """Static counts of one listing (its lines, comments included)."""
    code = [l.strip() for l in lines if not l.lstrip().startswith('//')]
    counts = {'loads': {}, 'stores': {}, 'loops': 0, 'syncs': 0, 'shared': 0, 'group': '', 'temps': '', 'slots': '',
              'loop_loads': 0}
    depth = 0
    for line in code:
        if line.startswith('loop'):
            depth += 1
            counts['loops'] += 1
        elif line.startswith('endloop'):
            depth -= 1
        elif line.startswith('sync'):
            counts['syncs'] += 1
        match = re.match(r'ld_raw\S*\s+r\d+\.([xyzw]+),\s*[^,]+,\s*([tu]\d+)', line)
        if match:
            entry = counts['loads'].setdefault(match.group(2), [0, 0])
            entry[0] += 1
            entry[1] += 4 * components(match.group(1))
            if depth and match.group(2) == 'u0':
                counts['loop_loads'] += 1
        match = re.match(r'store_raw\s+(u\d+)\.([xyzw]+)', line)
        if match:
            entry = counts['stores'].setdefault(match.group(1), [0, 0])
            entry[0] += 1
            entry[1] += 4 * components(match.group(2))
        match = re.match(r'dcl_tgsm_structured\s+g\d+,\s*(\d+),\s*(\d+)', line)
        if match:
            counts['shared'] += int(match.group(1)) * int(match.group(2))
        match = re.match(r'dcl_tgsm_raw\s+g\d+,\s*(\d+)', line)
        if match:
            counts['shared'] += int(match.group(1))
        match = re.match(r'dcl_thread_group\s+(.*)', line)
        if match:
            counts['group'] = re.sub(r'\s+', '', match.group(1))
        match = re.match(r'dcl_temps\s+(\d+)', line)
        if match:
            counts['temps'] = match.group(1)
    for line in lines:
        match = re.search(r'Approximately (\d+) instruction slots used', line)
        if match:
            counts['slots'] = match.group(1)
    return counts


def model_of(name):
    hbd = name.endswith('_hbd')
    base = name[:-4] if hbd else name
    return MODEL.get(base), 1 if hbd else 0


def number(value):
    return ('%.2f' % value).rstrip('0').rstrip('.')


def row_of(name, counts):
    """The table row of one shader and whether its listing agrees with the model (None: not comparable)."""
    model, hbd = model_of(name)
    frame = counts['loads'].get('u0', [0, 0])
    other = ', '.join('%s %d/%d' % (BUFFERS.get(key, key), value[0], value[1])
                      for key, value in sorted(counts['loads'].items()) if key != 'u0')
    stores = counts['stores'].get('u0', [0, 0])
    cells = [name, counts['group'], counts['slots'], counts['temps'], str(counts['shared']), str(counts['syncs']),
             str(counts['loops']), '%d/%d' % (frame[0], frame[1]), other or '-', '%d/%d' % (stores[0], stores[1])]
    agree = None
    if model:
        samples = model['out'][0] * model['out'][1]
        ref_bytes = model['refs'] * model['rows'] * model['row'][hbd]
        per_sample = ref_bytes / samples
        sample_bytes = 2 if hbd else 1
        cells += ['%dx%d' % model['out'], '%d x %s x %d' % (model['refs'], number(model['rows']), model['row'][hbd]),
                  number(ref_bytes), number(per_sample), number(per_sample / (model['refs'] * sample_bytes)),
                  model['section']]
        if not counts['loop_loads']:
            agree = frame[1] == ref_bytes + model['dst'][hbd]
            cells.append('yes' if agree else 'NO: listing %d, model %s + %d' % (frame[1], number(ref_bytes),
                                                                                model['dst'][hbd]))
        else:
            cells.append('loop')
    else:
        cells += ['?'] * 7
    return cells, agree


HEAD = ['shader', 'group', 'slots', 'temps', 'group memory, bytes', 'syncs', 'loops', 'frame loads, n/bytes',
        'other loads, n/bytes', 'frame stores, n/bytes', 'thread writes', 'refs x rows x bytes',
        'reference bytes per thread', 'per sample written', 'times the sample', 'section', 'listing = model']


def report(listings):
    paths = sorted(glob.glob(os.path.join(listings, 'inter_*.asm')))
    paths = [p for p in paths if 'ext_borders' not in os.path.basename(p)]
    if not paths:
        fail('no inter_*.asm in ' + listings)
    rows = []
    disagree = []
    for path in paths:
        name = os.path.basename(path)[:-4]
        with open(path, encoding='utf-8', errors='replace') as f:
            cells, agree = row_of(name, listing_counts(f.readlines()))
        rows.append(cells)
        if agree is False:
            disagree.append(name)
    out = ['# Inter prediction shaders, static view', '',
           'From the FXC listings (cs_5_0) and the table MODEL of testing/shader_cache/inter_static.py, which is '
           'read from the HLSL source.', '',
           'Listing columns (static: instructions in the listing): slots, temps (dcl_temps: registers of four '
           '32-bit components), group memory, group syncs, loops, loads of the frame buffer pool (the references '
           'and the destination are both in it) as instructions/bytes, loads of the other buffers, stores to the '
           'frame.', '',
           'Model columns: the samples one thread writes; references x rows of a reference x bytes of the loads '
           'of one row; the product, reference bytes a thread loads; that per sample written; and that as a '
           'multiple of the size of one sample of one reference (1 would be every reference sample loaded '
           'once). "listing = model": the frame loads of the listing are the model\'s reference bytes plus its '
           'loads of the destination; "loop" where the rows are loaded in a loop whose count depends on the '
           'scale (the model has the count for a reference of the frame\'s size).', '',
           '| ' + ' | '.join(HEAD) + ' |', '|' + '---|' * len(HEAD)]
    out += ['| ' + ' | '.join(cells) + ' |' for cells in rows]
    out += ['', 'No model columns for `inter_fast` and `inter_compound_fast` (test flag -interfast): a listing has '
            'the loads of every motion vector class, and a thread runs one of them. Per thread and reference, rows '
            'x bytes of a row, 8-bit / 10-bit: copy 4 x 8 / 4 x 16, h 4 x 16 / 4 x 24, v 11 x 8 / 11 x 16 for '
            '`inter_fast` (a thread writes 4x4); copy 2 x 8 / 2 x 16, h 2 x 16 / 2 x 24, v 9 x 8 / 9 x 16, 2d 9 x 16 '
            '/ 9 x 24 for `inter_compound_fast` (4x2, two references, each of its own class).']
    out += ['', '## How a thread maps to samples and reads its references', '']
    for name in sorted(MODEL):
        model = MODEL[name]
        out.append('- `%s` (%s): a thread writes %dx%d samples; %s.' % (
            name, model['section'], model['out'][0], model['out'][1], model['reuse']))
    return out, disagree, len(rows)


def self_test():
    listing = '''// comment ld_raw r0.x, r0.x, u0.x
dcl_temps 7
dcl_tgsm_structured g0, 4, 1280
dcl_thread_group 64, 1, 1
ld_raw_indexable(raw_buffer)(mixed,mixed,mixed,mixed) r1.xyz, r0.z, t0.xyzx
loop
  ld_raw_indexable(raw_buffer)(mixed,mixed,mixed,mixed) r5.xyzw, r4.x, u0.xyzw
  ld_raw_indexable(raw_buffer)(mixed,mixed,mixed,mixed) r4.x, r4.x, u0.xxxx
endloop
sync_g_t
store_raw u0.xy, r0.y, r0.xzxx
ret
// Approximately 208 instruction slots used
'''.splitlines()
    counts = listing_counts(listing)
    assert counts['loads'] == {'t0': [1, 12], 'u0': [2, 20]}, counts
    assert counts['stores'] == {'u0': [1, 8]}, counts
    assert (counts['loops'], counts['syncs'], counts['shared'], counts['loop_loads']) == (1, 1, 5120, 2), counts
    assert (counts['group'], counts['temps'], counts['slots']) == ('64,1,1', '7', '208'), counts
    cells, agree = row_of('inter_scale_hbd', counts)
    assert agree is None and cells[-1] == 'loop' and cells[11] == '1 x 11 x 20' and cells[13] == '55', cells
    # A shader without a loop: 11 rows of 24 bytes.
    flat = ['ld_raw_indexable(raw_buffer)(mixed,mixed,mixed,mixed) r5.xyzw, r4.x, u0.xyzw',
            'ld_raw_indexable(raw_buffer)(mixed,mixed,mixed,mixed) r4.xy, r4.x, u0.xyxx'] * 11
    cells, agree = row_of('inter_main_hbd', listing_counts(flat))
    assert agree is True and cells[12] == '264' and cells[13] == '16.5' and cells[14] == '8.25', cells
    cells, agree = row_of('inter_main_hbd', listing_counts(flat[:-1]))
    assert agree is False and cells[-1].startswith('NO'), cells
    assert model_of('inter_compound_hbd')[0]['refs'] == 2 and model_of('inter_compound_hbd')[1] == 1
    assert row_of('inter_new_shader', listing_counts(flat))[0][-1] == '?'
    print('inter_static.py: self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--listings', help='directory with the *.asm listings of a build')
    parser.add_argument('--out', help='write the markdown here (default: stdout)')
    parser.add_argument('--strict', action='store_true', help='a listing that differs from the model is an error')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if not args.listings:
        fail('--listings is needed')
    out, disagree, count = report(args.listings)
    text = '\n'.join(out) + '\n'
    if args.out:
        with open(args.out, 'w', encoding='utf-8') as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    print('inter_static.py: %d inter shaders; listing and model differ for %d%s' % (
        count, len(disagree), ': ' + ', '.join(disagree) if disagree else ''), file=sys.stderr)
    if disagree and args.strict:
        sys.exit(1)


if __name__ == '__main__':
    main()
