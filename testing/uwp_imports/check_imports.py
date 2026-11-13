"""The DLLs a program of the test player's package imports from, against a list.

The UWP test player runs in an app container on the console, where the desktop DLLs of Win32 are not
a given: a program that imports from kernel32.dll may not load there. The player's own code and the
C++ library import through the API sets of WindowsApp.lib; the Rust library brings import stubs of its
own for kernel32.dll (the windows crates) and two functions of ntdll.dll (the standard library), and
which of them the linker takes depends on the order of the libraries on its command line
(docs/rust-port.md, "The app container"). This check reads the import tables of the linked programs,
the normal one and the one of delayed imports, and fails on a DLL the list does not have and on a
function of a DLL that the list restricts to named functions.

Run: uv run --with pefile==2024.8.26 python check_imports.py --allow allow.txt [--report out.tsv] image.exe ...

The list has one DLL per line, without regard to case; # starts a comment. A line "name.dll: A, B"
allows only the functions A and B of that DLL.

The checker and the first list are GPT 6.1 Sol's, from its analysis of the imports of the Rust library
(2026-11-12).
"""

import argparse
import sys
from pathlib import Path

import pefile


def read_allow(path):
    """{dll: None (every function) or the set of its allowed functions}."""
    allowed = {}
    for line in path.read_text().splitlines():
        entry = line.split('#', 1)[0].strip()
        if not entry:
            continue
        name, _, functions = entry.partition(':')
        name = name.strip().casefold()
        if functions.strip():
            allowed[name] = {function.strip() for function in functions.split(',') if function.strip()}
        else:
            allowed[name] = None
    return allowed


def imports_of(path):
    """[(kind, dll, function)] of the normal and the delayed imports of a program."""
    rows = []
    with pefile.PE(str(path)) as pe:
        for directory, kind in (('DIRECTORY_ENTRY_IMPORT', 'normal'), ('DIRECTORY_ENTRY_DELAY_IMPORT', 'delay')):
            for entry in getattr(pe, directory, ()):
                dll = entry.dll.decode('ascii').casefold()
                for symbol in entry.imports:
                    rows.append((kind, dll, symbol.name.decode('ascii') if symbol.name else '#%d' % symbol.ordinal))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--allow', required=True, type=Path, help='the list of allowed DLLs')
    parser.add_argument('--also', action='append', default=[], metavar='DLL',
                        help='a DLL that is allowed in this run on top of the list (the desktop runtime of a link '
                             'that is not the player\'s)')
    parser.add_argument('--report', type=Path, help='writes every import of every image here (image, kind, DLL, function)')
    parser.add_argument('images', nargs='+', type=Path)
    args = parser.parse_args()
    allowed = read_allow(args.allow)
    for name in args.also:
        allowed[name.casefold()] = None
    failed = False
    report = ['image\tkind\tdll\tfunction']
    for path in args.images:
        try:
            rows = imports_of(path)
        except (OSError, pefile.PEFormatError, UnicodeDecodeError) as error:
            failed = True
            print('FAIL %s: %s' % (path.name, error))
            continue
        report += ['%s\t%s\t%s\t%s' % ((path.name,) + row) for row in rows]
        bad = {}
        for kind, dll, function in rows:
            if dll not in allowed:
                bad.setdefault((kind, dll, 'a DLL that is not in the list'), []).append(function)
            elif allowed[dll] is not None and function not in allowed[dll]:
                bad.setdefault((kind, dll, 'functions the list does not allow of this DLL'), []).append(function)
        for (kind, dll, why), functions in sorted(bad.items()):
            failed = True
            print('FAIL %s: %s import from %s, %s: %s' % (path.name, kind, dll, why, ', '.join(sorted(functions))))
        dlls = sorted({dll for _, dll, _ in rows})
        restricted = sorted('%s (%s)' % (dll, ', '.join(sorted({f for _, d, f in rows if d == dll})))
                            for dll in dlls if allowed.get(dll) is not None)
        print('%s %s: %d functions of %d DLLs%s' % (
            'checked' if not bad else 'FAILED', path.name, len(rows), len(dlls),
            '; of the restricted DLLs: ' + '; '.join(restricted) if restricted else ''))
    if args.report:
        args.report.write_text('\n'.join(report) + '\n')
    if not failed:
        print('PASS: %d image(s); every normal and delayed import is from an allowed DLL' % len(args.images))
    return int(failed)


if __name__ == '__main__':
    sys.exit(main())
