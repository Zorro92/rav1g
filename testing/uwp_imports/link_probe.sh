#!/bin/sh
# Links the static library of the decoder the way an app container application links it, without the
# application: a DLL that has every function of rav1g.h, with WindowsApp.lib in front of the libraries
# of the decoder, so that the functions of Windows come from the API sets that WindowsApp.lib names and
# not from the import stubs for kernel32.dll that the Rust library carries (docs/rust-port.md, "The app
# container"). check_imports.py then reads the DLL. lld-link, on any machine: the Linux build of CI.
#
# usage: link_probe.sh <rav1g.lib> <directory of rav1g_c.lib> <xwin directory> <out directory>
#
# The xwin directory is what cargo xwin keeps the Windows SDK and the C runtime in (~/.cache/cargo-xwin/xwin).
# It has the desktop C runtime only, so the DLL imports from vcruntime140.dll where the player imports
# from vcruntime140_app.dll.
set -eu
lib=$1
c_libs=$2
xwin=$3
out=$4
here=$(dirname "$0")
mkdir -p "$out"
exports=$(grep -o 'rav1g_[a-z0-9_]*(' "$here/../../crates/rav1g/include/rav1g.h" | tr -d '(' | sort -u | sed 's|^|/export:|')
test -n "$exports"
# ntdll.lib and userenv.lib are for functions of the standard library that are in the objects the linker
# loads and not in the DLL it writes (file and pipe calls, the home directory).
# shellcheck disable=SC2086
lld-link /dll /appcontainer "/out:$out/probe.dll" /opt:ref /opt:icf /machine:x64 $exports \
  "/libpath:$xwin/crt/lib/x86_64" "/libpath:$xwin/sdk/lib/um/x86_64" "/libpath:$xwin/sdk/lib/ucrt/x86_64" \
  WindowsApp.lib "$lib" "$c_libs/rav1g_c.lib" \
  d3d12.lib dxgi.lib dxguid.lib ntdll.lib userenv.lib
echo "linked $out/probe.dll with $(echo "$exports" | wc -l) functions of rav1g.h"
