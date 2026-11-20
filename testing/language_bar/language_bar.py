#!/usr/bin/env python3
"""What rav1g.dll is built from, by language: languages.json and languages.svg for the README.

GitHub's own language bar goes by the paths of the repository. This one goes by a build: it is run
after the release build of rav1g.dll and takes the source files from what that build recorded.

    language_bar.py --release-dir <target dir>/x86_64-pc-windows-msvc/release --out <dir>

  Rust      the dependency files rustc wrote (<crate>-<hash>.d) of the library's crate and of every
            crate of this repository it depends on (cargo metadata; the crates of crates.io are not
            counted, nor are build scripts).
  Assembly  the files rav1d's build script named to cargo (cargo:rerun-if-changed in the output
            cargo keeps of it) and what they %include.
  HLSL      for every bytecode file the Rust embeds (include_bytes! of dx/shaders/dxil/<shader>.dxil
            of RAV1G_SHADER_DIR, which rustc names in the dependency file of rav1g-sys): the shader's
            source and its includes, as testing/shader_cache/shaders.py takes them for its cache
            keys. The key is computed again and held against the one in the report of the shader
            build, so the files counted are the files that were compiled.
  C         what a build script of BUILD_SCRIPTS compiles as C, and what the files #include. The
            library has had none since its shader table became Rust (2026-11-19), and a language
            without files is not in the bar.

A file the build wrote itself (the tables build.rs of rav1g writes, the shader table build.rs of
rav1g-sys writes, config.asm) is listed in the JSON and not counted, and neither is the bytecode
(the HLSL behind it is). Bytes decide the percentages, as on GitHub.

cargo has two layouts of a build directory, and both are read: deps/<crate>-<hash>.d with
build/<package>-<hash>/output and out, and build/<package>/<hash>/out/<crate>-<hash>.d with
build/<package>/<hash>/run/stdout and out.

Any of these is an error: a crate or build script without its record, a recorded file that is not
there, a file of a kind the script does not know, no source files at all, no Rust, no HLSL or no
Assembly (the languages the library cannot be without).
"""
import argparse
import datetime
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, '..', '..'))

# GitHub's colours of the languages (github-linguist, languages.yml).
COLOURS = {'Rust': '#dea584', 'HLSL': '#aace60', 'Assembly': '#6E4C13', 'C': '#555555'}
# The languages a build of the library has to have: without one of them the records were misread.
REQUIRED = ['Rust', 'HLSL', 'Assembly']

# What rustc may name in a dependency file of a crate of this repository.
RUST_EXTENSIONS = {'.rs': 'Rust'}

# The build scripts of the crates in the library, by package: the extensions of the files they name
# to cargo (rerun-if-changed) that they compile into the library, and the language. A build script
# that is not here is an error: someone has to say what it compiles.
BUILD_SCRIPTS = {
    # nasm: the entropy decoder and the motion vector lists of the parse pass.
    'rav1d': {'.asm': 'Assembly'},
    # Compiles nothing: it writes the shader table as Rust (a file of the build, not counted) with an
    # include_bytes! per shader, which the dependency file of the crate names.
    'rav1g-sys': {},
    # Compiles nothing: it reads tables of libaom's C files as text and writes them as Rust, which
    # is in the dependency file of the crate as a file of the build (not counted).
    'rav1g': {},
}

# Where the files of a build script look for their includes, next to the directory of the including
# file and the package: nasm is given -Isrc/ (third_party/rav1d/build.rs).
INCLUDE_DIRECTORIES = {'rav1d': ['src']}

INCLUDE = {
    'Assembly': re.compile(rb'^[ \t]*%[ \t]*include[ \t]+"([^"]+)"', re.M | re.I),
    'C': re.compile(rb'^[ \t]*#[ \t]*include[ \t]*"([^"]+)"', re.M),
}
# The bytecode of a shader in RAV1G_SHADER_DIR, of the default set or of the set od of the shader pick.
SHADER_BYTECODE = re.compile(r'(?:^|[\\/])dx[\\/]shaders[\\/](dxil|dxil_od)[\\/]([A-Za-z0-9_]+)\.dxil$')


def fail(message):
    raise SystemExit('language_bar.py: ' + message)


def canonical(path):
    return os.path.normcase(os.path.realpath(path))


def inside(path, directory):
    path, directory = canonical(path), canonical(directory)
    return path == directory or path.startswith(directory.rstrip(os.sep) + os.sep)


def newest(release_dir, patterns, what):
    """The one file of a build that matches a pattern (the parts of a path under the release
    directory, each a regular expression); the newest when the directory has older builds too."""
    found = []
    for pattern in patterns:
        paths = [release_dir]
        for part in pattern:
            paths = [os.path.join(path, name) for path in paths if os.path.isdir(path)
                     for name in sorted(os.listdir(path)) if re.fullmatch(part, name)]
        found += [path for path in paths if os.path.isfile(path)]
    if not found:
        fail('%s: %s has none of %s' % (what, release_dir, ', '.join('/'.join(pattern) for pattern in patterns)))
    return max(found, key=os.path.getmtime)


HASH = '[0-9a-f]{16}'


class Sources:
    """The files counted, {path relative to the repository: language}, and the files of the build."""

    def __init__(self, release_dir):
        self.release_dir = release_dir
        self.files = {}
        self.generated = set()

    def add(self, path, language, why):
        """True: a file of the repository, counted. False: a file the build wrote."""
        if not os.path.isfile(path):
            fail('%s: %s is not there' % (why, path))
        if inside(path, self.release_dir):
            self.generated.add(os.path.relpath(canonical(path), canonical(self.release_dir)).replace(os.sep, '/'))
            return False
        if not inside(path, REPO):
            fail('%s: %s is neither in the repository nor in the build directory' % (why, path))
        relative = os.path.relpath(canonical(path), canonical(REPO)).replace(os.sep, '/')
        if self.files.setdefault(relative, language) != language:
            fail('%s is %s and %s' % (relative, self.files[relative], language))
        return True


def library_crates(package, target):
    """The packages of this repository that are in the library: the package and what it depends on,
    not through build or dev dependencies. [(name, crate name, directory, has a build script)]."""
    command = ['cargo', 'metadata', '--format-version', '1', '--locked', '--manifest-path',
               os.path.join(REPO, 'Cargo.toml'), '--filter-platform', target]
    try:
        done = subprocess.run(command, stdout=subprocess.PIPE, check=True)
    except (OSError, subprocess.CalledProcessError) as error:
        fail('cargo metadata failed: %s' % error)
    metadata = json.loads(done.stdout)
    packages = {entry['id']: entry for entry in metadata['packages']}
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    roots = [entry['id'] for entry in metadata['packages'] if entry['name'] == package and entry['source'] is None]
    if len(roots) != 1:
        fail('the workspace has %d packages named %s' % (len(roots), package))
    seen = []
    todo = roots
    while todo:
        identity = todo.pop()
        if identity in seen:
            continue
        seen.append(identity)
        for dependency in nodes[identity]['deps']:
            if any(kind['kind'] is None for kind in dependency['dep_kinds']):
                todo.append(dependency['pkg'])
    crates = []
    for identity in seen:
        entry = packages[identity]
        if entry['source'] is not None:
            continue  # a crate of crates.io
        libraries = [t for t in entry['targets'] if set(t['kind']) & {'lib', 'rlib', 'staticlib', 'cdylib'}]
        if len(libraries) != 1:
            fail('%s: %d library targets' % (entry['name'], len(libraries)))
        crates.append((entry['name'], libraries[0]['name'].replace('-', '_'), os.path.dirname(entry['manifest_path']),
                       any('custom-build' in t['kind'] for t in entry['targets'])))
    return sorted(crates), metadata['workspace_root']


def dependency_file(path):
    """The files of a dependency file of rustc: what stands behind 'target:', a space in a name
    written as backslash space."""
    with open(path, encoding='utf-8') as f:
        text = f.read()
    files = []
    for line in text.splitlines():
        if line.startswith('#') or ': ' not in line:
            continue
        for name in re.findall(r'(?:\\ |[^ ])+', line.split(': ', 1)[1]):
            name = name.replace('\\ ', ' ')
            if name not in files:
                files.append(name)
    return files


def rust_sources(sources, crates, workspace_root):
    """The Rust of the crates. The bytecode files the crates embed (include_bytes!), which their
    dependency files name as well: [(path, directory of the set, shader)]."""
    embedded = []
    for name, crate, directory, _ in crates:
        files = ['%s-%s\\.d' % (re.escape(crate), HASH), re.escape(crate) + '\\.d']
        record = newest(sources.release_dir, [['deps', file] for file in files] +
                        [['build', re.escape(name), HASH, 'out', file] for file in files],
                        'the dependency file of the crate %s' % name)
        names = dependency_file(record)
        counted = 0
        for listed in names:
            # rustc runs in the directory of the workspace and names the files of a crate from there.
            path = listed if os.path.isabs(listed) else os.path.join(workspace_root, listed)
            extension = os.path.splitext(path)[1].lower()
            bytecode = SHADER_BYTECODE.search(path)
            if bytecode:
                embedded.append((path, bytecode.group(1), bytecode.group(2)))
                continue
            if extension not in RUST_EXTENSIONS:
                fail('%s names %s: what language is %s?' % (record, listed, extension or 'a file without extension'))
            counted += sources.add(path, RUST_EXTENSIONS[extension], record)
        if not counted:
            fail('%s names no file of the repository' % record)
        print('language_bar.py: %s: %d files of the repository in %s' % (name, counted, os.path.basename(record)))
    return embedded


def build_script_inputs(sources, name, directory):
    """What the build script of a package named to cargo: (files, the directory it wrote to)."""
    output = newest(sources.release_dir, [['build', '%s-%s' % (re.escape(name), HASH), 'output'],
                                          ['build', re.escape(name), HASH, 'run', 'stdout']],
                    'the output of the build script of %s' % name)
    # The directory the script wrote to (OUT_DIR): next to the output, or next to its directory.
    out_dir = os.path.join(os.path.dirname(output), 'out')
    if os.path.basename(output) == 'stdout':
        out_dir = os.path.join(os.path.dirname(os.path.dirname(output)), 'out')
    with open(output, encoding='utf-8') as f:
        lines = f.read().splitlines()
    named = [match.group(1) for match in (re.match(r'cargo::?rerun-if-changed=(.+)$', line) for line in lines) if match]
    # Relative to the package, which is where a build script runs.
    return [os.path.normpath(os.path.join(directory, path)) for path in named], out_dir, output


def compiled_sources(sources, crates):
    """The files the build scripts compile, with their includes. The directory of the shader build
    (RAV1G_SHADER_DIR), which the build script of rav1g-sys names by its shader_set.txt."""
    shader_root = None
    for name, _, directory, has_script in crates:
        if not has_script:
            continue
        if name not in BUILD_SCRIPTS:
            fail('%s has a build script that BUILD_SCRIPTS of this script does not know: say what it compiles' % name)
        named, out_dir, output = build_script_inputs(sources, name, directory)
        if name == 'rav1g-sys':
            roots = [os.path.dirname(path) for path in named if os.path.basename(path) == 'shader_set.txt']
            if len(roots) != 1:
                fail('%s names %d shader_set.txt' % (output, len(roots)))
            shader_root = roots[0]
        search = [directory] + [os.path.join(directory, extra) for extra in INCLUDE_DIRECTORIES.get(name, [])]
        search += [out_dir]
        counted = 0
        todo = [(path, BUILD_SCRIPTS[name][os.path.splitext(path)[1].lower()]) for path in named
                if os.path.splitext(path)[1].lower() in BUILD_SCRIPTS[name]]
        if BUILD_SCRIPTS[name] and not todo:
            fail('%s names no %s file' % (output, ' or '.join(sorted(BUILD_SCRIPTS[name]))))
        seen = set()
        while todo:
            path, language = todo.pop()
            if canonical(path) in seen:
                continue
            seen.add(canonical(path))
            if not sources.add(path, language, output):
                continue  # written by the build: nothing of the repository behind it but the shaders
            counted += 1
            with open(path, 'rb') as f:
                data = f.read()
            for match in INCLUDE[language].finditer(data):
                include = match.group(1).decode('utf-8')
                found = [os.path.join(where, include) for where in [os.path.dirname(path)] + search
                         if os.path.isfile(os.path.join(where, include))]
                if found:
                    todo.append((found[0], language))
                elif conditional(data, match.start()):
                    print('language_bar.py: %s: "%s" is not there and is behind a condition: not in this build' % (
                        os.path.basename(path), include))
                else:
                    fail('%s includes "%s", which is in none of %s' % (path, include, ', '.join(search)))
        print('language_bar.py: %s: %d files compiled by its build script' % (name, counted))
    return shader_root


def conditional(data, position):
    """An #include at this place of a C file is inside an #if."""
    depth = 0
    for line in data[:position].splitlines():
        word = re.match(rb'[ \t]*#[ \t]*(if|ifdef|ifndef|endif)\b', line)
        if word:
            depth += -1 if word.group(1) == b'endif' else 1
    return depth > 0


def shader_sources(sources, shader_root, embedded, report_path):
    """The sources of the shaders whose bytecode the library has."""
    if not shader_root or not embedded:
        fail('the Rust of the build embeds no bytecode (include_bytes! of dx/shaders/dxil/<shader>.dxil)')
    spec = importlib.util.spec_from_file_location('shaders', os.path.join(REPO, 'testing', 'shader_cache', 'shaders.py'))
    shaders = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(shaders)
    report_path = report_path or os.path.join(shader_root, 'report.json')
    try:
        with open(report_path, encoding='utf-8') as f:
            report = json.load(f)
    except OSError as error:
        fail('the report of the shader build: %s' % error)
    # The sets of the build by their directory: the default one, and the set od of a shader-pick build.
    built = {shaders.BYTECODE: (report['shaders'], lambda shader: shader['od'])}
    for other in report.get('sets', {}).values():
        built[other['directory']] = (other['shaders'], lambda shader: shader['name'] != shaders.LR_LEGACY)
    counted = set()
    names = set()
    for path, directory, name in embedded:
        expected = os.path.join(shader_root, 'dx', 'shaders', directory, name + '.dxil')
        if canonical(path) != canonical(expected):
            fail('the library embeds %s, which is not in the shader build %s' % (path, shader_root))
        entries, od = built.get(directory, ([], None))
        entry = [shader for shader in entries if shader['name'] == name]
        if len(entry) != 1:
            fail('the library embeds %s, which the shader build of %s does not have' % (path, report_path))
        if shaders.sha256_file(path) != entry[0]['bytecode_sha256']:
            fail('%s is not the bytecode the build of %s wrote' % (path, report_path))
        # 'key' is of the compiler call the set made for the shader; the bytecode may be of another
        # ('bytecode_key': the default bytecode where the set od cannot have its own).
        if shaders.shader_key(name, od(entry[0]), report['variant'], report['dxc_identity']) != entry[0]['key']:
            fail('%s: the sources in the repository are not the ones %s was compiled from' % (name, path))
        for relative, _ in shaders.source_files(name):
            sources.add(os.path.join(shaders.SHADERS, relative), 'HLSL', 'shaders.py, ' + name)
            counted.add(relative)
        names.add(name)
    print('language_bar.py: HLSL: %d files behind the %d shaders in the library (%d shaders compiled, %s)' % (
        len(counted), len(names), len(report['shaders']), report['shader_set']))
    return {'compiler': report['shader_set'], 'in_library': len(names), 'compiled': len(report['shaders'])}


def measure(sources):
    """[{name, colour, bytes, lines, files, percent}], the largest first; percentages in tenths
    that add up to 100."""
    languages = {}
    for relative, language in sources.files.items():
        with open(os.path.join(REPO, relative), 'rb') as f:
            data = f.read()
        entry = languages.setdefault(language, {'name': language, 'colour': COLOURS[language], 'bytes': 0, 'lines': 0,
                                                'files': 0})
        entry['bytes'] += len(data)
        entry['lines'] += data.count(b'\n') + (1 if data and not data.endswith(b'\n') else 0)
        entry['files'] += 1
    for language in REQUIRED:
        if language not in languages or not languages[language]['bytes']:
            fail('the build has no %s' % language)
    ordered = sorted(languages.values(), key=lambda entry: (-entry['bytes'], entry['name']))
    total = sum(entry['bytes'] for entry in ordered)
    # Largest remainder: every share rounded down to a tenth, the tenths left over to the largest rests.
    exact = [entry['bytes'] * 1000 / total for entry in ordered]
    tenths = [int(value) for value in exact]
    for index in sorted(range(len(ordered)), key=lambda i: exact[i] - tenths[i], reverse=True)[:1000 - sum(tenths)]:
        tenths[index] += 1
    for entry, value in zip(ordered, tenths):
        entry['percent'] = value / 10
    return ordered, total


def commit_of_checkout(commit, date):
    def git(*arguments):
        try:
            return subprocess.run(['git', '-C', REPO] + list(arguments), stdout=subprocess.PIPE, check=True
                                  ).stdout.decode().strip()
        except (OSError, subprocess.CalledProcessError) as error:
            fail('git %s failed: %s' % (' '.join(arguments), error))
    commit = commit or git('rev-parse', 'HEAD')
    # The date of the commit, in UTC: the same files for the same commit.
    stamp = date or git('show', '-s', '--format=%ct', commit)
    if stamp.isdigit():
        stamp = datetime.datetime.fromtimestamp(int(stamp), datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    return commit, stamp


def svg(languages, commit, date):
    """A bar of the shares and a line of names below it, 800 wide. One grey for the text that reads on
    the white and on the dark background of GitHub: an image in a README does not learn the theme of
    the page (prefers-color-scheme would be the system's, which need not be GitHub's)."""
    width, bar_height, radius = 800, 10, 5
    text_colour = '#6e7781'
    # Every language is seen: at least 3 units of the bar, taken from the largest.
    total = sum(entry['bytes'] for entry in languages)
    widths = [max(3.0, width * entry['bytes'] / total) for entry in languages]
    widths[0] -= sum(widths) - width
    label = ', '.join('%s %.1f%%' % (entry['name'], entry['percent']) for entry in languages)
    out = ['<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="42" viewBox="0 0 %d 42" role="img" '
           'aria-label="rav1g.dll is built from: %s">' % (width, width, label),
           '<title>rav1g.dll is built from: %s (bytes of source, commit %s, %s)</title>' % (label, commit[:7], date[:10]),
           '<clipPath id="bar"><rect width="%d" height="%d" rx="%d"/></clipPath>' % (width, bar_height, radius),
           '<g clip-path="url(#bar)">']
    x = 0.0
    for entry, w in zip(languages, widths):
        out.append('<rect x="%.1f" width="%.1f" height="%d" fill="%s"/>' % (x, w, bar_height, entry['colour']))
        x += w
    out.append('</g>')
    # White lines between the parts would be dark lines' opposite on a dark page: the grey of the text.
    x = 0.0
    for w in widths[:-1]:
        x += w
        out.append('<rect x="%.1f" width="1" height="%d" fill="%s" opacity="0.6"/>' % (x - 0.5, bar_height, text_colour))
    out.append('<g font-family="-apple-system,BlinkMacSystemFont,\'Segoe UI\',\'Noto Sans\',Helvetica,Arial,sans-serif" '
               'font-size="13" fill="%s">' % text_colour)
    x = 0.0
    for entry in languages:
        name, share = entry['name'], '%.1f%%' % entry['percent']
        # The ring keeps the dark colours (C, Assembly) visible on a dark page.
        out.append('<circle cx="%.1f" cy="30" r="4.5" fill="%s" stroke="%s" stroke-opacity="0.6"/>' % (
            x + 5, entry['colour'], text_colour))
        out.append('<text x="%.1f" y="34.5"><tspan font-weight="600">%s</tspan> %s</text>' % (x + 16, name, share))
        # No text measuring here: 8 units a letter is more than the widest of these fonts takes.
        x += 16 + 8 * (len(name) + 1 + len(share)) + 14
    out += ['</g>', '</svg>', '']
    if x > width:
        fail('the legend of the image is %d wide, more than %d' % (x, width))
    return '\n'.join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--release-dir', required=True,
                        help='the directory of the build with deps and build: <target dir>/<target>/release')
    parser.add_argument('--out', required=True, help='directory for languages.json and languages.svg')
    parser.add_argument('--package', default='rav1g', help='the package the library is (default: rav1g)')
    parser.add_argument('--target', default='x86_64-pc-windows-msvc', help='the target of the build')
    parser.add_argument('--shader-report',
                        help='report.json of the shader build (default: in the RAV1G_SHADER_DIR of the build)')
    parser.add_argument('--commit', default=os.environ.get('GITHUB_SHA'), help='default: GITHUB_SHA, or HEAD')
    parser.add_argument('--date', help='default: the date of the commit')
    args = parser.parse_args()

    if not os.path.isdir(os.path.join(args.release_dir, 'build')):
        fail('%s has no build directory: not the release directory of a build' % args.release_dir)
    sources = Sources(args.release_dir)
    crates, workspace_root = library_crates(args.package, args.target)
    embedded = rust_sources(sources, crates, workspace_root)
    shader_root = compiled_sources(sources, crates)
    shader_set = shader_sources(sources, shader_root, embedded, args.shader_report)
    if not sources.files:
        fail('the build has no source files')
    languages, total = measure(sources)
    commit, date = commit_of_checkout(args.commit, args.date)

    by_language = {entry['name']: sorted(path for path, language in sources.files.items() if language == entry['name'])
                   for entry in languages}
    result = {
        'what': 'The source files rav1g.dll is built from, by language, as recorded by its release build: '
                'testing/language_bar/language_bar.py',
        'commit': commit,
        'date': date,
        'target': args.target,
        'crates': [name for name, _, _, _ in crates],
        'shaders': shader_set,
        'total': {'bytes': total, 'lines': sum(entry['lines'] for entry in languages), 'files': len(sources.files)},
        'languages': languages,
        'files': by_language,
        'written_by_the_build_not_counted': sorted(sources.generated),
    }
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, 'languages.json'), 'w', encoding='utf-8', newline='\n') as f:
        json.dump(result, f, indent=1)
        f.write('\n')
    with open(os.path.join(args.out, 'languages.svg'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(svg(languages, commit, date))
    lines = ['%s %.1f %% (%d bytes, %d lines, %d files)' % (entry['name'], entry['percent'], entry['bytes'],
                                                             entry['lines'], entry['files']) for entry in languages]
    for line in lines:
        print('language_bar.py: ' + line)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('\n'.join(['', '## rav1g.dll is built from', ''] + ['- ' + line for line in lines]) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
