"""Takes the import members for desktop DLLs out of the static library of the decoder.

The windows crates declare every function of Windows they bind as an import from a named DLL
(raw-dylib), and rustc puts an import member for each of them into the static library: about a
thousand, most of them for kernel32.dll, for functions nothing calls as well. When an application links
the library, link.exe looks for a function first in the library whose object asked for it; so a call
of the Rust, of the C or of Rust's standard library that is in rav1g.lib takes the member for
kernel32.dll there, whatever stands in front of the library on the command line, and the program
imports from kernel32.dll. A program in the console's app container must not (docs/rust-port.md, "The
app container").

Without those members the same functions are looked for in the libraries behind rav1g.lib, where the
application puts WindowsApp.lib (the API sets) first. Kept are the members for DirectX (d3d12.dll,
dxgi.dll), for API sets (api-ms-*) and for bcryptprimitives.dll (the standard library's random
numbers, which only it declares).

usage: python prune_import_members.py --ar <llvm-ar> <rav1g.lib>

The library is rewritten in place. llvm-ar takes one member off per name it is given, so every name is
passed as often as the library has it.
"""

import argparse
import collections
import subprocess
import sys

KEEP = ('d3d12.dll', 'dxgi.dll', 'bcryptprimitives.dll')


def members(ar, library):
    out = subprocess.run([ar, 't', library], check=True, stdout=subprocess.PIPE, text=True).stdout
    return [line.strip() for line in out.splitlines() if line.strip()]


def is_pruned(name):
    lower = name.lower()
    return lower.endswith('.dll') and lower not in KEEP and not lower.startswith('api-ms-')


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--ar', required=True, help='llvm-ar')
    parser.add_argument('library')
    args = parser.parse_args()
    before = members(args.ar, args.library)
    drop = [name for name in before if is_pruned(name)]
    kept = collections.Counter(name.lower() for name in before if name.lower().endswith('.dll') and not is_pruned(name))
    if not drop:
        print('prune_import_members.py: %s has no import members for desktop DLLs' % args.library)
        return 0
    # In pieces: a command line has a length limit.
    for start in range(0, len(drop), 200):
        subprocess.run([args.ar, 'd', args.library] + drop[start:start + 200], check=True)
    after = members(args.ar, args.library)
    left = [name for name in after if is_pruned(name)]
    if left or len(after) != len(before) - len(drop):
        print('prune_import_members.py: %d members before, %d to take out, %d after, %d of them still there' % (
            len(before), len(drop), len(after), len(left)))
        return 1
    counts = collections.Counter(name.lower() for name in drop)
    print('prune_import_members.py: %s: %d import members taken out (%s); kept: %s; %d members left' % (
        args.library, len(drop), ', '.join('%s %d' % item for item in sorted(counts.items())),
        ', '.join('%s %d' % item for item in sorted(kept.items())) or 'none', len(after)))
    return 0


if __name__ == '__main__':
    sys.exit(main())
