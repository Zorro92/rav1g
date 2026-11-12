#!/bin/sh
# Builds and runs the self-test of the Shader Model 6 test's comparisons on a Linux host (g++, no GPU):
# emulated outputs of the test shaders must pass, outputs with one word changed must fail
# (checks_test.cpp; docs/test-player.md, "Shader Model 6 test").
#
#   sh testing/sm6test/run.sh        exit code 0 = every expectation met
set -eu

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

g++ -std=c++14 -O2 -Wall -Wextra -Werror -I "$here/.." -o "$work/checks_test" "$here/checks_test.cpp"
"$work/checks_test"
