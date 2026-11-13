"""The licence notices of the Rust crates that are linked into the decoder.

The library (crates/rav1g) and the decoder program (tools/rav1g-dec) are built from crates of
crates.io besides the code of this repository: the crates rav1d depends on (third_party/rav1d) and the
Windows bindings. They are MIT or Apache-2.0 licensed, some under either, and both licences want their
text and the copyright notices passed on with a binary. This script writes them into one file,
NOTICES.txt next to it, which the packages carry in their licenses directory:

- the crates: every crate the two packages link for x86_64-pc-windows-msvc, as `cargo metadata --locked`
  resolves them from Cargo.lock (normal dependencies; no build scripts' crates, no procedural macros,
  which run at build time and are not in the binary), without the crates of this repository;
- per crate its name, version, licence expression and repository, and the licence files of its source
  (LICENSE*, COPYING*, NOTICE*, COPYRIGHT*); a text that several crates share is written once.

usage: python3 third_party/rust-crates/collect.py [--check]

--check: nothing is written; exit code 1 when NOTICES.txt is not what the script would write (CI runs
that: a change of Cargo.lock has to come with its notices). Needs cargo and the sources of the crates
in cargo's registry, which a build leaves there.
"""

import hashlib
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
TARGET = 'x86_64-pc-windows-msvc'
ROOTS = ['rav1g', 'rav1g-dec']
FILES = re.compile(r'(?i)^(licen[sc]e|copying|notice|copyright|unlicense)([-_.].*)?$')


def metadata():
    done = subprocess.run(['cargo', 'metadata', '--format-version', '1', '--locked', '--filter-platform', TARGET],
                          cwd=ROOT, stdout=subprocess.PIPE, check=True)
    return json.loads(done.stdout)


def linked_crates(meta):
    """The packages the roots link, without the workspace's own and without procedural macros."""
    packages = {package['id']: package for package in meta['packages']}
    nodes = {node['id']: node for node in meta['resolve']['nodes']}
    members = set(meta['workspace_members'])
    is_macro = lambda package: any('proc-macro' in target['kind'] for target in package['targets'])
    todo = [package['id'] for package in meta['packages'] if package['id'] in members and package['name'] in ROOTS]
    seen = set()
    while todo:
        package_id = todo.pop()
        if package_id in seen:
            continue
        seen.add(package_id)
        for dep in nodes[package_id]['deps']:
            if not any(kind['kind'] is None for kind in dep['dep_kinds']):
                continue
            if is_macro(packages[dep['pkg']]):
                continue
            todo.append(dep['pkg'])
    return sorted((packages[i] for i in seen if i not in members), key=lambda package: (package['name'], package['version']))


def licence_files(package):
    directory = os.path.dirname(package['manifest_path'])
    out = []
    for name in sorted(os.listdir(directory)):
        path = os.path.join(directory, name)
        if FILES.match(name) and os.path.isfile(path):
            with open(path, 'rb') as f:
                text = f.read().decode('utf-8', 'replace').replace('\r\n', '\n').replace('\r', '\n')
            out.append((name, '\n'.join(line.rstrip() for line in text.strip('\n').split('\n'))))
    return out


def notices():
    crates = linked_crates(metadata())
    lines = [
        'Licence notices of the Rust crates linked into rav1g',
        '=====================================================',
        '',
        'The decoder library and rav1g-dec are built with the crates below (crates.io), next to the code of',
        'the repository: the crates rav1d depends on (third_party/rav1d) and the Windows bindings. Each is',
        'used under the licence it names; where a crate offers a choice ("MIT OR Apache-2.0"), either. The',
        'texts and copyright notices of their sources follow the list. Written by',
        'third_party/rust-crates/collect.py from Cargo.lock; do not edit.',
        '',
    ]
    texts = {}
    missing = []
    for package in crates:
        label = '%s %s' % (package['name'], package['version'])
        lines.append('- %s: %s%s' % (label, package.get('license') or 'see its licence file',
                                    ' (%s)' % package['repository'] if package.get('repository') else ''))
        files = licence_files(package)
        if not files:
            missing.append(label)
        for name, text in files:
            key = hashlib.sha256(re.sub(r'\s+', ' ', text).encode('utf-8')).hexdigest()
            texts.setdefault(key, (text, []))[1].append('%s (%s)' % (label, name))
    if missing:
        lines += ['', 'Crates whose source has no licence file; their licence is the expression above:', '']
        lines += ['- ' + label for label in missing]
    for text, users in sorted(texts.values(), key=lambda item: item[1]):
        lines += ['', '', '-' * 100, 'The licence text of: ' + ', '.join(users), '-' * 100, '', text]
    return '\n'.join(lines) + '\n', len(crates), len(texts)


def main():
    target = os.path.join(HERE, 'NOTICES.txt')
    text, crates, texts = notices()
    if '--check' in sys.argv[1:]:
        try:
            with open(target, encoding='utf-8', newline='') as f:
                current = f.read()
        except OSError:
            current = ''
        if current != text:
            print('collect.py: third_party/rust-crates/NOTICES.txt is not what Cargo.lock gives (%d crates); run '
                  'python3 third_party/rust-crates/collect.py' % crates)
            return 1
        print('collect.py: NOTICES.txt has the notices of the %d crates of Cargo.lock (%d texts)' % (crates, texts))
        return 0
    with open(target, 'w', encoding='utf-8', newline='\n') as f:
        f.write(text)
    print('collect.py: wrote NOTICES.txt: %d crates, %d texts, %d bytes' % (crates, texts, len(text.encode('utf-8'))))
    return 0


if __name__ == '__main__':
    sys.exit(main())
