#!/usr/bin/env python3
"""Keeps the source list of the desktop decoder in step with the library project.

libav1/build/libav1.vcxproj is a Windows Store static library. warp_decoder is a desktop
program, so it compiles the library's sources and shaders itself, with the settings the
library project gives them in Release x64. The list of those files lives in
testing/warp_decoder/libav1_sources.props, which this script writes from libav1.vcxproj:

  python3 testing/warp_decoder/sync_sources.py           write libav1_sources.props
  python3 testing/warp_decoder/sync_sources.py --check   exit code 1 when it is out of date

Run it after a source file or a shader was added to or removed from libav1.vcxproj; CI
runs the check.
"""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
LIBAV1 = os.path.join(HERE, '..', '..', 'libav1', 'build', 'libav1.vcxproj')
OUT = os.path.join(HERE, 'libav1_sources.props')
RELEASE = "'$(Configuration)|$(Platform)'=='Release|x64'"


def items(text, kind):
    """(path below libav1, {metadata: value in Release x64}) for every <kind Include=...>."""
    found = []
    pattern = re.compile(r'<%s Include="([^"]+)"\s*(/>|>(.*?)</%s>)' % (kind, kind), re.S)
    for match in pattern.finditer(text):
        path = match.group(1).replace('/', '\\')
        if not path.startswith('..\\'):
            raise SystemExit('unexpected path in libav1.vcxproj: ' + path)
        meta = {}
        body = match.group(3) or ''
        for name, attrs, value in re.findall(r'<(\w+)([^>]*)>([^<]*)</\1>', body):
            condition = re.search(r'Condition="([^"]*)"', attrs)
            if condition and condition.group(1) != RELEASE:
                continue
            if value.strip():
                meta[name] = value.strip()
        found.append((path[3:], meta))
    return found


def build():
    with open(LIBAV1, encoding='utf-8-sig') as f:
        text = f.read()
    lines = [
        '<?xml version="1.0" encoding="utf-8"?>',
        '<!-- Written by sync_sources.py from libav1/build/libav1.vcxproj. Do not edit. -->',
        '<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">',
        '  <ItemGroup>',
    ]
    sources = items(text, 'ClCompile')
    shaders = items(text, 'FxCompile')
    if len(sources) < 70 or len(shaders) < 80:
        raise SystemExit('libav1.vcxproj: only %d sources and %d shaders found' % (len(sources), len(shaders)))
    for path, meta in sources:
        if meta.get('ExcludedFromBuild') == 'true':
            continue
        unknown = set(meta) - {'Optimization', 'ExcludedFromBuild'}
        if unknown:
            raise SystemExit('%s: settings this script does not carry over: %s' % (path, ', '.join(sorted(unknown))))
        include = '$(RepoRoot)libav1\\' + path
        if 'Optimization' in meta:
            lines.append('    <ClCompile Include="%s">' % include)
            lines.append('      <Optimization Condition="\'$(Configuration)\'==\'Release\'">%s</Optimization>'
                         % meta['Optimization'])
            lines.append('    </ClCompile>')
        else:
            lines.append('    <ClCompile Include="%s" />' % include)
    lines += ['  </ItemGroup>', '  <ItemGroup>']
    for path, meta in shaders:
        if meta.get('ExcludedFromBuild') == 'true':
            continue
        unknown = set(meta) - {'DisableOptimizations', 'ShaderType', 'FileType', 'ExcludedFromBuild',
                               'DeploymentContent', 'AssemblerOutput', 'AssemblerOutputFile'}
        if unknown:
            raise SystemExit('%s: settings this script does not carry over: %s' % (path, ', '.join(sorted(unknown))))
        if meta.get('ShaderType', 'Compute') != 'Compute':
            raise SystemExit('%s: not a compute shader' % path)
        include = '$(RepoRoot)libav1\\' + path
        if meta.get('DisableOptimizations') == 'true':
            lines.append('    <FxCompile Include="%s">' % include)
            lines.append('      <DisableOptimizations>true</DisableOptimizations>')
            lines.append('    </FxCompile>')
        else:
            lines.append('    <FxCompile Include="%s" />' % include)
    lines += ['  </ItemGroup>', '</Project>', '']
    return '\n'.join(lines), len(sources), len(shaders)


def main():
    text, sources, shaders = build()
    if '--check' in sys.argv[1:]:
        try:
            with open(OUT, encoding='utf-8') as f:
                current = f.read()
        except OSError:
            current = None
        if current != text:
            print('testing/warp_decoder/libav1_sources.props is out of date: run testing/warp_decoder/sync_sources.py')
            return 1
        print('libav1_sources.props matches libav1.vcxproj: %d sources, %d shaders' % (sources, shaders))
        return 0
    with open(OUT, 'w', encoding='utf-8', newline='\n') as f:
        f.write(text)
    print('wrote %s: %d sources, %d shaders' % (os.path.relpath(OUT), sources, shaders))
    return 0


if __name__ == '__main__':
    sys.exit(main())
