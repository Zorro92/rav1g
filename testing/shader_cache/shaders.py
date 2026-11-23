#!/usr/bin/env python3
"""Build the decoder's DXC cs_6_0 shaders from a content-addressed cache.

A key covers the compiler binaries, arguments, source and transitive includes. A cache entry has
one shader's bytecode (the file DXC wrote, as it wrote it), listing and compiler messages.

    shaders.py key     the digest of all keys, for the actions/cache key
    shaders.py build   bytecode and listings of one variant from the cache, compiling what is missing
    shaders.py prune   drop the cache entries the given builds did not use
    shaders.py verify  the bytecode of a build is in a built program
    shaders.py same    compare two builds' shader bytecode hashes

The Rust library takes dx/shaders/dxil/<name>.dxil and shader_set.txt from RAV1G_SHADER_DIR=<root>
(crates/rav1g-sys/build.rs embeds the files as they are). The uavsync variant builds with all-memory
group syncs. The shader-pick variant adds dx/shaders/dxil_od and shader_sets.txt. The shader-pick Cargo feature embeds the extra DXC bytecode for -shaderpick
(docs/test-player.md, "Shader pick").
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
FORMAT = 2  # of the key material and of a cache entry; a change makes every key new
BYTECODE = 'dxil'  # <root>/dx/shaders/dxil/<name>.dxil; the set od of the shader pick in dxil_od

# DXC variants and their preprocessor definitions.
VARIANTS = {
    'normal': [],
    # The library built with the uavsync feature of the crates takes this bytecode (RAV1G_SHADER_DIR).
    'uavsync': ['GroupMemoryBarrierWithGroupSync=AllMemoryBarrierWithGroupSync'],
    'shader-pick': [],
}

INTRA_MAIN = ['intra_main', 'intra_main_hbd']
# The -intraod comparison kernels are built without optimization in the default set.
UNOPTIMIZED = [name + '_legacy' for name in INTRA_MAIN]

# Verify the alternate kernels selected at runtime in the built binary.
REQUIRED_BYTECODE = [
    'cdef_filter', 'cdef_filter_v2', 'cdef_filter_v3', 'cdef_filter_v3_sp_tex', *INTRA_MAIN, *UNOPTIMIZED,
    'intra_main_prefetch_hbd', 'intra_main_prefetch',
    'intra_main_single_edge', 'intra_main_single_edge_hbd',
    'intra_filter_prefetch', 'intra_filter_prefetch_hbd',
    'cdef_lists', 'cdef_lists_v2',
    'intra_filter_legacy', 'intra_v2', 'intra_v2_hbd',
    'gen_lf_blocks_vert', 'gen_lf_blocks_hor',
    'loopfilter_vert', 'loopfilter_hor',
    'loopfilter_vert_hbd', 'loopfilter_hor_hbd',
    'loop_restoration', 'loop_restoration_wiener',
    'loop_restoration_sp_tex', 'loop_restoration_wiener_sp_tex',
    'upscaling_sp_tex',
    'film_grain_filter_sp_tex_v2',
    'copy_plane_sp_tex_v2',
    'display_pass', 'display_pass_sdr', 'display_pass_hlg',
    'idct4x4_v2', 'idct4x8_v2', 'idct8x4_v2', 'idct8x8_v2',
    'inter_main_v2', 'inter_main_v2_hbd',
    'inter_compound_v2', 'inter_compound_v2_hbd',
]

# What DXC is given for every shader on top of -T, -E and -Od:
#
#   -T cs_6_0   Shader Model 6.0, the lowest DXIL there is: what every device with DXIL takes. Nothing in
#               the sources asks for more.
#   -HV 2018    the language version whose && || and ?: evaluate both sides. DXC's default
#               (2021) short-circuits them, which is other control flow for the same sources (57 of the
#               shaders compile to other code with it). 2016 gives the same bytecode as 2018.
#   -WX         a warning is an error.
#   -Wno-ambig-lit-shift
#               the one warning the sources give (560 times): "1 << n" with a literal on the left, whose
#               type DXC could take as 64 bits. The default build refuses a listing with a 64-bit
#               value in it, so the shifts are 32-bit ones.
#   no -O       DXC's default, -O3. The shaders in UNOPTIMIZED use -Od.
#   no -Gis     the shaders have no floating point arithmetic (the listings have none), so IEEE
#               strictness has nothing to decide.
#   nothing that strips: the container keeps its reflection part.
DXC_PROFILE = 'cs_6_0'
DXC_ARGUMENTS = ['-HV', '2018', '-WX', '-Wno-ambig-lit-shift']


def fail(message):
    raise SystemExit('shaders.py: ' + message)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def project_shaders(shaders_dir=SHADERS):
    """Every .hlsl file: [(name, built with optimizations disabled)], by name."""
    names = sorted(name[:-5] for name in os.listdir(shaders_dir) if name.endswith('.hlsl'))
    if len(names) < 80:
        fail('only %d shaders found in %s' % (len(names), shaders_dir))
    for name in INTRA_MAIN + UNOPTIMIZED:
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
            # Relative to the including file.
            todo.append(os.path.normpath(os.path.join(os.path.dirname(rel), include.decode('utf-8'))).replace('\\', '/'))
    return sorted(seen.items())


def dxc_libraries(dxc):
    """The libraries DXC loads: next to it (Windows), or in ../lib (the release for Linux)."""
    directory = os.path.dirname(os.path.abspath(dxc))
    found = []
    for where in (directory, os.path.join(os.path.dirname(directory), 'lib')):
        if os.path.isdir(where):
            found += [os.path.join(where, other) for other in sorted(os.listdir(where))
                      if re.match(r'(lib)?(dxcompiler|dxil)\.(dll|so)$', other, re.I)]
    return found


def dxc_identity(dxc):
    """What decides DXC's output: the program, the compiler library and the validator."""
    if not dxc or not os.path.isfile(dxc):
        fail('dxc not found: %s' % dxc)
    parts = [('dxc', sha256_file(dxc))]
    parts += [(os.path.basename(path).lower(), sha256_file(path)) for path in dxc_libraries(dxc)]
    return hashlib.sha256(json.dumps(parts).encode()).hexdigest()


def dxc_version(dxc):
    """What DXC says it is, and the short form of it for the line the library reports."""
    text = ''
    for flag in ('--version', '-version'):
        done = subprocess.run([dxc, flag], stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        text = ' '.join(done.stdout.decode('utf-8', 'replace').split())
        if done.returncode == 0 and text:
            break
    match = re.search(r'\d+\.\d+\.\d+\.\d+', text) or re.search(r'\d+\.\d+(\([^)]*\))?', text)
    short = re.sub(r'[^0-9A-Za-z.()+-]', '_', match.group(0)) if match else 'of an unknown version'
    return text or 'version not reported', short


def dxc_arguments(od, variant, name=None):
    """The arguments that decide DXC's output; the output files and the source path come on top. DXC
    writes the bytecode (-Fo), and that file is what the library embeds."""
    args = ['-nologo', '-T', DXC_PROFILE, '-E', 'main'] + DXC_ARGUMENTS
    if od:
        args.append('-Od')
        if name in INTRA_MAIN:
            args += ['-D', 'INTRA_MAIN_LEGACY']
    for define in VARIANTS[variant]:
        args += ['-D', define]
    return args


def shader_key(name, od, variant, identity, shaders_dir=SHADERS):
    material = {'format': FORMAT, 'dxc': identity, 'arguments': dxc_arguments(od, variant, name),
                'name': name, 'files': source_files(name, shaders_dir)}
    return hashlib.sha256(json.dumps(material, sort_keys=True).encode()).hexdigest()


def entry_files(cache, key, name):
    directory = os.path.join(cache, key)
    return directory, os.path.join(directory, name + '.dxil'), os.path.join(directory, name + '.asm'), os.path.join(
        directory, 'meta.json')


def entry_complete(cache, key, name):
    _, bytecode, listing, meta = entry_files(cache, key, name)
    return all(os.path.isfile(p) and os.path.getsize(p) > 0 for p in (bytecode, listing, meta))


def messages(lines):
    """Compiler diagnostics without blank lines."""
    return [line.rstrip() for line in lines if line.strip()]


def container_parts(blob):
    """The four-character codes of the parts of a DXBC container."""
    count = int.from_bytes(blob[28:32], 'little')
    offsets = [int.from_bytes(blob[32 + 4 * i:36 + 4 * i], 'little') for i in range(count)]
    return [blob[offset:offset + 4].decode('ascii', 'replace') for offset in offsets]


def dxil_problem(blob, listing, allow_wide=False):
    """Why the output of a DXC call is not a shader of the build, or None. allow_wide: 64-bit values in
    the code are not a reason (the caller looks for them itself: dxil_wide)."""
    if len(blob) < 36 or blob[:4] != b'DXBC':
        return 'the output is not a DXBC container'
    if 'DXIL' not in container_parts(blob):
        return 'the container has no DXIL part'
    # Bytes 4..19: the validator's hash, zero when the bytecode is not signed. D3D12 takes no unsigned DXIL.
    if not any(blob[4:20]):
        return 'no validation hash: DXC did not find the validator (dxil.dll, libdxil.so)'
    # A 64-bit value in the code (not in the comments, the data layout or the metadata) would be a shift
    # or other expression whose literal DXC took as 64 bits. The sources have no 64-bit types.
    wide = dxil_wide(listing)
    if wide and not allow_wide:
        return '64-bit values in the listing, the first: %s' % wide
    return None


def dxil_wide(listing):
    """The first line of the code of a DXC listing with a 64-bit value, or None."""
    with open(listing, encoding='utf-8', errors='replace') as f:
        for line in f:
            if re.search(r'\bi64\b', line) and not re.match(r'\s*(;|!|target datalayout)', line):
                return line.strip()[:160]
    return None


def compile_shader(compiler, cache, name, od, variant, key, shaders_dir, allow_wide=False):
    """One compiler call: the bytecode and the listing of the shader, into a new cache entry. The
    bytecode is the file DXC wrote (-Fo), kept as it is: nothing rewrites it on its way into the library. allow_wide
    (the -Od set of the shader pick): DXIL with 64-bit values in its code is kept, and the entry says so
    ('wide' in meta.json); see build_od_set."""
    os.makedirs(cache, exist_ok=True)
    work = tempfile.mkdtemp(prefix='new-', dir=cache)
    listing = os.path.join(work, name + '.asm')
    source = os.path.join(shaders_dir, name + '.hlsl')
    blob_path = os.path.join(work, name + '.dxil')
    arguments = dxc_arguments(od, variant, name)
    command = [compiler] + arguments + ['-Fc', listing, '-Fo', blob_path, source]
    start = time.monotonic()
    done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    seconds = round(time.monotonic() - start, 1)
    output = messages(done.stdout.decode('utf-8', 'replace').splitlines())
    wide = None
    if done.returncode == 0 and os.path.isfile(blob_path) and os.path.isfile(listing):
        with open(blob_path, 'rb') as f:
            blob = f.read()
        problem = dxil_problem(blob, listing, allow_wide)
        wide = dxil_wide(listing)
        if problem:
            output.append(problem)
            os.remove(blob_path)
    ok = done.returncode == 0 and os.path.isfile(blob_path) and os.path.isfile(listing)
    if not ok:
        shutil.rmtree(work, ignore_errors=True)
        return {'ok': False, 'seconds': seconds, 'output': output, 'exit': done.returncode}
    with open(os.path.join(work, 'meta.json'), 'w', encoding='utf-8') as f:
        meta = {'format': FORMAT, 'name': name, 'variant': variant, 'od': od, 'seconds': seconds, 'output': output,
                'command': 'dxc ' + ' '.join(arguments) + ' ' + name + '.hlsl', 'validation_hash': blob[4:20].hex(),
                'bytecode_sha256': hashlib.sha256(blob).hexdigest()}
        if wide:
            meta['wide'] = wide
        json.dump(meta, f, indent=1)
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
    digest = hashlib.sha256()
    count = 0
    identity = dxc_identity(args.dxc)
    shaders = project_shaders()
    for variant in args.variants.split(','):
        if variant not in VARIANTS:
            fail('unknown variant %s' % variant)
        for name, od in shaders:
            digest.update(shader_key(name, od, variant, identity).encode())
            count += 1
            if variant == 'shader-pick' and not od:
                digest.update(shader_key(name, True, variant, identity).encode())
                count += 1
    value = digest.hexdigest()[:32]
    print('shaders.py: %d shader keys (%s), digest %s' % (count, args.variants, value))
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a', encoding='utf-8') as f:
            f.write('digest=%s\n' % value)
    return 0


def cache_entries(cache, compiler, variant, shaders, identity, jobs, what, allow_wide=False):
    """The cache entries of the shaders [(name, od)] of a variant, compiling the ones that are missing:
    ({name: key}, {name: the result of the compiler call of this run}, the number that were missing)."""
    keys = {name: shader_key(name, od, variant, identity) for name, od in shaders}
    missing = [(name, od) for name, od in shaders if not entry_complete(cache, keys[name], name)]
    results = {}
    if missing:
        print('shaders.py: %s: %d of %d shaders are not in the cache; DXC on up to %d at a time' %
              (what, len(missing), len(shaders), jobs), flush=True)
        with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = {pool.submit(compile_shader, compiler, cache, name, od, variant, keys[name], SHADERS,
                                   allow_wide): name for name, od in missing}
            for future in concurrent.futures.as_completed(futures):
                results[futures[future]] = future.result()
    failed = sorted(name for name, result in results.items() if not result['ok'])
    for name in failed:
        print('DXC failed for %s.hlsl (exit code %s):' % (name, results[name]['exit']))
        for line in results[name]['output']:
            print('  ' + line)
    if failed:
        fail('DXC failed for ' + ', '.join(name + '.hlsl' for name in failed))
    return keys, results, len(missing)


def copy_bytecode(source, target, expected=None):
    """The bytecode of a cache entry into a build, byte for byte: the SHA-256 of the copy. expected: the
    hash the entry recorded when DXC wrote the file."""
    shutil.copyfile(source, target)
    digest = sha256_file(target)
    if expected and digest != expected:
        fail('%s is not the bytecode DXC wrote into the cache entry (SHA-256 %s, recorded %s)' % (
            source, digest, expected))
    with open(target, 'rb') as f:
        if f.read(4) != b'DXBC':
            fail('%s is not a DXBC container' % target)
    return digest


def fresh_directory(path):
    """An empty directory: a build has the bytecode of its own shaders and of no earlier build."""
    shutil.rmtree(path, ignore_errors=True)
    os.makedirs(path)
    return path


def build_od_set(args, shaders, dxc_keys, identity, jobs, shader_set, report):
    """Build the alternate -Od bytecode and record the compiler and arguments of both sets.

    A shader whose -Od DXIL has 64-bit values in its code (the literal of a shift or of a ?: that DXC
    takes as 64 bits and that only its optimisation narrows; a device needs 64-bit integer shader
    operations for it) has the bytecode of the default set in the set od as well, and the line of the set
    names it."""
    unoptimized = ', '.join(name for name, od in shaders if od)
    od_shaders = [(name, True) for name, _ in shaders]
    keys, results, missing = cache_entries(args.cache, args.dxc, args.variant, od_shaders, identity, jobs,
                                           'shader-pick, the set od', allow_wide=True)
    directory = BYTECODE + '_od'
    bytecodes = fresh_directory(os.path.join(args.root, 'dx', 'shaders', directory))
    listings = os.path.join(args.listings, 'od') if args.listings else None
    if listings:
        os.makedirs(listings, exist_ok=True)
    entries = []
    wide = []
    for name, _ in shaders:
        key = keys[name]
        _, bytecode, listing, meta_path = entry_files(args.cache, key, name)
        with open(meta_path, encoding='utf-8') as f:
            meta = json.load(f)
        expected = meta['bytecode_sha256']
        is_wide = bool(meta.get('wide'))
        if listings:
            shutil.copyfile(listing, os.path.join(listings, name + '.asm'))
        if is_wide:
            # The bytecode of the default set instead (the listing stays the one of -Od).
            wide.append(name)
            key = dxc_keys[name]
            _, bytecode, _, default_meta = entry_files(args.cache, key, name)
            with open(default_meta, encoding='utf-8') as f:
                expected = json.load(f)['bytecode_sha256']
        digest = copy_bytecode(bytecode, os.path.join(bytecodes, name + '.dxil'), expected)
        entries.append({'name': name, 'od': not is_wide, 'key': keys[name], 'bytecode_key': key,
                        'compiled': name in results, 'seconds': meta['seconds'],
                        'bytecode_sha256': digest})

    dxc_line = 'dxc: %s: dxc %s; -Od for %s' % (shader_set, report['arguments'], unoptimized)
    od_line = 'od: %s: dxc %s; -D INTRA_MAIN_LEGACY for %s' % (
        shader_set, report['arguments_unoptimized'], ', '.join(INTRA_MAIN))
    if wide:
        od_line += '; default DXIL for %s (64-bit values at -Od)' % ', '.join(wide)
    lines = [dxc_line, od_line]
    report['sets'] = {'od': {'directory': directory, 'line': od_line,
                              'default_bytecode': wide, 'compiled': missing,
                              'from_cache': len(shaders) - missing, 'shaders': entries}}
    print('shaders.py: shader-pick, the set od: %d shaders, %d compiled now, %d from the cache; bytecode in %s' %
          (len(shaders), missing, len(shaders) - missing, bytecodes))
    with open(os.path.join(args.root, 'shader_sets.txt'), 'w', encoding='ascii', newline='\n') as f:
        f.write('\n'.join(lines) + '\n')
    report['set_lines'] = lines
    for line in lines:
        print('shaders.py: shader set ' + line)


def command_build(args):
    start = time.monotonic()
    identity = dxc_identity(args.dxc)
    shaders = project_shaders()
    jobs = max(1, args.jobs or (os.cpu_count() or 2))
    keys, results, missing = cache_entries(args.cache, args.dxc, args.variant, shaders, identity, jobs,
                                           args.variant)

    bytecodes = fresh_directory(os.path.join(args.root, 'dx', 'shaders', BYTECODE))
    if args.listings:
        os.makedirs(args.listings, exist_ok=True)
    version, short = dxc_version(args.dxc)
    shader_set = 'DXC %s %s' % (short, DXC_PROFILE)
    report = {'format': FORMAT, 'variant': args.variant, 'dxc': args.dxc, 'dxc_identity': identity,
              'dxc_version': version, 'dxc_libraries': [os.path.basename(path) for path in dxc_libraries(args.dxc)],
              'arguments': ' '.join(dxc_arguments(False, args.variant)),
              'arguments_unoptimized': ' '.join(dxc_arguments(True, args.variant)),
              'shader_set': shader_set, 'jobs': jobs,
              'compiled': missing, 'from_cache': len(shaders) - missing, 'shaders': []}
    print('shaders.py: %s: %s; dxc %s; -Od for %s' % (shader_set, version, report['arguments'], ', '.join(UNOPTIMIZED)))
    if args.variant == 'shader-pick':
        build_od_set(args, shaders, keys, identity, jobs, shader_set, report)
    for name, od in shaders:
        _, bytecode, listing, meta_path = entry_files(args.cache, keys[name], name)
        with open(meta_path, encoding='utf-8') as f:
            meta = json.load(f)
        if meta.get('wide'):
            fail('%s: the cache entry has 64-bit values in its code (%s)' % (name, meta['wide']))
        digest = copy_bytecode(bytecode, os.path.join(bytecodes, name + '.dxil'), meta['bytecode_sha256'])
        if args.listings:
            shutil.copyfile(listing, os.path.join(args.listings, name + '.asm'))
        report['shaders'].append({'name': name, 'od': od, 'key': keys[name], 'compiled': name in results,
                                  'seconds': meta['seconds'], 'output': messages(meta['output']),
                                  'validation_hash': meta['validation_hash'], 'bytecode_sha256': digest})
        # Compiler messages, also when the output came from the cache.
        for line in messages(meta['output']):
            print('%s.hlsl%s: %s' % (name, '' if name in results else ' (cached)', line))
    with open(os.path.join(args.root, 'shader_set.txt'), 'w', encoding='ascii', newline='\n') as f:
        f.write(shader_set + '\n')
    report['wall_seconds'] = round(time.monotonic() - start, 1)
    if args.report:
        os.makedirs(os.path.dirname(os.path.abspath(args.report)), exist_ok=True)
        with open(args.report, 'w', encoding='utf-8') as f:
            json.dump(report, f, indent=1)
    cpu = sum(results[name]['seconds'] for name in results)
    print('shaders.py: %s: %d shaders, %d compiled now (%.1f s of DXC in %.1f s), %d from the cache; bytecode in %s' %
          (args.variant, len(shaders), missing, cpu, report['wall_seconds'], report['from_cache'], bytecodes))
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('- shaders, %s: %d compiled in this run (%.1f s of DXC in %.1f s, up to %d at a time), %d from '
                    'the cache; %s (`%s`), `dxc %s`\n' % (args.variant, missing, cpu, report['wall_seconds'], jobs,
                                                       report['from_cache'], shader_set, version, report['arguments']))
    return 0


def command_prune(args):
    used = set()
    for path in args.reports:
        with open(path, encoding='utf-8') as f:
            report = json.load(f)
        used.update(shader['key'] for shader in report['shaders'])
        # The extra bytecode of the shader-pick variant.
        for other in report.get('sets', {}).values():
            used.update(shader['key'] for shader in other['shaders'])
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
# The bytecode of a build is in the program that was built from it.
# ---------------------------------------------------------------------------------------------
def built_bytecode(root, directory, shader):
    """The bytecode of a shader of a build, which has to be the file the build of the report wrote."""
    path = os.path.join(root, 'dx', 'shaders', directory, shader['name'] + '.dxil')
    try:
        with open(path, 'rb') as f:
            data = f.read()
    except OSError as error:
        fail('the bytecode of %s: %s' % (shader['name'], error))
    if hashlib.sha256(data).hexdigest() != shader['bytecode_sha256']:
        fail('%s is not the bytecode the build of the report wrote' % path)
    if data[:4] != b'DXBC':
        fail('%s is not a DXBC container' % path)
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
    lines = []
    ok = True
    for binary in args.binary:
        for program, data in program_bytes(binary):
            found = []
            absent = []
            for shader in report['shaders']:
                (found if built_bytecode(args.root, BYTECODE, shader) in data else absent).append(shader['name'])
            line = ('%s (%s): the bytecode of %d of the %d shaders of the %s build is in the program, byte for '
                    'byte' % (program, os.path.basename(binary), len(found), len(report['shaders']), report['variant']))
            if absent:
                line += '; not in it, as the table of the library does not name them: ' + ', '.join(absent)
            lines.append(line)
            print('shaders.py: ' + line)
            # The library embeds the shaders of its table (crates/rav1g-sys/shaders.txt): all but a few.
            if len(found) < args.min_found:
                ok = False
            # The shader-pick build also embeds its alternate sets.
            for set_name, other in sorted(report.get('sets', {}).items()):
                count = 0
                for shader in other['shaders']:
                    count += built_bytecode(args.root, other['directory'], shader) in data
                line = '%s (%s): the bytecode of %d of the %d shaders of the set %s is in the program' % (
                    program, os.path.basename(binary), count, len(other['shaders']), other['line'])
                lines.append(line)
                print('shaders.py: ' + line)
                if count < args.min_found:
                    ok = False
            missing = [name for name in REQUIRED_BYTECODE if name not in found]
            if missing:
                print('shaders.py: required shader bytecode missing from the program: ' + ', '.join(missing))
                ok = False
    if args.note:
        for path in args.note:
            with open(path, 'a', encoding='utf-8') as f:
                f.write('\n'.join([''] + lines) + '\n')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n'.join([''] + ['- ' + line for line in lines]) + '\n')
    if not ok:
        fail('the program was not built from the shader bytecode of this run')
    return 0


def command_same(args):
    """Compare the build reports by shader bytecode hash."""
    builds = []
    for path in (args.reference, args.candidate):
        with open(path, encoding='utf-8') as f:
            report = json.load(f)
        builds.append({shader['name']: shader for shader in report['shaders']})
    reference, candidate = builds
    different, changed = [], []
    # Shaders the candidate may have alone: those of a path the reference does not have, which the runs
    # that are compared do not take.
    alone = [name for name in args.candidate_only.split(',') if name]
    for name in alone:
        if name in reference:
            different.append('%s: named as a shader of %s alone, but %s has it' % (
                name, args.candidate_name, args.reference_name))
    expected_changes = set(filter(None, args.candidate_changed.split(',')))
    for name in sorted(set(reference) | set(candidate) | expected_changes):
        a, b = reference.get(name), candidate.get(name)
        if name in expected_changes:
            if a is None or b is None:
                missing = args.reference_name if a is None else args.candidate_name
                different.append('%s: named as changed on purpose but missing from %s' % (name, missing))
            elif a['bytecode_sha256'] == b['bytecode_sha256']:
                different.append('%s: named as changed on purpose but identical' % name)
            else:
                changed.append(name)
            continue
        if a is None and name in alone:
            continue
        if a is None or b is None:
            different.append('%s: only in %s' % (name, args.candidate_name if a is None else args.reference_name))
        elif a['bytecode_sha256'] != b['bytecode_sha256']:
            different.append('%s: bytecode differs (key %s.. in %s, %s.. in %s)' % (
                name, a['key'][:12], args.reference_name, b['key'][:12], args.candidate_name))
    for name in changed:
        print('CHANGED ON PURPOSE ' + name)
    for line in different:
        print('DIFFERENT ' + line)
    summary = ('%d shader comparison errors for %s and %s' % (len(different), args.reference_name, args.candidate_name)
               if different else
               '%d shaders of %s match %s%s' % (
                   len(reference) - len(changed), args.reference_name, args.candidate_name,
                   ', which has %d more that the compared runs do not use (%s)' % (
                       len([name for name in alone if name in candidate]), ', '.join(alone)) if alone else ''))
    if changed:
        summary += '; %d changed on purpose (%s)' % (len(changed), ', '.join(changed))
    print('shaders.py: ' + summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('- %s\n' % summary)
    return 1 if different else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)

    key = commands.add_parser('key', help='the digest of the keys of all shaders')
    key.add_argument('--dxc', required=True)
    key.add_argument('--variants', default='normal')
    key.set_defaults(run=command_key)

    build = commands.add_parser('build', help='bytecode and listings of one variant')
    build.add_argument('--dxc', required=True)
    build.add_argument('--cache', required=True, help='the cache directory')
    build.add_argument('--variant', choices=VARIANTS, default='normal')
    build.add_argument('--root', required=True, help='the bytecode goes to <root>/dx/shaders/dxil/<name>.dxil')
    build.add_argument('--listings', help='directory for the listings, <name>.asm')
    build.add_argument('--report', help='JSON file: the key, compile time and messages of every shader')
    build.add_argument('--jobs', type=int, default=0, help='compiler processes at a time (default: one per processor)')
    build.set_defaults(run=command_build)

    prune = commands.add_parser('prune', help='drop the cache entries the builds of these reports did not use')
    prune.add_argument('--cache', required=True)
    prune.add_argument('reports', nargs='+')
    prune.set_defaults(run=command_prune)

    verify = commands.add_parser('verify', help='the bytecode of a build is in a built program')
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
    same.add_argument('--candidate-only', default='',
                      help='names, with commas between them, of shaders the candidate may have alone')
    same.add_argument('--candidate-changed', default='',
                      help='names, with commas between them, of shaders that must be present and differ in both builds')
    same.set_defaults(run=command_same)

    args = parser.parse_args()
    # Keep diagnostics readable on consoles with a limited code page.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, 'reconfigure'):
            stream.reconfigure(errors='replace')
    return args.run(args)


if __name__ == '__main__':
    sys.exit(main())
