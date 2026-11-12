#!/usr/bin/env python3
"""Compiles the shaders of the Shader Model 6 test (docs/test-player.md, "Shader Model 6 test") with DXC.

These shaders are not shaders of the decoder: they are in testing/sm6test/shaders, they are compiled here
and nowhere else (not by the project files, not by testing/shader_cache), and they go into the players only
when the build is given the directory this script writes (/p:Rav1gSm6TestShaders=<out>).

  compile.py --dxc <dxc.exe> --out <directory>
  compile.py --verify <directory> <program or app package>...

The first form writes into <directory>:

  sm6test_generated/shaders.h   the bytecode of every shader as a C array, and the table the test reads
  dxil/<name>.dxil              the bytecode
  asm/<name>.txt                DXC's listing (-Fc)
  report.json, report.md        compiler, arguments, size, validation hash of every shader

A shader must compile, and its bytecode must carry a validation hash: DXC writes it when it finds the
validator (dxil.dll, libdxil.so) next to itself, and D3D12 refuses bytecode without it. A shader marked
optional may fail to compile (a profile this DXC does not have): the table then says why, and the test
reports the shader as not built. Exit code 1 when a required shader failed.

The second form checks that the programs were built with these shaders: the bytecode of every shader that
was compiled is in each program, byte for byte (in the program of an app package, which is a zip).
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))

# name, source, profile, further arguments, optional. The names are the ones testing/sm6test/sm6test.cpp
# has in its list of tests.
SHADERS = [
    ('basic', 'basic.hlsl', 'cs_6_0', [], False),
    ('wave_ops', 'wave_ops.hlsl', 'cs_6_0', [], False),
    ('reduce_groupshared', 'reduce.hlsl', 'cs_6_0', ['-D', 'REDUCE_GROUPSHARED'], False),
    ('reduce_wave', 'reduce.hlsl', 'cs_6_0', ['-D', 'REDUCE_WAVE'], False),
    ('native16_math', 'native16_math.hlsl', 'cs_6_2', ['-enable-16bit-types'], False),
    ('native16_load', 'native16_load.hlsl', 'cs_6_2', ['-enable-16bit-types'], False),
    ('wavesize4', 'wave_ops.hlsl', 'cs_6_6', ['-D', 'WAVE_SIZE=4'], False),
    ('wavesize8', 'wave_ops.hlsl', 'cs_6_6', ['-D', 'WAVE_SIZE=8'], False),
    ('wavesize16', 'wave_ops.hlsl', 'cs_6_6', ['-D', 'WAVE_SIZE=16'], False),
    ('wavesize32', 'wave_ops.hlsl', 'cs_6_6', ['-D', 'WAVE_SIZE=32'], False),
    ('wavesize64', 'wave_ops.hlsl', 'cs_6_6', ['-D', 'WAVE_SIZE=64'], False),
    ('reduce_wave64', 'reduce.hlsl', 'cs_6_6', ['-D', 'REDUCE_WAVE_ONLY'], False),
    # Shader Model 6.8: a range of wave sizes and the one the shader prefers.
    ('wavesize_range_pref32', 'wave_ops.hlsl', 'cs_6_8',
     ['-D', 'WAVE_MIN=4', '-D', 'WAVE_MAX=128', '-D', 'WAVE_PREF=32'], True),
    ('wavesize_range_pref64', 'wave_ops.hlsl', 'cs_6_8',
     ['-D', 'WAVE_MIN=4', '-D', 'WAVE_MAX=128', '-D', 'WAVE_PREF=64'], True),
]


def run(command):
    done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    return done.returncode, done.stdout.decode('utf-8', 'replace').replace('\r\n', '\n').strip()


def compiler_version(dxc):
    for flag in ('--version', '-version'):
        code, text = run([dxc, flag])
        if code == 0 and text:
            return ' '.join(text.split())
    return 'version not reported'


def c_string(text):
    out = ''
    for ch in text:
        if ch in '\\"':
            out += '\\' + ch
        elif 32 <= ord(ch) < 127:
            out += ch
        else:
            out += ' '
    return '"%s"' % out


def verify(directory, programs):
    with open(os.path.join(directory, 'report.json'), encoding='utf-8') as f:
        report = json.load(f)
    blobs = []
    for entry in report['shaders']:
        if entry['compiled'] and entry['signed']:
            with open(os.path.join(directory, 'dxil', entry['name'] + '.dxil'), 'rb') as f:
                blobs.append((entry['name'], f.read()))
    if not blobs:
        print('error: no compiled shader in %s' % directory)
        return 1
    missing = 0
    for path in programs:
        if zipfile.is_zipfile(path):
            with zipfile.ZipFile(path) as package:
                files = [(name, package.read(name)) for name in package.namelist() if name.lower().endswith('.exe')]
        else:
            with open(path, 'rb') as f:
                files = [(os.path.basename(path), f.read())]
        if not files:
            print('error: no program in %s' % path)
            return 1
        for name, data in files:
            absent = [shader for shader, blob in blobs if blob not in data]
            missing += len(absent)
            print('%s: %s' % (name, ('the bytecode of all %d Shader Model 6 test shaders is in the program' % len(blobs))
                              if not absent else 'NOT in the program: ' + ', '.join(absent)))
    return 1 if missing else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    parser.add_argument('--dxc', help='dxc.exe (or dxc), with the validator next to it')
    parser.add_argument('--out', help='directory to write')
    parser.add_argument('--verify', metavar='DIRECTORY', help='check the programs against the shaders in DIRECTORY')
    parser.add_argument('programs', nargs='*', help='with --verify: programs or app packages')
    args = parser.parse_args()
    if args.verify:
        if not args.programs:
            parser.error('--verify needs at least one program')
        return verify(args.verify, args.programs)
    if not args.dxc or not args.out or args.programs:
        parser.error('--dxc and --out are needed')

    if not os.path.isfile(args.dxc):
        print('error: %s is not there' % args.dxc)
        return 1
    for sub in ('sm6test_generated', 'dxil', 'asm'):
        os.makedirs(os.path.join(args.out, sub), exist_ok=True)
    version = compiler_version(args.dxc)
    # What makes the hash: the validator next to the compiler. Named in the report, not searched for: the
    # hash in the bytecode is what is checked.
    beside = sorted(name for name in os.listdir(os.path.dirname(os.path.abspath(args.dxc)))
                    if name.lower() in ('dxil.dll', 'dxcompiler.dll'))
    print('DXC: %s%s' % (version, ('; next to it: ' + ', '.join(beside)) if beside else ''))

    report = []
    failed = []
    for name, source, profile, extra, optional in SHADERS:
        path = os.path.join(HERE, 'shaders', source)
        dxil = os.path.join(args.out, 'dxil', name + '.dxil')
        listing = os.path.join(args.out, 'asm', name + '.txt')
        for stale in (dxil, listing):
            if os.path.exists(stale):
                os.remove(stale)
        command = [args.dxc, '-nologo', '-T', profile, '-E', 'main', '-O3'] + extra + ['-Fo', dxil, '-Fc', listing, path]
        code, text = run(command)
        entry = {'name': name, 'source': source, 'profile': profile, 'args': ' '.join(['-O3'] + extra),
                 'optional': optional, 'compiled': False, 'signed': False, 'size': 0, 'note': ''}
        blob = b''
        if code == 0 and os.path.isfile(dxil):
            with open(dxil, 'rb') as f:
                blob = f.read()
        if len(blob) >= 32 and blob[:4] == b'DXBC':
            entry['compiled'] = True
            entry['size'] = len(blob)
            # Bytes 4..19 of the container: the validator's hash, zero when the bytecode is not signed.
            entry['signed'] = any(blob[4:20])
            entry['hash'] = blob[4:20].hex()
            entry['sha256'] = hashlib.sha256(blob).hexdigest()
            if not entry['signed']:
                entry['note'] = 'no validation hash: DXC did not find the validator (dxil.dll)'
                failed.append(name)
            if text:
                entry['warnings'] = text
        else:
            lines = [line for line in text.split('\n') if line.strip()]
            entry['note'] = 'not compiled by %s: %s' % (version, lines[0][-200:] if lines else 'exit code %d' % code)
            entry['output'] = text
            if not optional:
                failed.append(name)
        entry['blob'] = blob if entry['compiled'] and entry['signed'] else b''
        report.append(entry)
        print('%-24s %-7s %-44s %s' % (
            name, profile, entry['args'],
            ('%5d bytes, hash %s' % (entry['size'], entry['hash'] if entry['signed'] else 'ABSENT'))
            if entry['compiled'] else ('NOT COMPILED%s: %s' % (' (optional)' if optional else '', entry['note']))))
        if text and (not entry['compiled'] or 'warning' in text):
            print('  ' + text.replace('\n', '\n  '))

    header = [
        '// Written by testing/sm6test/compile.py: the shaders of the Shader Model 6 test, compiled with DXC.',
        '// Not a file of the repository.',
        '',
        '#define SM6TEST_COMPILER %s' % c_string(version),
        '',
    ]
    for entry in report:
        if not entry['blob']:
            continue
        header.append('static const unsigned char sm6test_dxil_%s[] = {' % entry['name'])
        blob = entry['blob']
        for start in range(0, len(blob), 20):
            header.append('  ' + ' '.join('0x%02x,' % b for b in blob[start:start + 20]))
        header += ['};', '']
    header.append('static const Sm6ShaderBlob sm6test_shaders[] = {')
    for entry in report:
        if entry['blob']:
            code = 'sm6test_dxil_%s, sizeof(sm6test_dxil_%s)' % (entry['name'], entry['name'])
        else:
            code = '0, 0'
        header.append('  {%s, %s, %s, %s, %s},' % (c_string(entry['name']), c_string(entry['profile']),
                                                c_string(entry['args']), code, c_string(entry['note'])))
    header += ['};', '']
    with open(os.path.join(args.out, 'sm6test_generated', 'shaders.h'), 'w', encoding='ascii', newline='\n') as f:
        f.write('\n'.join(header))

    for entry in report:
        del entry['blob']
    with open(os.path.join(args.out, 'report.json'), 'w', encoding='utf-8', newline='\n') as f:
        json.dump({'compiler': version, 'beside': beside, 'shaders': report}, f, indent=1)
    table = ['Compiler: `%s`%s' % (version, ('; next to it: ' + ', '.join('`%s`' % b for b in beside)) if beside else ''),
             '',
             '| shader | source | profile | arguments | bytes | validation hash |',
             '|---|---|---|---|---|---|']
    for entry in report:
        table.append('| %s | %s | %s | `%s` | %s | %s |' % (
            entry['name'], entry['source'], entry['profile'], entry['args'],
            entry['size'] if entry['compiled'] else '-',
            ('`%s`' % entry['hash']) if entry['signed'] else (entry['note'] or 'absent').replace('|', '/')))
    with open(os.path.join(args.out, 'report.md'), 'w', encoding='utf-8', newline='\n') as f:
        f.write('\n'.join(table) + '\n')

    if failed:
        print('error: %d shaders failed: %s' % (len(failed), ', '.join(failed)))
        return 1
    built = sum(1 for entry in report if entry['compiled'] and entry['signed'])
    print('%d of %d shaders compiled, all with a validation hash' % (built, len(report)))
    return 0


if __name__ == '__main__':
    sys.exit(main())
