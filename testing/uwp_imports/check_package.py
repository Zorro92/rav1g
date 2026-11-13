"""The imports of the programs of a test player package (check_imports.py), for the player job of CI.

Finds the application package (.msix) under a directory, unpacks its programs (.exe, .dll) and holds
the DLLs they import from against player-dlls-allow.txt. Writes, into --out: check.txt (what the check
printed), imports.tsv (every import of every program) and unpacked/ (the programs).

usage: python check_package.py --out <directory> [--need <file in the package>] [--also <DLL>]
                               <directory with the package>

--need: a file the package must have. --also: a DLL that is allowed on top of the list, for a package
that has a DLL of its own which its program imports from (rav1g.dll).
"""

import argparse
import os
import subprocess
import sys
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--out', required=True)
    parser.add_argument('--need', action='append', default=[], help='a file the package must have')
    parser.add_argument('--also', action='append', default=[], help='a DLL that is allowed on top of the list')
    parser.add_argument('packages')
    args = parser.parse_args()
    here = os.path.dirname(os.path.abspath(__file__))
    found = []
    for directory, _, files in os.walk(args.packages):
        if os.path.basename(directory).lower() == 'dependencies' or os.sep + 'Dependencies' + os.sep in directory + os.sep:
            continue
        found += [os.path.join(directory, name) for name in files if name.lower().endswith(('.msix', '.appx'))]
    if len(found) != 1:
        print('check_package.py: %d application packages under %s, expected one' % (len(found), args.packages))
        return 2
    unpacked = os.path.join(args.out, 'unpacked')
    os.makedirs(unpacked, exist_ok=True)
    with zipfile.ZipFile(found[0]) as package:
        names = package.namelist()
        programs = [name for name in names if name.lower().endswith(('.exe', '.dll'))]
        for name in programs:
            package.extract(name, unpacked)
    missing = [name for name in args.need if name.lower() not in [n.lower() for n in names]]
    if missing:
        print('check_package.py: %s has no %s' % (os.path.basename(found[0]), ', '.join(missing)))
        return 1
    if not programs:
        print('check_package.py: no program in %s' % found[0])
        return 2
    command = ['uv', 'run', '--quiet', '--with', 'pefile==2024.8.26', 'python', os.path.join(here, 'check_imports.py'),
               '--allow', os.path.join(here, 'player-dlls-allow.txt'), '--report', os.path.join(args.out, 'imports.tsv')]
    for name in args.also:
        command += ['--also', name]
    command += [os.path.join(unpacked, name) for name in programs]
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    text = '%s: %s\n%s' % (os.path.basename(found[0]), ', '.join(programs), result.stdout)
    print(text, end='')
    with open(os.path.join(args.out, 'check.txt'), 'w', encoding='utf-8') as f:
        f.write(text)
    return result.returncode


if __name__ == '__main__':
    sys.exit(main())
