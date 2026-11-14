"""A variant of a test player package that differs from it in one restricted capability and nothing else.

For the question of how much memory the system gives the player with a capability and into which of the
console's pools the decoder's buffers go (docs/test-player.md, "The two memory pools"): the package with
the capability must be the default package in every byte of every program, so that the capability is the
only difference between two runs. The player job of CI unpacks the default package, has this script write
the capability and another revision into the manifest, packs and signs the result (MakeAppx, SignTool) and
has this script hold the two packages against each other.

usage: python capability_variant.py manifest --revision <n> --restricted <capability> <AppxManifest.xml>
       python capability_variant.py check --revision <n> --restricted <capability>
                                          --base <package or directory> --variant <package or directory>
                                          [--note <file>]
       python capability_variant.py self-test

manifest: rewrites the manifest of an unpacked package in place: the fourth part of the version becomes
<n> (it must be 0), the namespace of the restricted capabilities is declared and ignorable, and
<rescap:Capability Name="<capability>"/> is added behind the capabilities that are there (in front of
device and custom capabilities, which the schema wants last). Text is edited, not a parsed tree, so that
the prefixes the manifest has stay as they are: IgnorableNamespaces names them.

check: exit code 0 when
  - the base package does not declare the capability and has revision 0;
  - the variant declares it as a restricted capability, has revision <n> and is signed;
  - identity name, publisher and everything else in the two manifests are the same;
  - every file of the packages other than the manifest and the files the packaging writes (block map,
    signature, content types, code integrity catalog) is in both, byte for byte.
A directory is searched for its one application package (not under Dependencies).
"""

import argparse
import hashlib
import io
import os
import re
import sys
import tempfile
import xml.etree.ElementTree as ET
import zipfile

FOUNDATION = 'http://schemas.microsoft.com/appx/manifest/foundation/windows10'
RESCAP = 'http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities'
# What MakeAppx and SignTool write: these differ between any two packages.
FOOTPRINT = ('appxblockmap.xml', 'appxsignature.p7x', '[content_types].xml', 'appxmetadata/codeintegrity.cat')
MANIFEST = 'appxmanifest.xml'


class Problem(Exception):
    pass


def edit_manifest(text, revision, capability):
    """The manifest text with the revision and the restricted capability."""
    package = re.search(r'<Package\b[^>]*>', text)
    if not package:
        raise Problem('no <Package> element')
    tag = package.group(0)
    prefix = None
    for name, value in re.findall(r'\bxmlns:([A-Za-z0-9_]+)\s*=\s*"([^"]*)"', tag):
        if value == RESCAP:
            prefix = name
    new_tag = tag
    if prefix is None:
        prefix = 'rescap'
        if re.search(r'\bxmlns:rescap\s*=', tag):
            raise Problem('the prefix rescap is bound to another namespace')
        new_tag = re.sub(r'\s*/?>$', lambda m: ' xmlns:rescap="%s"%s' % (RESCAP, m.group(0).lstrip()), new_tag)
    ignorable = re.search(r'\bIgnorableNamespaces\s*=\s*"([^"]*)"', new_tag)
    if ignorable is None:
        new_tag = re.sub(r'>$', ' IgnorableNamespaces="%s">' % prefix, new_tag)
    elif prefix not in ignorable.group(1).split():
        new_tag = new_tag.replace(ignorable.group(0),
                                  'IgnorableNamespaces="%s"' % ' '.join(ignorable.group(1).split() + [prefix]))
    text = text.replace(tag, new_tag, 1)

    identity = re.search(r'<Identity\b[^>]*>', text)
    if not identity:
        raise Problem('no <Identity> element')
    version = re.search(r'\bVersion\s*=\s*"(\d+)\.(\d+)\.(\d+)\.(\d+)"', identity.group(0))
    if not version:
        raise Problem('the identity has no version of four parts')
    if version.group(4) != '0':
        raise Problem('the revision of the package is %s, expected 0 (the default package)' % version.group(4))
    new_identity = identity.group(0).replace(
        version.group(0), 'Version="%s.%s.%s.%d"' % (version.group(1), version.group(2), version.group(3), revision))
    text = text.replace(identity.group(0), new_identity, 1)

    capabilities = re.search(r'<Capabilities\s*>(.*?)</Capabilities\s*>', text, re.S)
    if not capabilities:
        raise Problem('no <Capabilities> element with content')
    body = capabilities.group(1)
    if re.search(r'\bName\s*=\s*"%s"' % re.escape(capability), body):
        raise Problem('the manifest declares %s already' % capability)
    newline = '\r\n' if '\r\n' in text else '\n'
    lines = re.findall(r'^([ \t]*)<[A-Za-z0-9_:]*Capability\b', body, re.M)
    indent = lines[0] if lines else '    '
    element = '%s<%s:Capability Name="%s" />%s' % (indent, prefix, capability, newline)
    # Device and custom capabilities are last in the schema; everything else may be in front of ours.
    later = re.search(r'^[ \t]*<(?:[A-Za-z0-9_]+:)?(?:DeviceCapability|CustomCapability)\b', body, re.M)
    if later:
        at = later.start()
    else:
        closing = re.search(r'[ \t]*$', body)
        at = closing.start()
        if at and body[at - 1] not in '\r\n':
            element = newline + element
    new_body = body[:at] + element + body[at:]
    return text[:capabilities.start(1)] + new_body + text[capabilities.end(1):]


def command_manifest(args):
    with open(args.manifest, 'rb') as f:
        raw = f.read()
    bom = raw.startswith(b'\xef\xbb\xbf')
    text = raw[3:].decode('utf-8') if bom else raw.decode('utf-8')
    text = edit_manifest(text, args.revision, args.restricted)
    ET.fromstring(text)  # well-formed, or an exception
    with open(args.manifest, 'wb') as f:
        f.write((b'\xef\xbb\xbf' if bom else b'') + text.encode('utf-8'))
    print('capability_variant.py: %s has revision %d and the restricted capability %s'
          % (args.manifest, args.revision, args.restricted))
    return 0


def find_package(path):
    if os.path.isfile(path):
        return path
    found = []
    for directory, _, files in os.walk(path):
        if os.sep + 'dependencies' + os.sep in (directory + os.sep).lower():
            continue
        found += [os.path.join(directory, name) for name in files if name.lower().endswith(('.msix', '.appx'))]
    if len(found) != 1:
        raise Problem('%d application packages under %s, expected one' % (len(found), path))
    return found[0]


def read_package(path):
    """(manifest bytes, {payload name: sha256}, lower-case names of all files)."""
    with zipfile.ZipFile(path) as package:
        names = [name for name in package.namelist() if not name.endswith('/')]  # no directory entries
        manifest = None
        payload = {}
        for name in names:
            lower = name.lower()
            if lower == MANIFEST:
                manifest = package.read(name)
            elif lower not in FOOTPRINT:
                payload[name] = hashlib.sha256(package.read(name)).hexdigest()
    if manifest is None:
        raise Problem('%s has no AppxManifest.xml' % path)
    return manifest, payload, [name.lower() for name in names]


def capabilities_of(root):
    found = []
    for parent in root.iter('{%s}Capabilities' % FOUNDATION):
        for child in parent:
            found.append((child.tag, child.get('Name')))
    return found


def tree_differences(a, b, where, out):
    """What differs between two elements, their version and ignorable namespaces aside."""
    if a.tag != b.tag:
        out.append('%s: element %s against %s' % (where, a.tag, b.tag))
        return
    here = '%s/%s' % (where, a.tag.split('}')[-1])
    attributes_a, attributes_b = dict(a.attrib), dict(b.attrib)
    for attributes in (attributes_a, attributes_b):
        attributes.pop('IgnorableNamespaces', None)
        if a.tag == '{%s}Identity' % FOUNDATION:
            attributes.pop('Version', None)
    if attributes_a != attributes_b:
        out.append('%s: attributes %s against %s' % (here, sorted(attributes_a.items()), sorted(attributes_b.items())))
    if (a.text or '').strip() != (b.text or '').strip():
        out.append('%s: text %r against %r' % (here, (a.text or '').strip(), (b.text or '').strip()))
    children_a, children_b = list(a), list(b)
    if len(children_a) != len(children_b):
        out.append('%s: %d children against %d' % (here, len(children_a), len(children_b)))
        return
    for child_a, child_b in zip(children_a, children_b):
        tree_differences(child_a, child_b, here, out)


def compare(base_path, variant_path, revision, capability):
    """The problems of the pair, and lines that describe it."""
    problems = []
    base_manifest, base_payload, base_names = read_package(base_path)
    variant_manifest, variant_payload, variant_names = read_package(variant_path)
    base_root = ET.fromstring(base_manifest)
    variant_root = ET.fromstring(variant_manifest)
    ours = ('{%s}Capability' % RESCAP, capability)

    def identity(root):
        element = root.find('{%s}Identity' % FOUNDATION)
        if element is None:
            raise Problem('a manifest has no identity')
        return element

    base_version = identity(base_root).get('Version', '')
    variant_version = identity(variant_root).get('Version', '')
    if base_version.split('.')[3:] != ['0']:
        problems.append('the base package has version %s, expected revision 0' % base_version)
    if variant_version.split('.')[:3] != base_version.split('.')[:3] or \
            variant_version.split('.')[3:] != [str(revision)]:
        problems.append('the variant has version %s, expected %s with revision %d'
                        % (variant_version, base_version, revision))
    base_capabilities = capabilities_of(base_root)
    variant_capabilities = capabilities_of(variant_root)
    if any(name == capability for _, name in base_capabilities):
        problems.append('the base package declares %s: it must not' % capability)
    if variant_capabilities.count(ours) != 1:
        problems.append('the variant does not declare the restricted capability %s once: %s'
                        % (capability, variant_capabilities))
    else:
        # Without the capability the variant's manifest is the base's.
        for parent in variant_root.iter('{%s}Capabilities' % FOUNDATION):
            for child in list(parent):
                if (child.tag, child.get('Name')) == ours:
                    parent.remove(child)
        differences = []
        tree_differences(base_root, variant_root, '', differences)
        problems += ['the manifests differ in more than the capability and the version: %s' % d for d in differences]
    for name in sorted(set(base_payload) | set(variant_payload)):
        if name not in variant_payload:
            problems.append('%s is in the base package only' % name)
        elif name not in base_payload:
            problems.append('%s is in the variant only' % name)
        elif base_payload[name] != variant_payload[name]:
            problems.append('%s differs between the packages' % name)
    for name in ('appxsignature.p7x', 'appxblockmap.xml'):
        if name not in variant_names:
            problems.append('the variant has no %s%s' % (name, ' (it is not signed)' if 'signature' in name else ''))
    lines = [
        'base:    %s, version %s, capabilities %s' % (os.path.basename(base_path), base_version,
                                                     ', '.join(name for _, name in base_capabilities)),
        'variant: %s, version %s, capabilities %s' % (os.path.basename(variant_path), variant_version,
                                                     ', '.join(name for _, name in variant_capabilities)),
        '%d files of the package are the same in both, byte for byte (sha256):' % len(
            [n for n in base_payload if variant_payload.get(n) == base_payload[n]]),
    ] + ['  %s  %s' % (base_payload[name], name) for name in sorted(base_payload)
         if variant_payload.get(name) == base_payload[name]]
    return problems, lines


def command_check(args):
    base = find_package(args.base)
    variant = find_package(args.variant)
    problems, lines = compare(base, variant, args.revision, args.restricted)
    lines += ['PROBLEM: %s' % problem for problem in problems]
    lines.append('capability_variant.py: %s' % (
        'the variant is the base package with the restricted capability %s and revision %d, and nothing else'
        % (args.restricted, args.revision) if not problems else '%d problems' % len(problems)))
    print('\n'.join(lines))
    if args.note:
        with open(args.note, 'w', encoding='utf-8') as f:
            f.write('\n'.join(lines) + '\n')
    return 1 if problems else 0


SELF_TEST_MANIFEST = '''﻿<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10" xmlns:mp="http://schemas.microsoft.com/appx/2014/phone/manifest" xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10" IgnorableNamespaces="uap mp build" xmlns:build="http://schemas.microsoft.com/developer/appx/2015/build">
  <Identity Name="Player" Publisher="CN=Somebody" Version="0.2.7.0" ProcessorArchitecture="x64" />
  <Properties>
    <DisplayName>Player</DisplayName>
  </Properties>
  <Capabilities>
    <Capability Name="internetClient" />
  </Capabilities>
  <build:Metadata>
    <build:Item Name="cl.exe" Version="19.0" />
  </build:Metadata>
</Package>
'''.replace('\n', '\r\n')


def self_test():
    failures = []

    def expect(what, condition):
        if not condition:
            failures.append(what)

    source = SELF_TEST_MANIFEST[1:]
    edited = edit_manifest(source, 3, 'hevcPlayback')
    expect('revision', 'Version="0.2.7.3"' in edited and 'Version="0.2.7.0"' not in edited)
    expect('namespace', ('xmlns:rescap="%s"' % RESCAP) in edited)
    expect('ignorable', 'IgnorableNamespaces="uap mp build rescap"' in edited)
    expect('capability, behind the one that was there, with the manifest\'s line ends',
           '    <Capability Name="internetClient" />\r\n    <rescap:Capability Name="hevcPlayback" />\r\n  </Capabilities>'
           in edited)
    expect('nothing else changed', edited.count('\n') == source.count('\n') + 1 and '\n' not in edited.replace('\r\n', ''))
    root = ET.fromstring(edited)
    expect('parsed capability', capabilities_of(root) == [('{%s}Capability' % FOUNDATION, 'internetClient'),
                                                         ('{%s}Capability' % RESCAP, 'hevcPlayback')])
    with_device = source.replace('  </Capabilities>', '    <DeviceCapability Name="microphone" />\r\n  </Capabilities>')
    edited_device = edit_manifest(with_device, 3, 'hevcPlayback')
    expect('in front of a device capability',
           edited_device.index('rescap:Capability') < edited_device.index('DeviceCapability'))
    one_line = source.replace('\r\n    <Capability Name="internetClient" />\r\n  </Capabilities>',
                              '<Capability Name="internetClient" /></Capabilities>')
    expect('capabilities on one line', capabilities_of(ET.fromstring(edit_manifest(one_line, 3, 'hevcPlayback')))[-1]
           == ('{%s}Capability' % RESCAP, 'hevcPlayback'))
    for what, text in (('twice', edited), ('not the default package', source.replace('0.2.7.0', '0.2.7.2')),
                       ('no capabilities', source.replace('Capabilities', 'Other'))):
        try:
            edit_manifest(text, 3, 'hevcPlayback')
            expect('refused: %s' % what, False)
        except Problem:
            pass

    def package(path, manifest, files, signed=True):
        with zipfile.ZipFile(path, 'w') as z:
            for name, data in files.items():
                z.writestr(name, data)
            z.writestr('AppxManifest.xml', manifest.encode('utf-8'))
            z.writestr('AppxBlockMap.xml', os.path.basename(path))
            z.writestr('[Content_Types].xml', 'types')
            z.writestr('AppxMetadata/CodeIntegrity.cat', os.path.basename(path))
            if signed:
                z.writestr('AppxSignature.p7x', os.path.basename(path))

    files = {'player.exe': b'MZ player', 'rav1g.dll': b'MZ library', 'Assets/Logo.png': b'png'}
    with tempfile.TemporaryDirectory() as directory:
        base = os.path.join(directory, 'base.msix')
        variant = os.path.join(directory, 'variant.msix')
        package(base, SELF_TEST_MANIFEST, files)
        package(variant, '﻿' + edited, files)
        problems, _ = compare(base, variant, 3, 'hevcPlayback')
        expect('a good pair has no problems: %s' % problems, not problems)
        cases = [
            ('another program', lambda: package(variant, '﻿' + edited, dict(files, **{'player.exe': b'MZ other'}))),
            ('a file more', lambda: package(variant, '﻿' + edited, dict(files, extra=b'x'))),
            ('not signed', lambda: package(variant, '﻿' + edited, files, signed=False)),
            ('wrong revision', lambda: package(variant, '﻿' + edit_manifest(source, 2, 'hevcPlayback'), files)),
            ('no capability', lambda: package(variant, '﻿' + source.replace('0.2.7.0', '0.2.7.3'), files)),
            ('another capability too', lambda: package(variant, '﻿' + edited.replace(
                '    <rescap:Capability', '    <Capability Name="privateNetworkClientServer" />\r\n    <rescap:Capability'),
                files)),
            ('another display name', lambda: package(variant, '﻿' + edited.replace('>Player<', '>Other<'), files)),
            ('not a restricted capability', lambda: package(variant, '﻿' + edited.replace(
                '<rescap:Capability Name="hevcPlayback"', '<Capability Name="hevcPlayback"'), files)),
        ]
        for what, make in cases:
            make()
            problems, _ = compare(base, variant, 3, 'hevcPlayback')
            expect('found: %s' % what, bool(problems))
        package(variant, '﻿' + edited, files)
        package(base, '﻿' + edited.replace('0.2.7.3', '0.2.7.0'), files)
        problems, _ = compare(base, variant, 3, 'hevcPlayback')
        expect('found: the base package has the capability', any('must not' in p for p in problems))
    for failure in failures:
        print('capability_variant.py self-test FAILED: %s' % failure)
    if not failures:
        print('capability_variant.py self-test: the manifest edit and the comparison of two packages work')
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    commands = parser.add_subparsers(dest='command', required=True)
    manifest = commands.add_parser('manifest')
    manifest.add_argument('--revision', type=int, required=True)
    manifest.add_argument('--restricted', required=True, help='the restricted capability to add')
    manifest.add_argument('manifest')
    check = commands.add_parser('check')
    check.add_argument('--revision', type=int, required=True)
    check.add_argument('--restricted', required=True)
    check.add_argument('--base', required=True)
    check.add_argument('--variant', required=True)
    check.add_argument('--note', help='a file for what the check printed')
    commands.add_parser('self-test')
    args = parser.parse_args()
    try:
        if args.command == 'manifest':
            return command_manifest(args)
        if args.command == 'check':
            return command_check(args)
        return self_test()
    except Problem as problem:
        print('capability_variant.py: %s' % problem)
        return 2


if __name__ == '__main__':
    sys.exit(main())
