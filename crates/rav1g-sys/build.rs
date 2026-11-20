// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Embeds the bytecode of the shaders: no C, nothing is compiled here.
//!
//! Nothing of `libav1` is compiled or read here. The decoder's parser is rav1d's parse pass
//! (`crates/rav1g-parse`, docs/phase2-rav1d.md), its host is Rust (`crates/rav1g`), and the types that
//! were bindings to the headers of `libav1` are Rust of this crate (`src/`; docs/rust-port.md, "Own
//! types, no libaom headers").
//!
//! **For a Windows target (the decoder)** the script writes `shader_table.rs` into `OUT_DIR`: the table of
//! the shaders of `shaders.txt`, in that order, each with an `include_bytes!` of its bytecode, so the
//! library has the files DXC wrote, byte for byte (`src/shaders.rs`). `RAV1G_SHADER_DIR` names the
//! directory with the DXC bytecode under `dx/shaders/dxil` and `shader_set.txt`
//! (`testing/shader_cache/shaders.py build --dxc <dxc> --root <dir>`). The `shader-pick` feature embeds the
//! alternate DXC bytecode from `dx/shaders/dxil_od`, which the experiment package can select per shader
//! with `-shaderpick`. (The table was C, `shader_table.c`, compiled by `cc` from bytecode headers, until
//! 2026-11-19: docs/rust-port.md, "No C in the library".)
//!
//! **For a target that is not Windows** the table is empty: the D3D12 host does not build there.
//! `crates/rav1g-parse` and `tools/rav1g-parse-check` use the types on Linux (docs/phase2-rav1d.md,
//! step 2).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn fail(message: &str) -> ! {
    eprintln!("rav1g-sys: {message}");
    std::process::exit(1);
}

/// The directory with `dx/shaders/dxil/<shader>.dxil`, from `RAV1G_SHADER_DIR`.
fn shader_dir() -> PathBuf {
    println!("cargo:rerun-if-env-changed=RAV1G_SHADER_DIR");
    let how = "set it to the directory with DXC cs_6_0 bytecode \
               (testing/shader_cache/shaders.py build --dxc <dxc> --root <dir>)";
    let Some(value) = env::var_os("RAV1G_SHADER_DIR").filter(|value| !value.is_empty()) else {
        fail(&format!("RAV1G_SHADER_DIR is not set: {how}"));
    };
    // Absolute: the table names the files to rustc, which reads a relative path from the directory of the
    // table and not from here.
    let dir = PathBuf::from(value);
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    let bytecode = dir.join("dx").join("shaders").join("dxil");
    if !bytecode.is_dir() {
        let headers = dir.join("dx").join("shaders").join("h");
        let old = if headers.is_dir() {
            " (it has the bytecode headers of an earlier shaders.py, dx/shaders/h: build the shaders again)"
        } else {
            ""
        };
        fail(&format!("RAV1G_SHADER_DIR={}: there is no {}{old} : {how}", dir.display(), bytecode.display()));
    }
    let count = fs::read_dir(&bytecode)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "dxil"))
                .count()
        })
        .unwrap_or(0);
    if count < 80 {
        fail(&format!(
            "RAV1G_SHADER_DIR={}: {} has only {count} shaders (.dxil): {how}",
            dir.display(),
            bytecode.display()
        ));
    }
    println!("cargo:rerun-if-changed={}", bytecode.display());
    dir
}

/// The compiler line of the DXC build, for the version text of the library.
fn shader_set(shaders: Option<&Path>) -> String {
    let Some(shaders) = shaders else {
        return "none (not a Windows target)".to_string();
    };
    let file = shaders.join("shader_set.txt");
    println!("cargo:rerun-if-changed={}", file.display());
    let line = fs::read_to_string(&file).unwrap_or_else(|error| {
        fail(&format!(
            "cannot read {}: {error}; build the DXC shaders with testing/shader_cache/shaders.py build --dxc <dxc>",
            file.display()
        ))
    });
    let line = line.trim();
    if !line.starts_with("DXC ")
        || !line.ends_with(" cs_6_0")
        || !line.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
    {
        fail(&format!("{}: expected the compiler line of a DXC cs_6_0 build, got {line:?}", file.display()));
    }
    line.to_owned()
}

/// The compiler and arguments of the `dxc` and `od` sets of `shader-pick`, from `shader_sets.txt`, for
/// the library's log.
fn shader_sets(shaders: &Path) -> [String; 2] {
    let file = shaders.join("shader_sets.txt");
    println!("cargo:rerun-if-changed={}", file.display());
    let text = fs::read_to_string(&file).unwrap_or_else(|_| {
        fail(&format!(
            "the shader-pick feature needs the shader-pick variant: there is no {} \
             (testing/shader_cache/shaders.py build --dxc <dxc> --variant shader-pick --root <dir>)",
            file.display()
        ))
    });
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let line = |index: usize, prefix: &str| -> String {
        match lines.get(index).and_then(|line| line.strip_prefix(prefix)) {
            Some(rest) if lines.len() == 2 && rest.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) => {
                rest.trim().to_string()
            }
            _ => fail(&format!("{}: expected the two lines dxc: and od: of a DXC build", file.display())),
        }
    };
    [line(0, "dxc:"), line(1, "od:")]
}

/// The names of the shaders of the table, in its order: the lines of `shaders.txt`.
fn shader_names(list: &Path) -> Vec<String> {
    println!("cargo:rerun-if-changed={}", list.display());
    let text =
        fs::read_to_string(list).unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", list.display())));
    let names: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect();
    for (index, name) in names.iter().enumerate() {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            fail(&format!("{}: {name:?} is not the name of a shader", list.display()));
        }
        if names[..index].contains(name) {
            fail(&format!("{}: {name} is there twice", list.display()));
        }
    }
    if names.len() < 80 {
        fail(&format!("{}: {} shaders", list.display(), names.len()));
    }
    names
}

/// A 64-bit FNV-1a hash: the test of `src/shaders.rs` (which has the same function) holds the bytes
/// rustc embedded against the bytes this script read.
fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// One set of the table as Rust: `pub static <table>: &[Shader]` with an `include_bytes!` of
/// `<directory>/<name>.dxil` per name, in the order of the names. Each file has to be what DXC writes: a
/// DXBC container of the length its header gives, with a DXIL part and the hash of the validator (D3D12
/// takes no unsigned DXIL). Nothing is converted on the way: the library has the bytes of the file.
fn shader_table(text: &mut String, table: &str, directory: &Path, names: &[String], what: &str) {
    println!("cargo:rerun-if-changed={}", directory.display());
    let (mut entries, mut read) = (String::new(), String::new());
    for name in names {
        let file = directory.join(format!("{name}.dxil"));
        println!("cargo:rerun-if-changed={}", file.display());
        let data =
            fs::read(&file).unwrap_or_else(|error| fail(&format!("{what}: missing {}: {error}", file.display())));
        let field =
            |at: usize| data.get(at..at + 4).map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize);
        let parts = |count: usize| {
            (0..count)
                .filter_map(|index| field(32 + 4 * index))
                .any(|offset| data.get(offset..offset + 4) == Some(b"DXIL"))
        };
        let problem = if data.get(..4) != Some(b"DXBC") {
            Some("not a DXBC container")
        } else if field(24) != Some(data.len()) {
            Some("not of the length its container header gives")
        } else if !field(28).is_some_and(parts) {
            Some("a container without a DXIL part")
        } else if data[4..20].iter().all(|byte| *byte == 0) {
            Some("DXIL without the hash of the validator (not signed)")
        } else {
            None
        };
        if let Some(problem) = problem {
            fail(&format!("{what}: {} is {problem}: not what DXC wrote", file.display()));
        }
        // The path as a Rust string literal, whatever is in it.
        let path = file.to_str().unwrap_or_else(|| fail(&format!("{}: the path is not UTF-8", file.display())));
        let _ = writeln!(entries, "    Shader {{ name: {name:?}, code: include_bytes!({path:?}) }},");
        let _ = writeln!(read, "    ({}, {:#018x}),", data.len(), fnv1a(&data));
    }
    let _ =
        writeln!(text, "/// {what}, in the order of `shaders.txt`.\npub static {table}: &[Shader] = &[\n{entries}];\n");
    let _ = writeln!(
        text,
        "/// The length and the FNV-1a hash of each file of `{table}` as the build script read it.\n\
         #[cfg(test)]\nstatic {table}_READ: &[(usize, u64)] = &[\n{read}];\n"
    );
}

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    // The decoder is for Windows. Elsewhere the crate is its Rust alone (see the top).
    let windows = target.ends_with("-windows-msvc");
    if !windows && env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        fail(&format!("target {target}: of the Windows targets the decoder builds for *-windows-msvc only"));
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));

    let pick = env::var_os("CARGO_FEATURE_SHADER_PICK").is_some();
    if pick && env::var_os("CARGO_FEATURE_UAVSYNC").is_some() {
        fail("shader-pick and uavsync require different shader builds");
    }
    let shaders = windows.then(shader_dir);
    let shaders = shaders.as_deref();
    // What compiled the shaders, for the version text of the library.
    println!("cargo:rustc-env=RAV1G_SHADER_SET={}", shader_set(shaders));

    // The table of the bytecode (src/shaders.rs includes it). The shader-pick feature embeds the alternate
    // -Od set as well.
    let names = shader_names(&manifest.join("shaders.txt"));
    let mut table = String::from("// Written by crates/rav1g-sys/build.rs from shaders.txt and RAV1G_SHADER_DIR.\n\n");
    let mut sets: [String; 2] = Default::default();
    match shaders {
        Some(shaders) => {
            let bytecode = shaders.join("dx").join("shaders");
            shader_table(&mut table, "SHADERS", &bytecode.join("dxil"), &names, "The DXC bytecode of the shaders");
            if pick {
                sets = shader_sets(shaders);
                let what = "The shader-pick feature: the DXC -Od bytecode of the shaders";
                shader_table(&mut table, "SHADERS_OD", &bytecode.join("dxil_od"), &names, what);
            }
        }
        None => {
            table += "/// No shaders: not a Windows target.\npub static SHADERS: &[Shader] = &[];\n\
                      #[cfg(test)]\nstatic SHADERS_READ: &[(usize, u64)] = &[];\n"
        }
    }
    if let Err(error) = fs::write(out.join("shader_table.rs"), table) {
        fail(&format!("cannot write shader_table.rs: {error}"));
    }
    for (name, line) in ["DXC", "OD"].iter().zip(&sets) {
        println!("cargo:rustc-env=RAV1G_SHADER_SET_{name}={line}");
    }
    // The C runtime the library is built for, for its version text (rav1g_version of rav1g.h).
    println!(
        "cargo:rustc-env=RAV1G_RUNTIME={}",
        if !windows {
            "the C runtime of the target"
        } else if env::var("CARGO_CFG_TARGET_FEATURE")
            .is_ok_and(|features| features.split(',').any(|f| f == "crt-static"))
        {
            "static runtime"
        } else {
            "DLL runtime"
        }
    );
    if windows {
        println!("cargo:rustc-link-lib=d3d12");
        println!("cargo:rustc-link-lib=dxgi");
        println!("cargo:rustc-link-lib=dxguid");
    }
}
