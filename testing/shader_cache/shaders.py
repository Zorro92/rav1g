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
    shaders.py same    two builds have the same shaders, with explicit exceptions for the C++ equality check

The Rust library is built from the headers: RAV1G_SHADER_DIR=<root> (crates/rav1g-sys/build.rs).

The variant dxc is the DXC experiment (docs/gpu-av1-decoder.md, "DXC experiment"): the same sources
compiled by DXC for Shader Model 6.0 instead of by FXC for cs_5_0 (build --dxc <dxc> --variant dxc;
DXC_ARGUMENTS has the arguments and why). Its cache entries have the same files, the header in the
layout of FXC's (const BYTE <name>[]), and its build also writes <root>/shader_set.txt, the line the
library of the dxc feature of the crates reports about its shaders. The survey is of FXC listings only.

The build of the variant dxc has three bytecodes of every shader, for the library of the dxc feature to
choose from per shader when it creates its pipeline states (-shaderpick, docs/test-player.md, "Shader
pick"): dx/shaders/h (DXC as above, the one a pipeline state has unless told otherwise),
dx/shaders/h_od (DXC with -Od for every shader, the arrays named <name>_od) and dx/shaders/h_fxc (what
FXC writes for the variant normal, from the same cache entries by the same keys, the arrays named
<name>_fxc). <root>/shader_sets.txt has one line per set with the compiler and its arguments.
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

# The variant DXC compiles, and what it is given for every shader on top of -T, -E and -Od:
#
#   -T cs_6_0   Shader Model 6.0, the lowest DXIL there is: what every device with DXIL takes. Nothing in
#               the sources asks for more.
#   -HV 2018    the language version whose && || and ?: evaluate both sides, as FXC's do. DXC's default
#               (2021) short-circuits them, which is other control flow for the same sources (57 of the
#               shaders compile to other code with it). 2016 gives the same bytecode as 2018.
#   -WX         a warning is an error: what DXC objects to and FXC does not is to be seen and decided.
#   -Wno-ambig-lit-shift
#               the one warning the sources give (560 times): "1 << n" with a literal on the left, whose
#               type DXC could take as 64 bits where FXC's is int. It never does here: compile_dxc
#               refuses a listing with a 64-bit value in it, so the shifts are 32-bit ones as in FXC.
#   no -O       DXC's default, -O3; -Od for the shaders FXC builds with /Od.
#   no -Gis     the shaders have no floating point arithmetic (the listings have none), so IEEE
#               strictness has nothing to decide.
#   nothing that strips: the container keeps its reflection part, as FXC's keeps RDEF.
DXC_VARIANT = 'dxc'
VARIANTS[DXC_VARIANT] = []
DXC_PROFILE = 'cs_6_0'
DXC_ARGUMENTS = ['-HV', '2018', '-WX', '-Wno-ambig-lit-shift']
# The two other bytecodes of every shader in the build of the variant dxc (the shader pick of the DXC
# experiment): (the name of the set, the directory of its headers under dx/shaders, the suffix of its
# arrays). od: DXC with -Od for every shader; fxc: FXC's bytecode of the variant normal.
PICK_SETS = [('od', 'h_od', '_od'), ('fxc', 'h_fxc', '_fxc')]


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
    if not fxc or not os.path.isfile(fxc):
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


def dxc_arguments(name, od, variant):
    """The arguments that decide DXC's output; the output files and the source path come on top. No
    -Vn: DXC writes the bytecode (-Fo), and write_header the array with the name of the shader."""
    args = ['-nologo', '-T', DXC_PROFILE, '-E', 'main'] + DXC_ARGUMENTS
    if od:
        args.append('-Od')
    for define in VARIANTS[variant]:
        args += ['-D', define]
    return args


def shader_key(name, od, variant, identity, shaders_dir=SHADERS):
    if variant == DXC_VARIANT:
        material = {'format': FORMAT, 'dxc': identity, 'arguments': dxc_arguments(name, od, variant),
                    'name': name, 'files': source_files(name, shaders_dir)}
    else:
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
    # whose literal DXC took as 64 bits (-Wno-ambig-lit-shift in DXC_ARGUMENTS), which is not what FXC
    # compiles; and none of the sources has a 64-bit type.
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


def write_header(path, name, blob, comment):
    """The bytecode as the array FXC's /Fh writes: const BYTE <name>[], which shader_table.c includes."""
    lines = ['// ' + line for line in comment] + ['', 'const BYTE %s[] =' % name, '{']
    for start in range(0, len(blob), 16):
        lines.append('  ' + ', '.join('%3d' % b for b in blob[start:start + 16]) +
                     (',' if start + 16 < len(blob) else ''))
    lines += ['};', '']
    with open(path, 'w', encoding='ascii', newline='\n') as f:
        f.write('\n'.join(lines))


def compile_shader(compiler, cache, name, od, variant, key, shaders_dir, allow_wide=False):
    """One compiler call: the header and the listing of the shader, into a new cache entry. allow_wide
    (the -Od set of the shader pick): DXIL with 64-bit values in its code is kept, and the entry says so
    ('wide' in meta.json); see build_pick_sets."""
    os.makedirs(cache, exist_ok=True)
    work = tempfile.mkdtemp(prefix='new-', dir=cache)
    header = os.path.join(work, name + '.h')
    listing = os.path.join(work, name + '.asm')
    source = os.path.join(shaders_dir, name + '.hlsl')
    dxc = variant == DXC_VARIANT
    blob_path = os.path.join(work, name + '.dxil')
    if dxc:
        command = [compiler] + dxc_arguments(name, od, variant) + ['-Fc', listing, '-Fo', blob_path, source]
    else:
        command = [compiler] + fxc_arguments(name, od, variant) + ['/Fc', listing, '/Fh', header, source]
    start = time.monotonic()
    done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    seconds = round(time.monotonic() - start, 1)
    output = messages(done.stdout.decode('utf-8', 'replace').splitlines())
    wide = None
    if dxc and done.returncode == 0 and os.path.isfile(blob_path) and os.path.isfile(listing):
        with open(blob_path, 'rb') as f:
            blob = f.read()
        os.remove(blob_path)
        problem = dxil_problem(blob, listing, allow_wide)
        wide = dxil_wide(listing)
        if problem:
            output.append(problem)
        else:
            write_header(header, name, blob, [
                'Written by testing/shader_cache/shaders.py from the bytecode of',
                'dxc ' + ' '.join(dxc_arguments(name, od, variant)) + ' ' + name + '.hlsl',
                'validation hash ' + blob[4:20].hex()])
    ok = done.returncode == 0 and os.path.isfile(header) and os.path.isfile(listing)
    if not ok:
        shutil.rmtree(work, ignore_errors=True)
        return {'ok': False, 'seconds': seconds, 'output': output, 'exit': done.returncode}
    with open(os.path.join(work, 'meta.json'), 'w', encoding='utf-8') as f:
        meta = {'format': FORMAT, 'name': name, 'variant': variant, 'od': od, 'seconds': seconds, 'output': output}
        if dxc and wide:
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
    for variant in args.variants.split(','):
        if variant not in VARIANTS:
            fail('unknown variant %s' % variant)
        identity = dxc_identity(args.dxc) if variant == DXC_VARIANT else fxc_identity(args.fxc)
        for name, od in project_shaders():
            digest.update(shader_key(name, od, variant, identity).encode())
            count += 1
            if variant == DXC_VARIANT and not od:
                # The -Od bytecode of the shader pick. (Its FXC bytecode has the keys of the variant normal.)
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
    tool = 'DXC' if variant == DXC_VARIANT else 'FXC'
    keys = {name: shader_key(name, od, variant, identity) for name, od in shaders}
    missing = [(name, od) for name, od in shaders if not entry_complete(cache, keys[name], name)]
    results = {}
    if missing:
        print('shaders.py: %s: %d of %d shaders are not in the cache; %s on up to %d at a time' %
              (what, len(missing), len(shaders), tool, jobs), flush=True)
        with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = {pool.submit(compile_shader, compiler, cache, name, od, variant, keys[name], SHADERS,
                                   allow_wide): name for name, od in missing}
            for future in concurrent.futures.as_completed(futures):
                results[futures[future]] = future.result()
    failed = sorted(name for name, result in results.items() if not result['ok'])
    for name in failed:
        print('%s failed for %s.hlsl (exit code %s):' % (tool, name, results[name]['exit']))
        for line in results[name]['output']:
            print('  ' + line)
    if failed:
        fail('%s failed for ' % tool + ', '.join(name + '.hlsl' for name in failed))
    return keys, results, len(missing)


def copy_renamed_header(source, target, name, suffix):
    """A header of the cache with its array renamed <name><suffix>, so that the bytecodes of one shader
    by several compilers can be in one program."""
    with open(source, 'rb') as f:
        data = f.read()
    declaration = ('const BYTE %s[]' % name).encode()
    if data.count(declaration) != 1:
        fail('%s: not one "%s"' % (source, declaration.decode()))
    with open(target, 'wb') as f:
        f.write(data.replace(declaration, ('const BYTE %s%s[]' % (name, suffix)).encode()))


def build_pick_sets(args, shaders, dxc_keys, dxc_id, jobs, shader_set, report):
    """The two other bytecodes of every shader for the shader pick of the DXC experiment (PICK_SETS), in
    <root>/dx/shaders/h_od and h_fxc, and <root>/shader_sets.txt, the line of every set.

    A shader whose -Od DXIL has 64-bit values in its code (the literal of a shift or of a ?: that DXC
    takes as 64 bits and that only its optimisation narrows; a device needs 64-bit integer shader
    operations for it) has the bytecode of the default set in the set od as well, and the line of the set
    names it."""
    unoptimized = ', '.join(name for name, od in shaders if od)
    sources = {'od': (args.dxc, DXC_VARIANT, [(name, True) for name, _ in shaders], dxc_id),
               'fxc': (args.fxc, 'normal', shaders, None)}
    report['sets'] = {}
    lines = {'dxc': 'dxc: %s: dxc %s; -Od for %s' % (
        shader_set, ' '.join(dxc_arguments('', False, DXC_VARIANT)), unoptimized)}
    for set_name, directory, suffix in PICK_SETS:
        compiler, variant, set_shaders, identity = sources[set_name]
        if identity is None:
            identity = fxc_identity(compiler)
        keys, results, missing = cache_entries(args.cache, compiler, variant, set_shaders, identity, jobs,
                                               'dxc, the set %s' % set_name, allow_wide=set_name == 'od')
        headers = os.path.join(args.root, 'dx', 'shaders', directory)
        os.makedirs(headers, exist_ok=True)
        listings = os.path.join(args.listings, set_name) if args.listings else None
        if listings:
            os.makedirs(listings, exist_ok=True)
        entries = []
        wide = []
        for name, od in set_shaders:
            key = keys[name]
            _, header, listing, meta_path = entry_files(args.cache, key, name)
            with open(meta_path, encoding='utf-8') as f:
                is_wide = bool(json.load(f).get('wide'))
            if listings:
                shutil.copyfile(listing, os.path.join(listings, name + '.asm'))
            if is_wide:
                # The bytecode of the default set instead (the listing stays the one of -Od).
                wide.append(name)
                key = dxc_keys[name]
                header = entry_files(args.cache, key, name)[1]
            target = os.path.join(headers, name + '.h')
            copy_renamed_header(header, target, name, suffix)
            entries.append({'name': name, 'od': od and not is_wide, 'key': keys[name], 'bytecode_key': key,
                            'compiled': name in results, 'header_sha256': sha256_file(target)})
        if set_name == 'od':
            lines[set_name] = 'od: %s: dxc %s for every shader%s' % (
                shader_set, ' '.join(dxc_arguments('', True, DXC_VARIANT)),
                ' but %s (64-bit values at -Od: the bytecode of the set dxc)' % ', '.join(wide) if wide else '')
        else:
            lines[set_name] = 'fxc: FXC cs_5_0: fxc %s; /Od for %s' % (
                ' '.join(fxc_arguments('<name>', False, 'normal')), unoptimized)
        report['sets'][set_name] = {'directory': directory, 'suffix': suffix, 'line': lines[set_name],
                                    'default_bytecode': wide, 'compiled': missing,
                                    'from_cache': len(set_shaders) - missing, 'shaders': entries}
        print('shaders.py: dxc, the set %s: %d shaders, %d compiled now, %d from the cache; headers in %s' %
              (set_name, len(set_shaders), missing, len(set_shaders) - missing, headers))
    lines = [lines[name] for name in ('dxc', 'od', 'fxc')]
    with open(os.path.join(args.root, 'shader_sets.txt'), 'w', encoding='ascii', newline='\n') as f:
        f.write('\n'.join(lines) + '\n')
    report['set_lines'] = lines
    for line in lines:
        print('shaders.py: shader set ' + line)


def command_build(args):
    if args.variant not in VARIANTS:
        fail('unknown variant %s' % args.variant)
    start = time.monotonic()
    dxc = args.variant == DXC_VARIANT
    tool = 'DXC' if dxc else 'FXC'
    compiler = args.dxc if dxc else args.fxc
    identity = dxc_identity(compiler) if dxc else fxc_identity(compiler)
    shaders = project_shaders()
    jobs = max(1, args.jobs or (os.cpu_count() or 2))
    keys, results, missing = cache_entries(args.cache, compiler, args.variant, shaders, identity, jobs,
                                                 args.variant)

    headers = os.path.join(args.root, 'dx', 'shaders', 'h')
    os.makedirs(headers, exist_ok=True)
    if args.listings:
        os.makedirs(args.listings, exist_ok=True)
    if dxc:
        version, short = dxc_version(compiler)
        shader_set = 'DXC %s %s' % (short, DXC_PROFILE)
        report = {'format': FORMAT, 'variant': args.variant, 'dxc': compiler, 'dxc_identity': identity,
                  'dxc_version': version, 'dxc_libraries': [os.path.basename(path) for path in dxc_libraries(compiler)],
                  'arguments': ' '.join(dxc_arguments('', False, args.variant)),
                  'arguments_unoptimized': ' '.join(dxc_arguments('', True, args.variant)),
                  'shader_set': shader_set, 'jobs': jobs,
                  'compiled': missing, 'from_cache': len(shaders) - missing, 'shaders': []}
        # What the library of the dxc feature says about its shaders (crates/rav1g-sys/build.rs).
        with open(os.path.join(args.root, 'shader_set.txt'), 'w', encoding='ascii', newline='\n') as f:
            f.write(shader_set + '\n')
        print('shaders.py: %s: %s; dxc %s; -Od instead of the default for %s' % (
            shader_set, version, report['arguments'], ', '.join(UNOPTIMIZED)))
        build_pick_sets(args, shaders, keys, identity, jobs, shader_set, report)
    else:
        report = {'format': FORMAT, 'variant': args.variant, 'fxc': args.fxc, 'fxc_identity': identity, 'jobs': jobs,
                  'compiled': missing, 'from_cache': len(shaders) - missing, 'shaders': []}
    for name, od in shaders:
        _, header, listing, meta_path = entry_files(args.cache, keys[name], name)
        with open(meta_path, encoding='utf-8') as f:
            meta = json.load(f)
        if meta.get('wide'):
            fail('%s: the cache entry has 64-bit values in its code (%s)' % (name, meta['wide']))
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
    print('shaders.py: %s: %d shaders, %d compiled now (%.1f s of %s in %.1f s), %d from the cache; headers in %s' %
          (args.variant, len(shaders), missing, cpu, tool, report['wall_seconds'], report['from_cache'], headers))
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('- shaders, %s: %d compiled in this run (%.1f s of %s in %.1f s, up to %d at a time), %d from '
                    'the cache%s\n' % (args.variant, missing, cpu, tool, report['wall_seconds'], jobs,
                                       report['from_cache'],
                                       ('; %s (`%s`), `dxc %s`' % (report['shader_set'], report['dxc_version'],
                                                                  report['arguments'])) if dxc else ''))
    return 0


def command_prune(args):
    used = set()
    for path in args.reports:
        with open(path, encoding='utf-8') as f:
            report = json.load(f)
        used.update(shader['key'] for shader in report['shaders'])
        # The other bytecodes of the build of the variant dxc (the shader pick).
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
        # The original 8-bit -intraopt experiment still uses run-time widths.
        if name != 'intra_main_opt':
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
            # The build of the variant dxc: the other bytecodes of the shader pick are in the program too.
            for set_name, other in sorted(report.get('sets', {}).items()):
                count = 0
                for shader in other['shaders']:
                    header = os.path.join(args.root, 'dx', 'shaders', other['directory'], shader['name'] + '.h')
                    if sha256_file(header) != shader['header_sha256']:
                        fail('%s is not the header the build of the report wrote' % header)
                    count += header_bytecode(header) in data
                line = '%s (%s): the bytecode of %d of the %d shader headers of the set %s is in the program' % (
                    program, os.path.basename(binary), count, len(other['shaders']), other['line'])
                lines.append(line)
                print('shaders.py: ' + line)
                if count < args.min_found:
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
    """Compare the build reports by shader header hash."""
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
            elif a['header_sha256'] == b['header_sha256']:
                different.append('%s: named as changed on purpose but identical' % name)
            else:
                changed.append(name)
            continue
        if a is None and name in alone:
            continue
        if a is None or b is None:
            different.append('%s: only in %s' % (name, args.candidate_name if a is None else args.reference_name))
        elif a['header_sha256'] != b['header_sha256']:
            different.append('%s: headers differ (key %s.. in %s, %s.. in %s)' % (
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
    key.add_argument('--fxc', required=True)
    key.add_argument('--dxc', help='for the variant dxc')
    key.add_argument('--variants', default='normal,uavsync')
    key.set_defaults(run=command_key)

    build = commands.add_parser('build', help='headers and listings of one variant')
    build.add_argument('--fxc', help='for the variants FXC compiles')
    build.add_argument('--dxc', help='for the variant dxc')
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
    same.add_argument('--candidate-only', default='',
                      help='names, with commas between them, of shaders the candidate may have alone')
    same.add_argument('--candidate-changed', default='',
                      help='names, with commas between them, of shaders that must be present and differ in both builds')
    same.set_defaults(run=command_same)

    args = parser.parse_args()
    # What FXC printed goes to the log as it is, whatever the console's code page can show.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, 'reconfigure'):
            stream.reconfigure(errors='replace')
    return args.run(args)


if __name__ == '__main__':
    sys.exit(main())
