#!/usr/bin/env python3
"""Register counts, group memory and occupancy of the decoder's shaders, compiled offline for RDNA2.

Every shader of crates/rav1g-sys/shaders.txt is compiled by DXC to SPIR-V, with the arguments of the
real build (testing/shader_cache/shaders.py) and the SPIR-V flags below, then by LLPC, the Vulkan
compiler that AMD's Radeon GPU Analyzer (RGA) ships, for gfx1030 in wave64. RGA's binary analysis mode
(rga -s bin) reads the registers and the group memory (LDS) of the code object, the code object's PAL
metadata its wave size and thread group. The theoretical waves per SIMD follow from those
(occupancy() below). This is AMD's desktop compiler, not the console's: a change from one commit to
the next means something, the absolute numbers are approximate (testing/shader_regs/README.md).

    shader_regs.py fetch     download the pinned RGA and DXC releases into a tools directory, verified
    shader_regs.py measure   compile every shader, write the TSV (and the failures)
    shader_regs.py compare   hold a TSV against the baseline; exit 1 on a drop in waves or new scratch
    shader_regs.py have      exit 0 when the pinned tools are in the tools directory (for precheck.sh)
    shader_regs.py key       key=<the pins>, a cache key for the archives (the job shader-regs)

The tools directory: --tools, or $RAV1G_SHADER_REGS_TOOLS, or $XDG_CACHE_HOME/rav1g/shader_regs
(~/.cache/rav1g/shader_regs).
"""
import argparse
import concurrent.futures
import hashlib
import os
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, '..', '..'))
SHADERS = os.path.join(REPO, 'libav1', 'dx', 'shaders')
SHADER_LIST = os.path.join(REPO, 'crates', 'rav1g-sys', 'shaders.txt')
BASELINE = os.path.join(HERE, 'baseline.tsv')
KNOWN_FAILURES = os.path.join(HERE, 'known_failures.txt')

sys.path.insert(0, os.path.join(REPO, 'testing', 'shader_cache'))
import shaders as shader_build  # noqa: E402  the DXC arguments of the real build

# The pinned releases. RGA 2.13 is the newest whose LLPC (utils/amdllpc) still takes gfx10 targets:
# the LLPC of 2.14.1 and 2.15 answers "Invalid gfxip: gfx1030" (AMDVLK, LLPC's driver, was
# discontinued in 2025 and its RDNA2 support with it). DXC: the Linux release of the version line the
# real build uses (Windows SDK 10.0.26100.0, DXC 1.9.2602).
TOOLS = {
    'rga': {
        'version': '2.13',
        'url': 'https://github.com/GPUOpen-Tools/radeon_gpu_analyzer/releases/download/2.13/rga-linux-2.13.tgz',
        'sha256': '76da94f739b46ad753c0fd2d887d65aca7f3a4853fbba5b253a2883b77545868',
        'top': 'rga-2.13.0.27',
        # What is used of the archive: the command line tool, LLPC and the disassembler RGA calls. Not the
        # bundled Vulkan driver (utils/vulkan, under AMD's EULA), the GUI or its Qt libraries.
        'members': ['rga', 'rga-bin', 'LICENSE.txt', 'RGA_NOTICES.txt', 'utils/amdllpc', 'utils/lc/disassembler/'],
    },
    'dxc': {
        'version': 'v1.9.2602',
        'url': 'https://github.com/microsoft/DirectXShaderCompiler/releases/download/v1.9.2602/'
               'linux_dxc_2026_02_20.x86_64.tar.gz',
        'sha256': 'a1d3e3b5e1c5685b3eb27d5e8890e41d87df45def05112a2d6f1a63a931f7d60',
        'top': '',
        'members': ['bin/dxc', 'lib/', 'LICENSE-LLVM.txt', 'LICENSE-MS.txt'],
    },
}

TARGET = 'gfx1030'  # RDNA2 (Navi 21), the nearest public target to the console's GPU
GFXIP = '10.3.0'
WAVE = 64  # the console runs wave64 only

# SPIR-V on top of the real build's arguments. Each register class gets its own binding range: DXC
# maps t0, u0, b0 and s0 to the same binding by default, and LLPC's automatic descriptor layout would
# merge them into one descriptor of one type.
SPIRV_ARGUMENTS = ['-spirv', '-fspv-target-env=vulkan1.1',
                   '-fvk-t-shift', '0', 'all', '-fvk-u-shift', '100', 'all',
                   '-fvk-b-shift', '200', 'all', '-fvk-s-shift', '300', 'all']

# The occupancy model of LLVM's AMDGPU backend for gfx1030 in wave64 (AMDGPUBaseInfo.cpp): 512 VGPRs
# per lane per SIMD, allocated in granules of 8; at most 16 waves per SIMD; SGPRs do not limit
# occupancy on gfx10. Group memory: 64 KiB per compute unit of two SIMDs (CU mode, which LLPC
# picks: .wgp_mode 0; WGP mode has 128 KiB for four SIMDs, the same per SIMD), allocated in granules of
# 512 bytes.
VGPRS_PER_SIMD = 512
VGPR_GRANULE = 8
MAX_WAVES_PER_SIMD = 16
LDS_PER_CU = 65536
LDS_GRANULE = 512
SIMDS_PER_CU = 2

COLUMNS = ['shader', 'threads', 'wave', 'vgprs', 'sgprs', 'lds_bytes', 'scratch_bytes', 'waves_per_simd', 'limit']


def fail(message):
    raise SystemExit('shader_regs.py: ' + message)


def tools_dir(args_dir):
    if args_dir:
        return os.path.abspath(args_dir)
    if os.environ.get('RAV1G_SHADER_REGS_TOOLS'):
        return os.path.abspath(os.environ['RAV1G_SHADER_REGS_TOOLS'])
    cache = os.environ.get('XDG_CACHE_HOME') or os.path.join(os.path.expanduser('~'), '.cache')
    return os.path.join(cache, 'rav1g', 'shader_regs')


def tool_root(tools, name):
    spec = TOOLS[name]
    return os.path.join(tools, '%s-%s-%s' % (name, spec['version'], spec['sha256'][:12]))


def paths(tools):
    rga = tool_root(tools, 'rga')
    dxc = tool_root(tools, 'dxc')
    return {'rga': os.path.join(rga, 'rga'), 'amdllpc': os.path.join(rga, 'utils', 'amdllpc'),
            'dxc': os.path.join(dxc, 'bin', 'dxc')}


def have(tools):
    return all(os.path.isfile(p) and os.access(p, os.X_OK) for p in paths(tools).values())


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def fetch_one(tools, name, downloads):
    spec = TOOLS[name]
    root = tool_root(tools, name)
    if os.path.isdir(root):
        return root
    os.makedirs(downloads, exist_ok=True)
    archive = os.path.join(downloads, os.path.basename(spec['url']))
    if not os.path.isfile(archive) or sha256_file(archive) != spec['sha256']:
        print('shader_regs.py: downloading %s' % spec['url'], flush=True)
        partial = archive + '.part'
        with urllib.request.urlopen(spec['url'], timeout=120) as response, open(partial, 'wb') as f:
            shutil.copyfileobj(response, f, 1 << 20)
        os.replace(partial, archive)
    digest = sha256_file(archive)
    if digest != spec['sha256']:
        fail('%s has SHA-256 %s, the pin is %s' % (archive, digest, spec['sha256']))
    # Only the members that are used, each checked to stay inside the new directory.
    work = tempfile.mkdtemp(prefix=name + '-', dir=tools)
    prefix = spec['top'] + '/' if spec['top'] else ''
    wanted = [prefix + m for m in spec['members']]
    with tarfile.open(archive) as tar:
        members = []
        for member in tar.getmembers():
            path = member.name[2:] if member.name.startswith('./') else member.name
            if not any(path == w or (w.endswith('/') and path.startswith(w)) for w in wanted):
                continue
            if os.path.isabs(path) or '..' in path.split('/') or not (member.isfile() or member.isdir()):
                fail('%s: unexpected member %s' % (archive, member.name))
            member.name = path[len(prefix):]
            members.append(member)
        for w in wanted:
            if not any(m.name == w[len(prefix):] or (w.endswith('/') and m.name.startswith(w[len(prefix):]))
                       for m in members):
                fail('%s has no %s' % (archive, w))
        try:
            tar.extractall(work, members=members, filter='data')
        except TypeError:  # a Python without extraction filters (before 3.12 and its backports)
            tar.extractall(work, members=members)
    os.replace(work, root)
    return root


def command_fetch(args):
    tools = tools_dir(args.tools)
    os.makedirs(tools, exist_ok=True)
    downloads = args.downloads or os.path.join(tools, 'downloads')
    for name in TOOLS:
        print('shader_regs.py: %s %s in %s' % (name, TOOLS[name]['version'], fetch_one(tools, name, downloads)))
    return 0


def command_key(args):
    print('key=shader-regs-' + '-'.join('%s-%s-%s' % (name, TOOLS[name]['version'], TOOLS[name]['sha256'][:16])
                                        for name in sorted(TOOLS)))
    return 0


def command_have(args):
    return 0 if have(tools_dir(args.tools)) else 1


def shader_names():
    with open(SHADER_LIST, encoding='utf-8') as f:
        names = [line.strip() for line in f if line.strip() and not line.startswith('#')]
    if len(names) < 80:
        fail('only %d shaders in %s' % (len(names), SHADER_LIST))
    return names


def unoptimized(name):
    """Built with -Od by the real build (shaders.py, UNOPTIMIZED)."""
    return name in shader_build.UNOPTIMIZED


# A reader of the MessagePack that the PAL metadata note of an LLPC code object is written in.
def msgpack(data, pos=0):
    b = data[pos]
    pos += 1
    if b <= 0x7f:
        return b, pos
    if b >= 0xe0:
        return b - 256, pos
    if 0x80 <= b <= 0x8f or b in (0xde, 0xdf):
        if b <= 0x8f:
            n = b & 15
        else:
            size = 2 if b == 0xde else 4
            n = int.from_bytes(data[pos:pos + size], 'big')
            pos += size
        out = {}
        for _ in range(n):
            k, pos = msgpack(data, pos)
            v, pos = msgpack(data, pos)
            out[k] = v
        return out, pos
    if 0x90 <= b <= 0x9f or b in (0xdc, 0xdd):
        if b <= 0x9f:
            n = b & 15
        else:
            size = 2 if b == 0xdc else 4
            n = int.from_bytes(data[pos:pos + size], 'big')
            pos += size
        out = []
        for _ in range(n):
            v, pos = msgpack(data, pos)
            out.append(v)
        return out, pos
    if 0xa0 <= b <= 0xbf or b in (0xd9, 0xda, 0xdb):
        if b <= 0xbf:
            n = b & 31
        else:
            size = {0xd9: 1, 0xda: 2, 0xdb: 4}[b]
            n = int.from_bytes(data[pos:pos + size], 'big')
            pos += size
        return data[pos:pos + n].decode('utf-8', 'replace'), pos + n
    if b in (0xc4, 0xc5, 0xc6):
        size = {0xc4: 1, 0xc5: 2, 0xc6: 4}[b]
        n = int.from_bytes(data[pos:pos + size], 'big')
        pos += size
        return data[pos:pos + n], pos + n
    if b == 0xc0:
        return None, pos
    if b in (0xc2, 0xc3):
        return b == 0xc3, pos
    if b in (0xcc, 0xcd, 0xce, 0xcf):
        size = {0xcc: 1, 0xcd: 2, 0xce: 4, 0xcf: 8}[b]
        return int.from_bytes(data[pos:pos + size], 'big'), pos + size
    if b in (0xd0, 0xd1, 0xd2, 0xd3):
        size = {0xd0: 1, 0xd1: 2, 0xd2: 4, 0xd3: 8}[b]
        return int.from_bytes(data[pos:pos + size], 'big', signed=True), pos + size
    if b in (0xca, 0xcb):
        return struct.unpack('>f' if b == 0xca else '>d', data[pos:pos + (4 if b == 0xca else 8)])[0], pos + (
            4 if b == 0xca else 8)
    raise ValueError('MessagePack type 0x%02x' % b)


def pal_metadata(elf_path):
    """The PAL metadata of an LLPC code object (ELF64, note AMDGPU type 32, NT_AMDGPU_METADATA)."""
    with open(elf_path, 'rb') as f:
        data = f.read()
    if data[:4] != b'\x7fELF' or data[4] != 2:
        raise ValueError('not an ELF64 file')
    shoff, = struct.unpack_from('<Q', data, 0x28)
    shentsize, shnum = struct.unpack_from('<HH', data, 0x3a)
    for i in range(shnum):
        sh_type, = struct.unpack_from('<I', data, shoff + i * shentsize + 4)
        if sh_type != 7:  # SHT_NOTE
            continue
        offset, size = struct.unpack_from('<QQ', data, shoff + i * shentsize + 0x18)
        pos = offset
        while pos + 12 <= offset + size:
            namesz, descsz, note_type = struct.unpack_from('<III', data, pos)
            name = data[pos + 12:pos + 12 + namesz].rstrip(b'\0')
            desc = pos + 12 + ((namesz + 3) & ~3)
            if name == b'AMDGPU' and note_type == 32:
                return msgpack(data[desc:desc + descsz])[0]
            pos = desc + ((descsz + 3) & ~3)
    raise ValueError('no AMDGPU metadata note')


def align(value, granule):
    return (value + granule - 1) // granule * granule


def occupancy(vgprs, lds_bytes, threads):
    """Theoretical waves per SIMD and what limits them: vgprs, lds or waves (the hardware's limit)."""
    waves_per_group = max(1, -(-threads // WAVE))
    by_vgprs = min(MAX_WAVES_PER_SIMD, VGPRS_PER_SIMD // align(max(vgprs, 1), VGPR_GRANULE))
    # Whole groups per compute unit, each limit in turn.
    limits = {'waves': MAX_WAVES_PER_SIMD * SIMDS_PER_CU // waves_per_group,
              'vgprs': by_vgprs * SIMDS_PER_CU // waves_per_group}
    if lds_bytes:
        limits['lds'] = LDS_PER_CU // align(lds_bytes, LDS_GRANULE)
    groups = min(limits.values())
    limit = next(k for k in ('waves', 'vgprs', 'lds') if limits.get(k) == groups)
    waves = groups * waves_per_group / SIMDS_PER_CU
    return min(waves, MAX_WAVES_PER_SIMD), limit


def run(command, cwd=None, env=None, timeout=900):
    done = subprocess.run(command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                          timeout=timeout)
    return done.returncode, done.stdout.decode('utf-8', 'replace')


def last_lines(text, n=3):
    lines = [line.strip() for line in text.splitlines() if line.strip() and not line.startswith('=====')]
    return ' / '.join(lines[-n:])[:300]


def measure_one(name, tool_paths, work):
    """One shader: (row dict, None) or (None, reason)."""
    directory = os.path.join(work, name)
    os.makedirs(directory)
    spv = os.path.join(directory, name + '.spv')
    arguments = shader_build.dxc_arguments(unoptimized(name), 'normal', name)
    code, out = run([tool_paths['dxc']] + arguments + SPIRV_ARGUMENTS + ['-Fo', spv, os.path.join(SHADERS, name + '.hlsl')])
    if code != 0 or not os.path.isfile(spv):
        return None, 'DXC to SPIR-V: ' + last_lines(out)
    rga_dir = os.path.dirname(tool_paths['rga'])
    env = dict(os.environ)
    env['LD_LIBRARY_PATH'] = os.pathsep.join([rga_dir, os.path.join(rga_dir, 'utils')] + (
        [env['LD_LIBRARY_PATH']] if env.get('LD_LIBRARY_PATH') else []))
    env['HOME'] = directory  # RGA writes its temporary files under $HOME/.rga
    # LLPC's automatic descriptor layout works on a SPIR-V file only. Its pipeline dump is that layout as a
    # pipeline file, which then gets the wave size and is compiled again.
    dump = os.path.join(directory, 'dump')
    os.makedirs(dump)
    code, out = run([tool_paths['amdllpc'], '--auto-layout-desc', '--enable-pipeline-dump',
                     '--pipeline-dump-dir=' + dump, '--gfxip=' + GFXIP, '-o', os.path.join(directory, 'auto.elf'), spv],
                    cwd=directory, env=env)
    pipes = [f for f in os.listdir(dump) if f.endswith('.pipe')]
    if code != 0 or len(pipes) != 1:
        return None, 'LLPC: ' + last_lines(out)
    pipe = os.path.join(dump, pipes[0])
    with open(pipe, encoding='utf-8') as f:
        text = f.read()
    # The wave size LLPC picks by itself can change with the shader: it is set.
    line = '\noptions.waveSize = 0\n'
    if line not in text:
        return None, 'the pipeline dump of LLPC has no options.waveSize = 0'
    with open(os.path.join(dump, 'wave.pipe'), 'w', encoding='utf-8') as f:
        f.write(text.replace(line, '\noptions.waveSize = %d\n' % WAVE))
    elf = os.path.join(directory, name + '.elf')
    code, out = run([tool_paths['amdllpc'], '--gfxip=' + GFXIP, '-o', elf, 'wave.pipe'], cwd=dump, env=env)
    if code != 0 or not os.path.isfile(elf):
        return None, 'LLPC (wave%d): ' % WAVE + last_lines(out)
    try:
        meta = pal_metadata(elf)
        cs = meta['amdpal.pipelines'][0]['.hardware_stages']['.cs']
    except (ValueError, KeyError, IndexError, TypeError) as e:
        return None, 'PAL metadata: %s' % e
    dims = cs.get('.threadgroup_dimensions') or [0, 0, 0]
    threads = dims[0] * dims[1] * dims[2]
    if not threads:
        return None, 'no thread group size in the PAL metadata'
    wave = cs.get('.wavefront_size')
    # LLPC makes wave32 code for a group of at most 32 threads whatever the options say (waveSize,
    # subgroupSize, --native-wave-size): such a shader is measured in wave32, with its occupancy as one
    # wave64 per group (the console's), from the same register count.
    if wave != WAVE and not (wave == 32 and threads <= 32):
        return None, 'LLPC made wave%s code for a group of %d threads' % (wave, threads)
    # RGA's analysis of the code object.
    analysis = os.path.join(directory, 'analysis.csv')
    code, out = run([tool_paths['rga'], '-s', 'bin', '-a', analysis, '--co', elf], cwd=directory, env=env)
    csvs = [f for f in os.listdir(directory) if f.startswith(TARGET) and f.endswith('.csv')]
    if code != 0 or len(csvs) != 1:
        return None, 'RGA analysis: ' + last_lines(out)
    with open(os.path.join(directory, csvs[0]), encoding='utf-8') as f:
        lines = [line.strip().split(',') for line in f if line.strip()]
    if len(lines) != 2:
        return None, 'RGA analysis: %d lines' % len(lines)
    stats = dict(zip(lines[0], lines[1]))
    try:
        vgprs = int(stats['USED_VGPRs'])
        sgprs = int(stats['USED_SGPRs'])
        lds = int(stats['USED_LDS_BYTES'])
        scratch = int(stats['SCRATCH_MEM'])
        spills = int(stats['VGPR_SPILLS']) + int(stats['SGPR_SPILLS'])
    except (KeyError, ValueError) as e:
        return None, 'RGA analysis: %s' % e
    # RGA and the metadata have to agree on what they both report.
    if vgprs != cs.get('.vgpr_count') or lds != cs.get('.lds_size', 0) or scratch != cs.get('.scratch_memory_size', 0):
        return None, 'RGA (%d VGPRs, %d LDS bytes, %d scratch) and the PAL metadata (%s, %s, %s) differ' % (
            vgprs, lds, scratch, cs.get('.vgpr_count'), cs.get('.lds_size'), cs.get('.scratch_memory_size'))
    if spills and not scratch:
        scratch = -1  # spills without scratch would be a reader error; keep it visible
    waves, limit = occupancy(vgprs, lds, threads)
    shutil.rmtree(directory, ignore_errors=True)
    return {'shader': name, 'threads': threads, 'wave': wave, 'vgprs': vgprs, 'sgprs': sgprs, 'lds_bytes': lds,
            'scratch_bytes': scratch, 'waves_per_simd': format_waves(waves), 'limit': limit}, None


def format_waves(waves):
    return ('%.1f' % waves).rstrip('0').rstrip('.')


def write_tsv(path, rows):
    with open(path, 'w', encoding='utf-8') as f:
        f.write('\t'.join(COLUMNS) + '\n')
        for row in rows:
            f.write('\t'.join(str(row[c]) for c in COLUMNS) + '\n')


def read_tsv(path):
    rows = {}
    with open(path, encoding='utf-8') as f:
        header = None
        for line in f:
            if not line.strip() or line.startswith('#'):
                continue
            fields = line.rstrip('\n').split('\t')
            if header is None:
                header = fields
                if header != COLUMNS:
                    fail('%s: the columns are %s, expected %s' % (path, header, COLUMNS))
                continue
            rows[fields[0]] = dict(zip(header, fields))
    return rows


def read_known_failures(path):
    known = {}
    if os.path.isfile(path):
        with open(path, encoding='utf-8') as f:
            for line in f:
                if line.strip() and not line.startswith('#') and not line.startswith('shader\treason'):
                    name, _, why = line.rstrip('\n').partition('\t')
                    known[name.strip()] = why.strip()
    return known


def command_measure(args):
    tools = tools_dir(args.tools)
    if not have(tools):
        fail('the pinned tools are not in %s: run shader_regs.py fetch' % tools)
    tool_paths = paths(tools)
    names = shader_names()
    if args.only:
        names = [n for n in names if n in args.only.split(',')]
    scratch_parent = os.environ.get('TMPDIR') or None
    work = tempfile.mkdtemp(prefix='shader-regs-', dir=scratch_parent)
    rows, failures = {}, {}
    start = time.monotonic()
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = {pool.submit(measure_one, name, tool_paths, work): name for name in names}
            for future in concurrent.futures.as_completed(futures):
                name = futures[future]
                try:
                    row, why = future.result()
                except subprocess.TimeoutExpired:
                    row, why = None, 'timed out'
                if row:
                    rows[name] = row
                else:
                    failures[name] = why
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    write_tsv(args.out, [rows[n] for n in names if n in rows])
    with open(args.failures, 'w', encoding='utf-8') as f:
        f.write('shader\treason\n')
        for name in names:
            if name in failures:
                f.write('%s\t%s\n' % (name, failures[name]))
    print('shader_regs.py: %d of %d shaders measured, %d could not be compiled, %.0f s (%s, wave%d, RGA %s, DXC %s)' % (
        len(rows), len(names), len(failures), time.monotonic() - start, TARGET, WAVE, TOOLS['rga']['version'],
        TOOLS['dxc']['version']))
    for name in names:
        if name in failures:
            print('  %s: %s' % (name, failures[name]))
    return 0


def waves_value(text):
    return float(text)


def command_compare(args):
    baseline = read_tsv(args.baseline)
    results = read_tsv(args.results)
    failures = read_known_failures(args.failures) if args.failures else {}
    known = read_known_failures(args.known)
    names = shader_names()
    problems, notes, table = [], [], []
    for name in names:
        new, old = results.get(name), baseline.get(name)
        if new is None:
            why = failures.get(name, 'not measured')
            if name in known:
                notes.append('%s: not compiled (known_failures.txt: %s): %s' % (name, known[name], why))
            else:
                problems.append('%s: could not be compiled and is not in known_failures.txt: %s' % (name, why))
            continue
        if name in known:
            notes.append('%s: compiles now; remove it from known_failures.txt' % name)
        scratch = int(new['scratch_bytes'])
        change = ''
        if old is None:
            change = 'new'
            if scratch:
                problems.append('%s: %s bytes of scratch (spills), and no baseline row' % (name, scratch))
            else:
                notes.append('%s: not in the baseline' % name)
        else:
            if waves_value(new['waves_per_simd']) < waves_value(old['waves_per_simd']):
                problems.append('%s: waves per SIMD %s -> %s (%s VGPRs -> %s, %s LDS bytes -> %s)' % (
                    name, old['waves_per_simd'], new['waves_per_simd'], old['vgprs'], new['vgprs'],
                    old['lds_bytes'], new['lds_bytes']))
                change = 'waves %s -> %s' % (old['waves_per_simd'], new['waves_per_simd'])
            if scratch > int(old['scratch_bytes']):
                problems.append('%s: scratch %s -> %s bytes (spills)' % (name, old['scratch_bytes'], scratch))
                change = (change + ', ' if change else '') + 'scratch %s -> %s' % (old['scratch_bytes'], scratch)
            if not change:
                diffs = ['%s %s -> %s' % (c, old[c], new[c]) for c in COLUMNS[1:] if old[c] != new[c]]
                if diffs:
                    change = ', '.join(diffs)
                    notes.append('%s: %s (not a regression; update the baseline to keep it)' % (name, change))
        table.append((name, new, change))
    for name in baseline:
        if name not in names:
            notes.append('%s: in the baseline, not in shaders.txt' % name)

    print('shader_regs.py: %d shaders compared with %s' % (len(table), os.path.relpath(args.baseline, REPO)))
    for line in problems:
        print('FAIL ' + line)
    for line in notes:
        print('note ' + line)
    if args.summary:
        write_summary(args.summary, table, problems, notes, known, failures)
    if problems:
        print('shader_regs.py: a drop in waves per SIMD or new scratch is a decision: if it is meant, update '
              'testing/shader_regs/baseline.tsv in the same commit (copy the measured TSV) and say why in the '
              'commit message')
        return 1
    return 0


def write_summary(path, table, problems, notes, known, failures):
    with open(path, 'a', encoding='utf-8') as f:
        f.write('## Shader registers and occupancy (%s, wave%d, RGA %s; approximate, see testing/shader_regs)\n\n' % (
            TARGET, WAVE, TOOLS['rga']['version']))
        if problems:
            f.write('**Regressions against the baseline:**\n\n')
            for line in problems:
                f.write('- %s\n' % line)
            f.write('\n')
        if notes:
            f.write('<details><summary>%d notes</summary>\n\n' % len(notes))
            for line in notes:
                f.write('- %s\n' % line)
            f.write('\n</details>\n\n')
        f.write('Lowest occupancy first.\n\n')
        f.write('| shader | threads | VGPRs | SGPRs | LDS bytes | scratch | waves/SIMD | limit | change |\n')
        f.write('| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |\n')
        for name, row, change in sorted(table, key=lambda t: (waves_value(t[1]['waves_per_simd']), t[0])):
            f.write('| %s | %s | %s | %s | %s | %s | %s | %s | %s |\n' % (
                name, row['threads'], row['vgprs'], row['sgprs'], row['lds_bytes'], row['scratch_bytes'],
                row['waves_per_simd'], row['limit'], change))
        not_compiled = [n for n in failures]
        if not_compiled:
            f.write('\nNot compiled:\n\n')
            for name in not_compiled:
                f.write('- %s: %s%s\n' % (name, failures[name], ' (known: %s)' % known[name] if name in known else ''))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('fetch', help='download and verify the pinned tools')
    p.add_argument('--tools')
    p.add_argument('--downloads', help='where the archives are kept (default: <tools>/downloads)')
    p.set_defaults(func=command_fetch)
    p = sub.add_parser('have', help='exit 0 when the pinned tools are there')
    p.add_argument('--tools')
    p.set_defaults(func=command_have)
    p = sub.add_parser('key', help='a cache key for the pinned archives')
    p.set_defaults(func=command_key)
    p = sub.add_parser('measure', help='compile every shader and write the TSV')
    p.add_argument('--tools')
    p.add_argument('--out', required=True)
    p.add_argument('--failures', required=True, help='TSV of the shaders that could not be compiled, and why')
    p.add_argument('--jobs', type=int, default=4)
    p.add_argument('--only', help='comma-separated shader names')
    p.add_argument('--keep', action='store_true', help='keep the work directory')
    p.set_defaults(func=command_measure)
    p = sub.add_parser('compare', help='hold a TSV against the baseline')
    p.add_argument('results')
    p.add_argument('--baseline', default=BASELINE)
    p.add_argument('--known', default=KNOWN_FAILURES)
    p.add_argument('--failures', help='the failures TSV of measure')
    p.add_argument('--summary', help='append a Markdown table to this file (GITHUB_STEP_SUMMARY)')
    p.set_defaults(func=command_compare)
    args = parser.parse_args()
    return args.func(args)


if __name__ == '__main__':
    sys.exit(main())
