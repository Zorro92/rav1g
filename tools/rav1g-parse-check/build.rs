// SPDX-License-Identifier: BSD-2-Clause

//! The block lists are counted by the host's own code (`crates/rav1g/src/host/blocks.rs`, compiled into
//! this program by path). It names the lists by the constants of the host's `types.rs`, a file that does
//! not build without D3D12; this script reads the constants from it, so that the program has the host's
//! values and not a copy of them.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let types = manifest.join("../../crates/rav1g/src/host/types.rs");
    println!("cargo:rerun-if-changed={}", types.display());
    let text = fs::read_to_string(&types).unwrap_or_else(|error| panic!("cannot read {}: {error}", types.display()));
    let mut constants = String::from("// Written by build.rs from crates/rav1g/src/host/types.rs.\n");
    let mut count = 0;
    for line in text.lines() {
        if line.starts_with("pub const ") && line.contains(": c_int = ") && line.ends_with(';') {
            constants.push_str(line);
            constants.push('\n');
            count += 1;
        }
    }
    assert!(count >= 30, "{}: only {count} constants found", types.display());
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("host_constants.rs"), constants).expect("cannot write host_constants.rs");
}
