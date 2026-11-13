#!/usr/bin/env python3
"""The shaders of libav1 for the CI builds: compiled once, by content, and surveyed.

FXC takes about 40 s for the shaders of libav1/dx/shaders when it runs on four at a time. A
workflow run needs the bytecode of every shader several times (the libraries of the test player
packages, rav1g-dec, its build on Linux) and the listing of every shader once (the survey). This script gives each of them the output of one
FXC call per shader, kept in a cache directory by the content that decides it:

    key = sha256(the FXC binaries, the arguments (profile, entry point, variable name, /Od,
                 defines), the source file and every file it includes, directly or not)

A cache entry is the header with the bytecode (/Fh) and the listing (/Fc) of that one call, and
what FXC printed. A shader whose key is in the cache is not compiled; the others are, several
at a time. The workflow keeps the directory with actions/cache, keyed by all keys together.

    shaders.py key     the digest of all keys, for the actions/cache key
    shaders.py build   headers and listings of one variant from the cache, compiling what is missing
    shaders.py prune   drop the cache entries the given builds did not use
    shaders.py survey  the shader survey and the CDEF and intra listings, from the listings of a build
    shaders.py verify  the bytecode of the headers is in a built program
    shaders.py same    two builds have the same shaders (the equality with the C++ decoder of branch cpp
                       holds only as long as they do: docs/rust-port.md, "Equality reference")

The Rust library is built from the headers: RAV1G_SHADER_DIR=<root> (crates/rav1g-sys/build.rs).
"""
import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, '..', '..'))
SHADERS = os.path.join(REPO, 'libav1', 'dx', 'shaders')
FORMAT = 1  # of the key material and of a cache entry; a change makes every key new

# The variants of a build: the preprocessor definitions FXC gets on top of the settings of every shader.
VARIANTS = {
    'normal': [],
    # The library built with the uavsync feature of the crates takes these headers (RAV1G_SHADER_DIR).
    'uavsync': ['GroupMemoryBarrierWithGroupSync=AllMemoryBarrierWithGroupSync'],
}

# The shaders whose listings and counts go into the artifact next to the survey, and the ones
# built without optimization (/Od): the upstream intra shaders, which the -intraopt comparison holds
# against their optimized builds intra_main_opt and intra_main_hbd_opt.
SETS = [
    ('cdef-shaders', 'CDEF shader listings', ['cdef_filter', 'cdef_filter_v2']),
    ('intra-shaders', 'Intra shader listings',
     ['intra_main', 'intra_main_opt', 'intra_main_hbd', 'intra_main_hbd_opt', 'intra_filter', 'intra_filter_hbd',
      'intra_v2', 'intra_v2_hbd']),
]
UNOPTIMIZED = ['intra_main', 'intra_main_hbd']


def fail(message):
    raise SystemExit('shaders.py: ' + message)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def project_shaders(shaders_dir=SHADERS):
    """The shaders of the decoder: [(name, built with /Od)], by name. Every .hlsl file of libav1/dx/shaders
    is one shader (what a shader includes is a .h file); UNOPTIMIZED lists the ones built with /Od. That is
    what the library project of the C++ decoder (branch cpp, libav1/build/libav1.vcxproj) says with its
    FxCompile items, so the keys, and with them the bytecode, are the same on both branches while the
    sources are."""
    names = sorted(name[:-5] for name in os.listdir(shaders_dir) if name.endswith('.hlsl'))
    if len(names) < 80:
        fail('only %d shaders found in %s' % (len(names), shaders_dir))
    for name in UNOPTIMIZED + ['intra_main_opt', 'intra_main_hbd_opt']:
        if name not in names:
            fail('%s is not a shader of %s' % (name, shaders_dir))
    return [(name, name in UNOPTIMIZED) for name in names]


INCLUDE = re.compile(rb'^[ \t]*#[ \t]*include[ \t]*"([^"]+)"', re.M)


def source_files(name, shaders_dir=SHADERS):
    """The source of a shader and every file it includes, directly or not: [(file name, sha256)]."""
    seen = {}
    todo = [name + '.hlsl']
    while todo:
        rel = todo.pop()
        if rel in seen:
            continue
        path = os.path.join(shaders_dir, rel)
        try:
            with open(path, 'rb') as f:
                data = f.read()
        except OSError:
            fail('%s: cannot read %s' % (name, rel))
        seen[rel] = hashlib.sha256(data).hexdigest()
        for include in INCLUDE.findall(data):
            # Relative to the including file, as FXC resolves it.
            todo.append(os.path.normpath(os.path.join(os.path.dirname(rel), include.decode('utf-8'))).replace('\\', '/'))
    return sorted(seen.items())


def fxc_identity(fxc):
    """What decides the compiler's output: fxc.exe and the d3dcompiler it loads from its directory."""
    if not os.path.isfile(fxc):
        fail('fxc not found: %s' % fxc)
    parts = [('fxc', sha256_file(fxc))]
    directory = os.path.dirname(os.path.abspath(fxc))
    for other in sorted(os.listdir(directory)):
        if re.match(r'd3dcompiler.*\.dll$', other, re.I):
            parts.append((other.lower(), sha256_file(os.path.join(directory, other))))
    return hashlib.sha256(json.dumps(parts).encode()).hexdigest()


def fxc_arguments(name, od, variant):
    """The arguments that decide the output; the output files and the source path come on top."""
    args = ['/nologo', '/T', 'cs_5_0', '/E', 'main', '/Vn', name]
    if od:
        args.append('/Od')
    for define in VARIANTS[variant]:
        args += ['/D', define]
    return args


def shader_key(name, od, variant, identity, shaders_dir=SHADERS):
    material = {'format': FORMAT, 'fxc': identity, 'arguments': fxc_arguments(name, od, variant),
                'files': source_files(name, shaders_dir)}
    return hashlib.sha256(json.dumps(material, sort_keys=True).encode()).hexdigest()


def entry_files(cache, key, name):
    directory = os.path.join(cache, key)
    return directory, os.path.join(directory, name + '.h'), os.path.join(directory, name + '.asm'), os.path.join(
        directory, 'meta.json')


def entry_complete(cache, key, name):
    _, header, listing, meta = entry_files(cache, key, name)
    return all(os.path.isfile(p) and os.path.getsize(p) > 0 for p in (header, listing, meta))


def messages(lines):
    """What FXC printed, without the lines that only say where it saved its output."""
    return [line.rstrip() for line in lines
            if line.strip() and not re.search(r'compilation \w+ save succeeded', line)]


def compile_shader(fxc, cache, name, od, variant, key, shaders_dir):
    """One FXC call: the header and the listing of the shader, into a new cache entry."""
    os.makedirs(cache, exist_ok=True)
    work = tempfile.mkdtemp(prefix='new-', dir=cache)
    header = os.path.join(work, name + '.h')
    listing = os.path.join(work, name + '.asm')
    command = [fxc] + fxc_arguments(name, od, variant) + ['/Fc', listing, '/Fh', header,
                                                           os.path.join(shaders_dir, name + '.hlsl')]
    start = time.monotonic()
    done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    seconds = round(time.monotonic() - start, 1)
    output = messages(done.stdout.decode('utf-8', 'replace').splitlines())
    ok = done.returncode == 0 and os.path.isfile(header) and os.path.isfile(listing)
    if not ok:
        shutil.rmtree(work, ignore_errors=True)
        return {'ok': False, 'seconds': seconds, 'output': output, 'exit': done.returncode}
    with open(os.path.join(work, 'meta.json'), 'w', encoding='utf-8') as f:
        json.dump({'format': FORMAT, 'name': name, 'variant': variant, 'od': od, 'seconds': seconds, 'output': output},
                  f, indent=1)
    directory = entry_files(cache, key, name)[0]
    shutil.rmtree(directory, ignore_errors=True)  # an incomplete entry of the same key
    try:
        os.replace(work, directory)
    except OSError:
        # Another process made the same entry in the meantime: the two are the same.
        shutil.rmtree(work, ignore_errors=True)
        if not entry_complete(cache, key, name):
            raise
    return {'ok': True, 'seconds': seconds, 'output': output, 'exit': 0}


def command_key(args):
    identity = fxc_identity(args.fxc)
    digest = hashlib.sha256()
    count = 0
    for variant in args.variants.split(','):
        if variant not in VARIANTS:
            fail('unknown variant %s' % variant)
        for name, od in project_shaders():
            digest.update(shader_key(name, od, variant, identity).encode())
            count += 1
    value = digest.hexdigest()[:32]
    print('shaders.py: %d shader keys (%s), digest %s' % (count, args.variants, value))
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a', encoding='utf-8') as f:
            f.write('digest=%s\n' % value)
    return 0


def command_build(args):
    if args.variant not in VARIANTS:
        fail('unknown variant %s' % args.variant)
    start = time.monotonic()
    identity = fxc_identity(args.fxc)
    shaders = project_shaders()
    keys = {name: shader_key(name, od, args.variant, identity) for name, od in shaders}
    missing = [(name, od) for name, od in shaders if not entry_complete(args.cache, keys[name], name)]
    jobs = max(1, args.jobs or (os.cpu_count() or 2))
    results = {}
    if missing:
        print('shaders.py: %s: %d of %d shaders are not in the cache; FXC on up to %d at a time' %
              (args.variant, len(missing), len(shaders), jobs), flush=True)
        with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = {pool.submit(compile_shader, args.fxc, args.cache, name, od, args.variant, keys[name], SHADERS):
                       name for name, od in missing}
            for future in concurrent.futures.as_completed(futures):
                results[futures[future]] = future.result()
    failed = sorted(name for name, result in results.items() if not result['ok'])
    for name in failed:
        print('FXC failed for %s.hlsl (exit code %s):' % (name, results[name]['exit']))
        for line in results[name]['output']:
            print('  ' + line)
    if failed:
        fail('FXC failed for ' + ', '.join(name + '.hlsl' for name in failed))

    headers = os.path.join(args.root, 'dx', 'shaders', 'h')
    os.makedirs(headers, exist_ok=True)
    if args.listings:
        os.makedirs(args.listings, exist_ok=True)
    report = {'format': FORMAT, 'variant': args.variant, 'fxc': args.fxc, 'fxc_identity': identity, 'jobs': jobs,
              'compiled': len(missing), 'from_cache': len(shaders) - len(missing), 'shaders': []}
    for name, od in shaders:
        _, header, listing, meta_path = entry_files(args.cache, keys[name], name)
        with open(meta_path, encoding='utf-8') as f:
            meta = json.load(f)
        shutil.copyfile(header, os.path.join(headers, name + '.h'))
        if args.listings:
            shutil.copyfile(listing, os.path.join(args.listings, name + '.asm'))
        report['shaders'].append({'name': name, 'od': od, 'key': keys[name], 'compiled': name in results,
                                  'seconds': meta['seconds'], 'output': messages(meta['output']),
                                  'header_sha256': sha256_file(header)})
        # What FXC said about the shader (warnings), also when its output came from the cache.
        for line in messages(meta['output']):
            print('%s.hlsl%s: %s' % (name, '' if name in results else ' (cached)', line))
    report['wall_seconds'] = round(time.monotonic() - start, 1)
    if args.report:
        os.makedirs(os.path.dirname(os.path.abspath(args.report)), exist_ok=True)
        with open(args.report, 'w', encoding='utf-8') as f:
            json.dump(report, f, indent=1)
    cpu = sum(results[name]['seconds'] for name in results)
    print('shaders.py: %s: %d shaders, %d compiled now (%.1f s of FXC in %.1f s), %d from the cache; headers in %s' %
          (args.variant, len(shaders), len(missing), cpu, report['wall_seconds'], report['from_cache'], headers))
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('- shaders, %s: %d compiled in this run (%.1f s of FXC in %.1f s, up to %d at a time), %d from '
                    'the cache\n' % (args.variant, len(missing), cpu, report['wall_seconds'], jobs, report['from_cache']))
    return 0


def command_prune(args):
    used = set()
    for path in args.reports:
        with open(path, encoding='utf-8') as f:
            used.update(shader['key'] for shader in json.load(f)['shaders'])
    removed = 0
    kept = 0
    for entry in sorted(os.listdir(args.cache)) if os.path.isdir(args.cache) else []:
        path = os.path.join(args.cache, entry)
        if entry in used:
            kept += 1
        elif os.path.isdir(path):
            shutil.rmtree(path, ignore_errors=True)
            removed += 1
    size = sum(os.path.getsize(os.path.join(root, name)) for root, _, names in os.walk(args.cache) for name in names)
    print('shaders.py: cache %s: %d entries kept (%.1f MiB), %d of other sources or compilers removed' %
          (args.cache, kept, size / (1024.0 * 1024.0), removed))
    return 0


# ---------------------------------------------------------------------------------------------
# The survey: counts from the listings. Static counts: instructions in the listing, not executed.
# ---------------------------------------------------------------------------------------------
def count(code, pattern):
    return sum(1 for line in code if re.search(pattern, line, re.I))


def listing_counts(path):
    with open(path, encoding='utf-8', errors='replace') as f:
        text = f.read().splitlines()
    code = [line for line in text if not re.match(r'^\s*(//|$)', line)]
    indexable = [line.strip() for line in code if re.match(r'^\s*dcl_indexableTemp', line, re.I)]
    indexable_values = 0
    for declaration in indexable:
        match = re.search(r'\[(\d+)\],\s*(\d+)', declaration)
        if match:
            indexable_values += int(match.group(1)) * int(match.group(2))
    shared = [line.strip() for line in code if re.match(r'^\s*dcl_tgsm_', line, re.I)]
    shared_bytes = 0
    for declaration in shared:
        numbers = [int(n) for n in re.findall(r'\d+', re.sub(r'^\S+\s+g\d+', '', declaration))]
        shared_bytes += numbers[0] * numbers[1] if 'structured' in declaration else numbers[0]
    # Entries of the immediate constant buffer: one per line of its declaration.
    icb_entries = 0
    for i, line in enumerate(code):
        if 'dcl_immediateConstantBuffer' not in line:
            continue
        for later in code[i:]:
            icb_entries += 1
            if re.search(r'\}\s*\}', later):
                break
        break
    group = re.sub(r'^dcl_thread_group\s*', '', ''.join(l for l in code if re.match(r'^\s*dcl_thread_group', l)).strip())
    threads = 1
    for n in group.split(','):
        if n.strip():
            threads *= int(n.strip())
    slots = None
    for line in text:
        match = re.search(r'Approximately (\d+) instruction slots used', line)
        if match:
            slots = match.group(1)
            break
    if slots is None:
        fail('%s: no instruction slot count in the listing' % path)
    temps = re.sub(r'^dcl_temps\s*', '', ''.join(l for l in code if re.match(r'^\s*dcl_temps', l)).strip())
    return {
        'code': code, 'group': group, 'threads': threads, 'slots': slots, 'temps': temps,
        'indexable': indexable, 'indexable_values': indexable_values,
        'indexable_instructions': count(code, r'\bx\d+\['),
        'icb_entries': icb_entries, 'icb_reads': count(code, r'\bicb\['),
        'shared': shared, 'shared_bytes': shared_bytes,
        'sync': count(code, r'^\s*sync'), 'sync_threads': count(code, r'^\s*sync\S*_t\b'),
        'shared_loads': count(code, r'^\s*ld_(raw|structured)\S*\s.*\bg\d'),
        'shared_stores': count(code, r'^\s*store_(raw|structured)\S*\s+g\d'),
        'buffer_loads': count(code, r'^\s*ld_(raw|structured)\S*\s.*\b[tu]\d'),
        'buffer_stores': count(code, r'^\s*store_(raw|structured)\S*\s+u\d'),
        'loops': count(code, r'^\s*loop\b'),
    }


def runtime_width_bit_fields(name, code):
    """Bit field instructions whose width is known only at run time, except ubfe at offset 0. FXC makes
    these out of x & ((1 << n) - 1) and of (x >> n) << n with a run-time n, and where n can be 0 the
    result depends on the GPU (docs/gpu-av1-decoder.md, "Bit fields of width 0"); the shaders take such
    masks from a table. ubfe at offset 0 is what the block index of every inter shader compiles to and
    is known to be right on the console."""
    found = []
    for line in code:
        match = re.match(r'^\s*(bfi|ubfe|ibfe)\s+\S+,\s*(l\([^)]*\)|[^,]+),\s*(l\([^)]*\)|[^,]+),', line, re.I)
        if not match:
            continue
        op, width, offset = match.group(1).lower(), match.group(2).strip(), match.group(3).strip()
        if width.lower().startswith('l('):
            continue
        if op == 'ubfe' and re.match(r'^l\(0(, 0)*\)$', offset, re.I):
            continue
        found.append('%s.hlsl: %s' % (name, line.strip()))
    return found


def command_survey(args):
    with open(args.report, encoding='utf-8') as f:
        report = json.load(f)
    shaders = report['shaders']
    rows = {}
    runtime_width = []
    for shader in shaders:
        name = shader['name']
        c = listing_counts(os.path.join(args.listings, name + '.asm'))
        # The two -intraopt builds are test builds of a source that is otherwise built with /Od and are
        # not held to the bit field rule.
        if not name.endswith('_opt'):
            runtime_width += runtime_width_bit_fields(name, c['code'])
        seconds = '%.1f' % shader['seconds']
        od = shader['od']
        block = ['', '== %s.hlsl%s ==' % (name, ' (/Od: this shader is built without optimization)' if od else ''),
                 'thread group:                           dcl_thread_group %s' % c['group'],
                 'instruction slots:                      %s' % c['slots'],
                 'temp registers:                         dcl_temps %s' % c['temps'],
                 'indexable temps:                        %d' % len(c['indexable'])]
        block += ['    ' + declaration for declaration in c['indexable']]
        block += ['instructions using an indexable temp:   %d' % c['indexable_instructions'],
                  'immediate constant buffer:              %s' %
                  ('yes, %d entries' % c['icb_entries'] if c['icb_entries'] else 'no'),
                  'instructions reading it (icb):          %d' % c['icb_reads'],
                  'groupshared declarations:               %d, %d bytes' % (len(c['shared']), c['shared_bytes'])]
        block += ['    ' + declaration for declaration in c['shared']]
        block += ['group barriers (sync):                  %d, %d of them wait for the whole thread group' %
                  (c['sync'], c['sync_threads']),
                  'groupshared loads / stores:             %d / %d' % (c['shared_loads'], c['shared_stores']),
                  'buffer loads / stores:                  %d / %d' % (c['buffer_loads'], c['buffer_stores']),
                  'loops:                                  %d' % c['loops'],
                  'FXC compile time:                       %s s%s' %
                  (seconds, '' if shader['compiled'] else ' (in the run that compiled it; from the cache here)'),
                  'bytecode header:                        sha256 %s, written by the FXC call that wrote this listing' %
                  shader['header_sha256']]
        rows[name] = {
            'block': block,
            'line': '| %s | %s | %s | %d | %s | %s | %d / %d | %d | %d | %d | %d | %d / %d | %d | %s |' % (
                name, '/Od' if od else '', c['group'], c['threads'], c['slots'], c['temps'], len(c['indexable']),
                c['indexable_values'], c['indexable_instructions'], c['icb_entries'], c['icb_reads'],
                c['shared_bytes'], c['sync'], c['sync_threads'], c['loops'], seconds),
            'csv': '%s,%d,%s,%d,%s,%s,%d,%d,%d,%d,%d,%d,%d,%d,%d,%s' % (
                name, 1 if od else 0, re.sub(r'[,\s]+', 'x', c['group']), c['threads'], c['slots'], c['temps'],
                len(c['indexable']), c['indexable_values'], c['indexable_instructions'], c['icb_entries'],
                c['icb_reads'], c['shared_bytes'], c['sync'], c['sync_threads'], c['loops'], seconds),
        }
    if runtime_width:
        for line in runtime_width:
            print(line)
        fail('bit field instructions with a run-time width in %d places: take the mask from a table' % len(runtime_width))

    intro = ['FXC of Windows SDK %s: /T cs_5_0 /E main with the settings every build has (default optimization; '
             '/Od where marked).' % args.sdk,
             'Counts are static: instructions in the listing, not instructions executed.',
             'One FXC call per shader writes its listing and the bytecode header the programs are built from; its '
             'output is kept by the content of the source, of the files it includes and of the compiler.',
             '%d of the %d shaders were compiled in this run, on up to %d at a time, so the compile time of a shader '
             'includes contention with the others; %d came from the cache, with the compile time of the run that '
             'compiled them.' % (report['compiled'], len(shaders), report['jobs'], report['from_cache'])]
    summary = []
    names = [shader['name'] for shader in shaders]
    for directory, title, members in SETS:
        target = os.path.join(args.out, directory)
        os.makedirs(target, exist_ok=True)
        stats = list(intro)
        for name in members:
            if name not in rows:
                fail('%s is not a shader of the build' % name)
            shutil.copyfile(os.path.join(args.listings, name + '.asm'), os.path.join(target, name + '.asm'))
            stats += rows[name]['block']
        with open(os.path.join(target, 'stats.txt'), 'w', encoding='utf-8') as f:
            f.write('\n'.join(stats) + '\n')
        summary += ['## ' + title, '```'] + stats + ['```']
    survey = ['# Shader survey', ''] + intro + [
        '',
        'Indexable temps: arrays the compiler keeps in addressable scratch memory, as count / total 32-bit components.',
        'Barriers: all sync instructions / those that wait for the whole thread group.', '',
        '| shader | opt | thread group | threads | instruction slots | temps | indexable temps | instructions on them '
        '| ICB entries | ICB reads | groupshared bytes | barriers | loops | FXC s |',
        '| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |']
    survey += [rows[name]['line'] for name in names]
    target = os.path.join(args.out, 'shader-survey')
    os.makedirs(target, exist_ok=True)
    with open(os.path.join(target, 'survey.md'), 'w', encoding='utf-8') as f:
        f.write('\n'.join(survey) + '\n')
    with open(os.path.join(target, 'survey.csv'), 'w', encoding='utf-8') as f:
        f.write('\n'.join(
            ['shader,od,thread_group,threads,instruction_slots,temps,indexable_temps,indexable_components,'
             'indexable_instructions,icb_entries,icb_reads,groupshared_bytes,barriers,barriers_thread_sync,loops,'
             'fxc_seconds'] + [rows[name]['csv'] for name in names]) + '\n')
    summary += ['## Shader survey', ''] + survey[2:]
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n'.join(summary) + '\n')
    total = sum(int(rows[name]['csv'].split(',')[4]) for name in names)
    print('shaders.py: survey of %d shaders in %s, %d instruction slots in all' % (len(names), target, total))
    return 0


# ---------------------------------------------------------------------------------------------
# The bytecode of the headers is in the program that was built from them.
# ---------------------------------------------------------------------------------------------
def header_bytecode(path):
    with open(path, encoding='utf-8', errors='replace') as f:
        text = f.read()
    start = text.find('const BYTE')
    if start < 0:
        fail('no bytecode array in %s' % path)
    body = text[text.index('{', start) + 1:text.index('}', start)]
    data = bytes(int(n) for n in re.findall(r'\d+', body))
    if data[:4] != b'DXBC':
        fail('%s: the array is not a DXBC container' % path)
    return data


def program_bytes(path):
    """A program file, or the program inside an app package (a zip): [(name, bytes)]."""
    if zipfile.is_zipfile(path):
        with zipfile.ZipFile(path) as package:
            programs = [(name, package.read(name)) for name in package.namelist() if name.lower().endswith('.exe')]
        if not programs:
            fail('no program in the package %s' % path)
        return programs
    with open(path, 'rb') as f:
        return [(os.path.basename(path), f.read())]


def command_verify(args):
    with open(args.report, encoding='utf-8') as f:
        report = json.load(f)
    headers = os.path.join(args.root, 'dx', 'shaders', 'h')
    lines = []
    ok = True
    for binary in args.binary:
        for program, data in program_bytes(binary):
            found = []
            absent = []
            for shader in report['shaders']:
                header = os.path.join(headers, shader['name'] + '.h')
                if sha256_file(header) != shader['header_sha256']:
                    fail('%s is not the header the build of the report wrote' % header)
                (found if header_bytecode(header) in data else absent).append(shader['name'])
            line = ('%s (%s): the bytecode of %d of the %d shader headers of the %s build is in the program, byte for '
                    'byte' % (program, os.path.basename(binary), len(found), len(report['shaders']), report['variant']))
            if absent:
                line += '; not in it, as no pipeline state uses them: ' + ', '.join(absent)
            lines.append(line)
            print('shaders.py: ' + line)
            # The linker drops an array nothing refers to. All but a few shaders have a pipeline state.
            if len(found) < args.min_found:
                ok = False
            for directory, _, members in SETS:
                missing = [name for name in members if name not in found]
                if missing:
                    print('shaders.py: listed in %s and not in the program: %s' % (directory, ', '.join(missing)))
                    ok = False
    if args.note:
        for path in args.note:
            with open(path, 'a', encoding='utf-8') as f:
                f.write('\n'.join([''] + lines) + '\n')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n'.join([''] + ['- ' + line for line in lines]) + '\n')
    if not ok:
        fail('the program was not built from the shader headers of this run')
    return 0


def command_same(args):
    """Two builds have the same shaders: every shader of one is in the other, with the same key (the
    compiler, its arguments and the sources) or the same header."""
    builds = []
    for path in (args.reference, args.candidate):
        with open(path, encoding='utf-8') as f:
            report = json.load(f)
        builds.append({shader['name']: shader for shader in report['shaders']})
    reference, candidate = builds
    different = []
    for name in sorted(set(reference) | set(candidate)):
        a, b = reference.get(name), candidate.get(name)
        if a is None or b is None:
            different.append('%s: only in %s' % (name, args.candidate_name if a is None else args.reference_name))
        elif a['key'] != b['key'] and a['header_sha256'] != b['header_sha256']:
            different.append('%s: another source, compiler or argument (key %s.. in %s, %s.. in %s)' % (
                name, a['key'][:12], args.reference_name, b['key'][:12], args.candidate_name))
    for line in different:
        print('DIFFERENT ' + line)
    summary = ('%d of %d shaders of %s and %s differ' % (len(different), len(set(reference) | set(candidate)),
                                                       args.reference_name, args.candidate_name)
               if different else
               'the %d shaders of %s and %s are the same' % (len(candidate), args.reference_name, args.candidate_name))
    print('shaders.py: ' + summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('- %s\n' % summary)
    return 1 if different else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)

    key = commands.add_parser('key', help='the digest of the keys of all shaders')
    key.add_argument('--fxc', required=True)
    key.add_argument('--variants', default='normal,uavsync')
    key.set_defaults(run=command_key)

    build = commands.add_parser('build', help='headers and listings of one variant')
    build.add_argument('--fxc', required=True)
    build.add_argument('--cache', required=True, help='the cache directory')
    build.add_argument('--variant', default='normal')
    build.add_argument('--root', required=True, help='the headers go to <root>/dx/shaders/h/<name>.h')
    build.add_argument('--listings', help='directory for the listings, <name>.asm')
    build.add_argument('--report', help='JSON file: the key, the compile time and the FXC output of every shader')
    build.add_argument('--jobs', type=int, default=0, help='FXC processes at a time (default: one per processor)')
    build.set_defaults(run=command_build)

    prune = commands.add_parser('prune', help='drop the cache entries the builds of these reports did not use')
    prune.add_argument('--cache', required=True)
    prune.add_argument('reports', nargs='+')
    prune.set_defaults(run=command_prune)

    survey = commands.add_parser('survey', help='survey.md, survey.csv and the CDEF and intra listings')
    survey.add_argument('--report', required=True)
    survey.add_argument('--listings', required=True)
    survey.add_argument('--out', required=True)
    survey.add_argument('--sdk', default='?')
    survey.set_defaults(run=command_survey)

    verify = commands.add_parser('verify', help='the bytecode of the headers is in a built program')
    verify.add_argument('--report', required=True)
    verify.add_argument('--root', required=True)
    verify.add_argument('--min-found', type=int, default=80)
    verify.add_argument('--note', action='append', help='file to append the result to')
    verify.add_argument('binary', nargs='+', help='a program, or an app package with the program in it')
    verify.set_defaults(run=command_verify)

    same = commands.add_parser('same', help='two builds (their reports) have the same shaders')
    same.add_argument('--reference', required=True, help='report.json of one build')
    same.add_argument('--candidate', required=True, help='report.json of the other')
    same.add_argument('--reference-name', default='the reference')
    same.add_argument('--candidate-name', default='this build')
    same.set_defaults(run=command_same)

    args = parser.parse_args()
    # What FXC printed goes to the log as it is, whatever the console's code page can show.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, 'reconfigure'):
            stream.reconfigure(errors='replace')
    return args.run(args)


if __name__ == '__main__':
    sys.exit(main())
