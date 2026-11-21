#!/bin/sh
# The self-test of the intra step timing script (testing/steptime/steptime.py): its parser of the STEPTIME
# lines and its fit on made-up steps with known costs. Plain Python 3, no GPU.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
python3 "$here/steptime.py" selftest
