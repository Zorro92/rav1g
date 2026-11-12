#!/usr/bin/env python3
"""Scan of the FXC listings for the instruction pair that the console's driver compiles wrongly.

The pair, from the 8-tap branch of the 8-bit vertical loop filter as FXC compiled it
(docs/gpu-av1-decoder.md, "A paired read before an overlapping in-place write"):

    iadd r2.x, r3.w, -r3.y            ; q0.w - p1.x: reads two components of r3 into one result
    iadd r1.w, r1.w, r2.x
    iadd r2.x, r3.x, r1.w
    iadd r2.x, r2.x, l(4)
    iadd r3.yw, r3.wwww, -r4.xxxy     ; rewrites exactly those two components of r3, from r3.w

WARP runs it as written. The console ran the first instruction as r3.z - r3.x: with that one change a
model of the filter gives the console's plane to the last sample (a 3840x2160 frame, 40240 edges in
that branch), and nothing else in the two 8-bit loop filters differs from the listings.

What the scan looks for, in two steps:

    overlapping in-place write
        an instruction that writes components of a temp register rN and reads, of the same register,
        for at least one written component c, a component d that it also writes, d != c
        (r3.y from r3.w above). Its result depends on the order in which components are read and
        written; DXBC says all sources are read first. FXC emits it in most shaders (1649 times in
        the 92 when this was written), and the console runs the ones that were checked correctly, so
        on its own it is counted, not refused.

    paired read before it  (the pair above: this stops the scan)
        an overlapping in-place write by an arithmetic or logic instruction, and before it, with no
        flow control instruction between the two, an instruction that does not write rN and reads
        two or more of the components the write replaces into one component of its result, while
        they still hold the values that the write replaces.

The same with a load as the write (ld_raw r9.xyzw, r9.w, ...: the address is a component the load
replaces) is listed and not refused: a load does not compute its result from the register.

How a source operand feeds a destination component (D3D11.3 functional spec, instruction operands):
a source with a four component swizzle is read by position, destination component x from the first
letter and so on; a source with one component feeds every destination component; a register inside an
index (cb0[r1.x + 3]) feeds every destination component. For the instructions that do not work
component by component (loads), and for a source with two or three components, every destination
component is taken to read every component the operand names. An opcode that is not in the tables
below stops the scan, so that no instruction is skipped unread.

    scan.py --listings <dir of .asm> [--listings <dir> ...] [--allow <file>] [--out <file>]
    scan.py --self-test     the scan finds the pair above and leaves similar sequences alone

Exit code 1 when there is a pair that the allow-list does not have. The allow-list (allow.txt next to
this file) has one line per accepted write, "<shader>: <instruction as in the listing>", with the
reason in a comment; an entry that matches no pair is an error as well, so the list cannot go stale.
--out gets the pairs and, per shader, every overlapping in-place write.
"""
import argparse
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
COMPONENTS = 'xyzw'

# Flow control: ends a straight run of instructions.
FLOW = {
    'if_nz', 'if_z', 'else', 'endif', 'loop', 'endloop', 'break', 'breakc_nz', 'breakc_z', 'continue',
    'continuec_nz', 'continuec_z', 'ret', 'retc_nz', 'retc_z', 'switch', 'case', 'default', 'endswitch',
    'discard_nz', 'discard_z', 'label', 'call', 'callc_nz', 'callc_z',
}
# Instructions that write no register: flow control, stores (their first operand is a UAV or shared
# memory), atomic operations without a result.
NO_DESTINATION = FLOW | {
    'nop', 'store_raw', 'store_structured', 'store_uav_typed',
    'atomic_and', 'atomic_or', 'atomic_xor', 'atomic_cmp_store', 'atomic_iadd', 'atomic_imax', 'atomic_imin',
    'atomic_umax', 'atomic_umin',
}
# One destination, every destination component computed from the same position of its sources.
COMPONENTWISE = {
    'mov', 'movc', 'iadd', 'imad', 'ineg', 'ishl', 'ishr', 'ushr', 'and', 'or', 'xor', 'not', 'imax', 'imin',
    'umax', 'umin', 'ieq', 'ine', 'ige', 'ilt', 'uge', 'ult', 'bfi', 'ubfe', 'ibfe', 'bfrev', 'countbits',
    'firstbit_hi', 'firstbit_lo', 'firstbit_shi', 'umad',
    'add', 'mul', 'mad', 'div', 'min', 'max', 'eq', 'ne', 'ge', 'lt', 'sqrt', 'rsq', 'rcp', 'frc', 'exp', 'log',
    'round_ne', 'round_ni', 'round_pi', 'round_z', 'itof', 'utof', 'ftoi', 'ftou', 'f16tof32', 'f32tof16',
}
# Two destinations (either can be null), component by component.
TWO_DESTINATIONS = {'imul', 'umul', 'udiv', 'swapc', 'sincos', 'uaddc', 'usubb'}
# One destination, not component by component: every destination component reads all of every source.
WHOLE_SOURCES = {
    'ld_raw', 'ld_structured', 'ld', 'ld_uav_typed', 'ldms', 'dp2', 'dp3', 'dp4', 'resinfo', 'bufinfo',
    'sampleinfo', 'samplepos',
    'imm_atomic_alloc', 'imm_atomic_consume', 'imm_atomic_and', 'imm_atomic_or', 'imm_atomic_xor',
    'imm_atomic_exch', 'imm_atomic_cmp_exch', 'imm_atomic_iadd', 'imm_atomic_imax', 'imm_atomic_imin',
    'imm_atomic_umax', 'imm_atomic_umin',
}

REGISTER = re.compile(r'\br(\d+)(?:\.([xyzw]{1,4}))?')


def fail(message):
    raise SystemExit('scan.py: ' + message)


def split_operands(text):
    """The operands of an instruction: commas outside of parentheses and brackets separate them."""
    operands = []
    depth = 0
    current = ''
    for character in text:
        if character in '([':
            depth += 1
        elif character in ')]':
            depth -= 1
        if character == ',' and depth == 0:
            operands.append(current.strip())
            current = ''
        else:
            current += character
    if current.strip():
        operands.append(current.strip())
    return operands


def opcode_of(token):
    """ld_raw_indexable -> ld_raw; sync_g_t -> sync; add_sat -> add; declarations -> dcl."""
    name = token.lower()
    name = re.sub(r'_indexable$', '', name)
    if name.startswith('sync'):
        return 'sync'
    if name.startswith('dcl_') or re.match(r'^[a-z]s_\d_\d$', name):
        return 'dcl'
    return re.sub(r'_sat$', '', name)


def instructions(path):
    """[(line number, opcode, [operands], text)] of a listing, without comments, declarations, syncs and
    the rows of the immediate constant buffer."""
    with open(path, encoding='utf-8', errors='replace') as f:
        lines = f.read().splitlines()
    found = []
    in_table = False
    for number, line in enumerate(lines, 1):
        text = line.strip()
        if not text or text.startswith('//'):
            continue
        if in_table or text.startswith('dcl_immediateConstantBuffer'):
            # The table ends with the line that closes its outer brace.
            in_table = not re.search(r'\}\s*\}\s*$', text)
            continue
        # The opcode, its modifiers in parentheses (they can have blanks: (structured_buffer, stride=4)),
        # then the operands.
        match = re.match(r'^([A-Za-z0-9_]+)((?:\([^)]*\))*)\s*(.*)$', text)
        if not match:
            fail('%s line %d: not an instruction: %s' % (path, number, text))
        opcode = opcode_of(match.group(1))
        if opcode in ('dcl', 'sync'):
            continue
        found.append((number, opcode, split_operands(match.group(3)), text))
    return found


def destination(operand):
    """(register number, set of components) of a destination operand, or None (null, x0[...], u0, g0)."""
    if operand == 'null':
        return None
    match = re.match(r'^r(\d+)(?:\.([xyzw]{1,4}))?$', operand)
    if not match:
        if re.match(r'^(x\d+\[.*\]|o\w*|u\d+|g\d+)(\.[xyzw]{1,4})?$', operand):
            return None
        fail('destination operand not understood: %s' % operand)
    return int(match.group(1)), set(match.group(2) or COMPONENTS)


def source_reads(operand, componentwise):
    """What a source operand reads of the temp registers: [(register, {destination component: set of
    components read for it}, or None when every destination component reads `all`, all)]."""
    reads = []
    # Registers inside an index feed every destination component.
    inner = ''.join(re.findall(r'\[([^\]]*)\]', operand))
    for match in REGISTER.finditer(inner):
        reads.append((int(match.group(1)), None, set(match.group(2) or COMPONENTS)))
    outer = re.sub(r'\[[^\]]*\]', '[]', operand)
    for match in REGISTER.finditer(outer):
        swizzle = match.group(2) or COMPONENTS
        if componentwise and len(swizzle) == 4:
            reads.append((int(match.group(1)), {c: {swizzle[i]} for i, c in enumerate(COMPONENTS)}, set(swizzle)))
        else:
            reads.append((int(match.group(1)), None, set(swizzle)))
    return reads


def decode(opcode, operands, text):
    """(destinations [(register, components)], sources [source_reads]) of an instruction."""
    if opcode in NO_DESTINATION:
        count = 0
    elif opcode in TWO_DESTINATIONS:
        count = 2
    elif opcode in COMPONENTWISE or opcode in WHOLE_SOURCES:
        count = 1
    else:
        fail('opcode not in the tables of scan.py: %s (%s)' % (opcode, text))
    if len(operands) < count:
        fail('too few operands: %s' % text)
    destinations = [d for d in (destination(o) for o in operands[:count]) if d]
    componentwise = opcode in COMPONENTWISE or opcode in TWO_DESTINATIONS
    sources = []
    for operand in operands[count:]:
        sources += source_reads(operand, componentwise)
    # A destination like x0[r1.x + 0].x reads the registers of its index.
    for operand in operands[:count]:
        for match in REGISTER.finditer(''.join(re.findall(r'\[([^\]]*)\]', operand))):
            sources.append((int(match.group(1)), None, set(match.group(2) or COMPONENTS)))
    return destinations, sources


def overlap(destinations, sources):
    """The overlapping in-place writes of one instruction: [(register, written component, the other
    written component it reads)]."""
    written = {}
    for register, components in destinations:
        written.setdefault(register, set()).update(components)
    found = []
    for register, components in destinations:
        for source_register, by_component, everything in sources:
            if source_register != register:
                continue
            for c in sorted(components):
                read = by_component[c] if by_component else everything
                for d in sorted(read):
                    # d == c is the plain in-place update, unless another destination operand of the
                    # same instruction is what writes d (udiv, imul): then the two results overlap.
                    other = sum(1 for r, cs in destinations if r == register and d in cs) > 1
                    if d in written[register] and (d != c or other):
                        found.append((register, c, d))
    return sorted(set(found))


def lanes_of(destinations, sources):
    """{register: [set of its components that one result component reads]}: one set per component the
    instruction computes (one for an instruction without a register destination)."""
    computed = sorted(set().union(*[cs for _, cs in destinations])) if destinations else ['x']
    lanes = {}
    for register, by_component, everything in sources:
        sets = lanes.setdefault(register, [set() for _ in computed])
        for i, c in enumerate(computed):
            sets[i].update(by_component[c] if by_component else everything)
    return lanes


def scan_listing(path):
    """The overlapping in-place writes of a listing: [{line, text, pairs, arithmetic, readers}]; readers
    are the earlier instructions that make it a paired read: [(line, text, components)]."""
    hits = []
    count = 0
    run = 0        # number of the straight run of instructions
    readers = {}   # (register, component) -> the instructions that read its present value
    for number, opcode, operands, text in instructions(path):
        count += 1
        if opcode in FLOW:
            run += 1
        destinations, sources = decode(opcode, operands, text)
        pairs = overlap(destinations, sources)
        if pairs:
            register = pairs[0][0]
            written = set().union(*[cs for r, cs in destinations if r == register])
            earlier = {}
            for c in written:
                for reader in readers.get((register, c), []):
                    earlier.setdefault(reader['line'], (reader, set()))[1].add(c)
            paired = []
            for line in sorted(earlier):
                reader, live = earlier[line]
                if reader['run'] != run or register in reader['writes']:
                    continue
                # One result component of the reader takes two or more of the values the write replaces.
                both = [lane & live for lane in reader['lanes'][register] if len(lane & live) >= 2]
                if both:
                    paired.append((line, reader['text'], ''.join(sorted(set().union(*both)))))
            hits.append({'line': number, 'text': text, 'pairs': pairs, 'register': register,
                         'arithmetic': opcode in COMPONENTWISE or opcode in TWO_DESTINATIONS, 'readers': paired})
        # What this instruction writes has no readers yet; what it reads, it reads in its present value.
        for register, components in destinations:
            for c in components:
                readers[(register, c)] = []
        lanes = lanes_of(destinations, sources)
        record = {'line': number, 'text': text, 'run': run, 'lanes': lanes, 'writes': {r for r, _ in destinations}}
        for register, sets in lanes.items():
            for c in set().union(*sets):
                readers.setdefault((register, c), []).append(record)
    return hits, count


# --self-test: the confirmed pair, and sequences next to it that are not the pair.
SELF_TEST = [
    ('the pair of the 8-bit vertical loop filter', 1, """
        iadd r2.x, r3.w, -r3.y
        iadd r1.w, r1.w, r2.x
        iadd r2.x, r3.x, r1.w
        iadd r2.x, r2.x, l(4)
        iadd r3.yw, r3.wwww, -r4.xxxy
        iadd r1.w, r1.w, r3.y
     """),
    ('the read is of one of the two components only', 0, """
        iadd r2.x, r3.w, -r4.y
        iadd r3.yw, r3.wwww, -r4.xxxy
     """),
    ('flow control between the read and the write', 0, """
        iadd r2.x, r3.w, -r3.y
        if_nz r2.x
          iadd r3.yw, r3.wwww, -r4.xxxy
        endif
     """),
    ('the reader rewrites one of the components itself', 0, """
        imad r3.x, l(3), r3.y, r3.x
        iadd r3.xy, r3.xxxx, l(4, 3, 0, 0)
     """),
    ('an in-place update without overlap', 0, """
        iadd r2.x, r3.w, -r3.y
        iadd r3.yw, r3.yyyw, -r4.xxxy
     """),
    ('a load as the write is listed, not refused', 0, """
        iadd r4.z, r9.z, r9.x
        ld_raw_indexable(raw_buffer)(mixed,mixed,mixed,mixed) r9.xyzw, r9.w, u0.xyzw
     """),
]


def self_test():
    import tempfile
    failed = 0
    with tempfile.TemporaryDirectory() as directory:
        for title, expected, text in SELF_TEST:
            path = os.path.join(directory, 'case.asm')
            with open(path, 'w', encoding='utf-8') as f:
                f.write('cs_5_0\ndcl_temps 10\n' + text)
            hits, _ = scan_listing(path)
            found = sum(1 for hit in hits if hit['readers'] and hit['arithmetic'])
            ok = found == expected
            failed += 0 if ok else 1
            print('scan.py: self-test, %s: %d paired read(s), %d expected: %s' %
                  (title, found, expected, 'ok' if ok else 'FAILED'))
    return 1 if failed else 0


def read_allow(path):
    """{(shader, instruction text): [line of the list, used]}"""
    allowed = {}
    if not path or not os.path.isfile(path):
        return allowed
    with open(path, encoding='utf-8') as f:
        for number, line in enumerate(f, 1):
            line = line.split('#', 1)[0].strip()
            if not line:
                continue
            match = re.match(r'^(\S+):\s*(.+)$', line)
            if not match:
                fail('%s line %d: expected "<shader>: <instruction>"' % (path, number))
            allowed[(match.group(1), re.sub(r'\s+', ' ', match.group(2)))] = [number, False]
    return allowed


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--listings', action='append',
                        help='a directory with the listings, <shader>.asm; may be given more than once')
    parser.add_argument('--self-test', action='store_true', help='check the scan on the known pair')
    parser.add_argument('--allow', default=os.path.join(HERE, 'allow.txt'), help='the allow-list')
    parser.add_argument('--out', help='file for the full report: the pairs and every overlapping in-place write')
    parser.add_argument('--min-listings', type=int, default=1, help='fail when a directory has fewer listings')
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not args.listings:
        parser.error('--listings or --self-test')

    allowed = read_allow(args.allow)
    short = []    # the standard output
    full = []     # --out
    new = 0
    accepted = 0
    total = {'instructions': 0, 'writes': 0, 'loads': 0}
    for directory in args.listings:
        names = sorted(name[:-4] for name in os.listdir(directory) if name.endswith('.asm'))
        if len(names) < args.min_listings:
            fail('%s: %d listings, at least %d expected' % (directory, len(names), args.min_listings))
        writes_here = 0
        shaders_with = 0
        pairs_here = 0
        loads_here = 0
        detail = []
        for name in names:
            hits, count = scan_listing(os.path.join(directory, name + '.asm'))
            total['instructions'] += count
            writes_here += len(hits)
            shaders_with += 1 if hits else 0
            for hit in hits:
                components = ', '.join('r%d.%s from r%d.%s' % (r, c, r, d) for r, c, d in hit['pairs'])
                detail.append('%s.asm:%d: %s    (%s)' % (name, hit['line'], hit['text'], components))
                if not hit['readers']:
                    continue
                reads = ['    line %d: %s    (r%d.%s into one result)' % (line, text, hit['register'], '+'.join(comps))
                         for line, text, comps in hit['readers']]
                if not hit['arithmetic']:
                    loads_here += 1
                    detail += ['    a load, with a paired read before it (listed, not refused):'] + reads
                    continue
                pairs_here += 1
                entry = allowed.get((name, re.sub(r'\s+', ' ', hit['text'])))
                if entry:
                    entry[1] = True
                    accepted += 1
                else:
                    new += 1
                block = ['%sPAIRED READ %s.asm:%d: %s' % ('allowed: ' if entry else '', name, hit['line'], hit['text']),
                         '    %s; before it, in the same run of instructions:' % components] + reads
                short += block
                detail += ['    ' + line for line in block]
        total['writes'] += writes_here
        total['loads'] += loads_here
        summary = ('scan.py: %s: %d listings; %d overlapping in-place writes in %d of them, %d with a paired read '
                   'before them (%d more are loads)' % (directory, len(names), writes_here, shaders_with, pairs_here,
                                                        loads_here))
        short.append(summary)
        full += ['== %s' % directory] + detail + [summary, '']
    stale = sorted((entry[0], key) for key, entry in allowed.items() if not entry[1])
    for number, (name, text) in stale:
        short.append('scan.py: allow-list line %d matches no paired read: %s: %s' % (number, name, text))
    short.append('scan.py: %d instructions read; paired reads before an overlapping in-place write: %d not on the '
                 'allow-list, %d on it' % (total['instructions'], new, accepted))
    sys.stdout.write('\n'.join(short) + '\n')
    if args.out:
        os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
        with open(args.out, 'w', encoding='utf-8') as f:
            f.write('\n'.join(full + short) + '\n')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n- listings scan (testing/dxbc_scan/scan.py): %d instructions, %d overlapping in-place writes, '
                    'paired reads before one: %d not on the allow-list, %d on it\n' %
                    (total['instructions'], total['writes'], new, accepted))
    return 1 if new or stale else 0


if __name__ == '__main__':
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, 'reconfigure'):
            stream.reconfigure(errors='replace')
    sys.exit(main())
