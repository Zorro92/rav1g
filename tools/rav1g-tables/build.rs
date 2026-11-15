// SPDX-License-Identifier: BSD-2-Clause

//! This program compiles the host's `blocks.rs`, `tables.rs` and `libaom.rs` by path, through the
//! module `tools/rav1g-parse-check` has for that (`src/host`). What that module needs written before it
//! compiles (the names of the block lists from the host's `types.rs`, the tables of the host's build
//! script) its build script writes; it is a module here and writes them for this program too.

#[path = "../rav1g-parse-check/build.rs"]
mod parse_check;

fn main() {
    parse_check::main();
    println!(
        "cargo:rerun-if-changed={}/../rav1g-parse-check/build.rs",
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")
    );
}
