// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Builds the little C the decoder has: the shader table.
//!
//! Nothing of `libav1` is compiled or read here. The decoder's parser is rav1d's parse pass
//! (`crates/rav1g-parse`, docs/phase2-rav1d.md), its host is Rust (`crates/rav1g`), and the types that
//! were bindings to the headers of `libav1` are Rust of this crate (`src/`; docs/rust-port.md, "Own
//! types, no libaom headers").
//!
//! **For a Windows target (the decoder)** one C file is compiled into `rav1g_c.lib`: `shader_table.c`,
//! the bytecode of the shaders. `RAV1G_SHADER_DIR` names the directory with the DXC bytecode headers
//! under `dx/shaders/h` and `shader_set.txt` (`testing/shader_cache/shaders.py build --dxc <dxc> --root
//! <dir>`). The `shader-pick` feature embeds two extra bytecodes per shader from `dx/shaders/h_od`
//! and `h_fxc`, which the experiment package can select per shader with `-shaderpick`.
//!
//! **For a target that is not Windows** the crate is its Rust alone: the D3D12 host does not build there.
//! `crates/rav1g-parse` and `tools/rav1g-parse-check` use the types on Linux (docs/phase2-rav1d.md,
//! step 2).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Link time optimisation of the C of the decoder, which the MSBuild projects of the C++ decoder have
/// as whole program optimisation (`/GL`, `/LTCG`). It was chosen while the entropy decoder of libaom was
/// part of this library (docs/rust-port.md, "The entropy decoder with link time optimisation"); the one
/// file that is left, the shader table, is compiled the same way.
///
/// The objects of such a build are not machine code: MSVC writes its intermediate language, clang LLVM
/// bitcode, and the linker compiles them. Two things follow:
///
/// - The libraries are not copied into the `.rlib` of this crate (`static:-bundle`). rustc writes the
///   symbol table of an archive itself and cannot read the objects of `/GL`, so the linker found none of
///   their functions (LNK1120); and what it reads of bitcode depends on its LLVM being as new as clang's.
///   The linker is given the libraries that `lib.exe` or `llvm-lib` wrote.
/// - Under MSVC the link needs `/LTCG`. `link.exe` starts again with it when it meets such an object, so
///   a program links without (the tests of the `rav1g` crate do); the build scripts of this crate and of
///   `rav1g-dec` pass it so that it does not have to. They pass `/LTCG:incremental`, which is what the
///   MSBuild projects link with: the code generator of the link is not the same in the two modes, and the
///   entropy decoder it makes with plain `/LTCG` is slower (other code for the same functions, the symbol
///   decoder among them; docs/rust-port.md). The mode leaves an `.iobj` and an `.ipdb` file next to the
///   program in the target directory.
///
/// `RAV1G_C_LTO` chooses for a measurement: `off`, `thin` or `full` (clang-cl), `on` (the default: `/GL`
/// under MSVC, full LTO under clang-cl, which was the faster of the two there).
#[derive(Clone, Copy, PartialEq)]
enum Lto {
    Off,
    /// MSVC: `/GL`.
    Msvc,
    /// clang-cl: `-flto=thin`.
    Thin,
    /// clang-cl: `-flto`.
    Full,
}

fn lto() -> Lto {
    println!("cargo:rerun-if-env-changed=RAV1G_C_LTO");
    let choice = env::var("RAV1G_C_LTO").unwrap_or_default();
    let clang_cl = cc::Build::new().get_compiler().is_like_clang_cl();
    match (choice.as_str(), clang_cl) {
        ("off", _) => Lto::Off,
        ("" | "on", false) => Lto::Msvc,
        ("thin", true) => Lto::Thin,
        ("" | "on" | "full", true) => Lto::Full,
        _ => fail(&format!("RAV1G_C_LTO={choice}: off, on, or with clang-cl thin or full")),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("rav1g-sys: {message}");
    std::process::exit(1);
}

/// The directory with `dx/shaders/h/<shader>.h`, from `RAV1G_SHADER_DIR`.
fn shader_dir() -> PathBuf {
    println!("cargo:rerun-if-env-changed=RAV1G_SHADER_DIR");
    let how = "set it to the directory with DXC cs_6_0 bytecode headers \
               (testing/shader_cache/shaders.py build --dxc <dxc> --root <dir>)";
    let Some(value) = env::var_os("RAV1G_SHADER_DIR").filter(|value| !value.is_empty()) else {
        fail(&format!("RAV1G_SHADER_DIR is not set: {how}"));
    };
    let dir = PathBuf::from(value);
    let headers = dir.join("dx").join("shaders").join("h");
    if !headers.is_dir() {
        fail(&format!("RAV1G_SHADER_DIR={}: there is no {} : {how}", dir.display(), headers.display()));
    }
    let count = fs::read_dir(&headers)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "h"))
                .count()
        })
        .unwrap_or(0);
    if count < 80 {
        fail(&format!(
            "RAV1G_SHADER_DIR={}: {} has only {count} shader headers: {how}",
            dir.display(),
            headers.display()
        ));
    }
    println!("cargo:rerun-if-changed={}", headers.display());
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
            "cannot read {}: {error}; generate DXC headers with testing/shader_cache/shaders.py build --dxc <dxc>",
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

/// The feature `shader-pick`: the two extra bytecodes of each shader (`dx/shaders/h_od`, DXC with
/// `-Od`; `dx/shaders/h_fxc`, FXC's), for the shader pick of the library. Writes `rav1g_shader_sets.inc`
/// to `out`, which `shader_table.c` includes with `RAV1G_SHADER_SETS`: the headers and a table per set in
/// the order of the table of `shader_table.c`. Returns the lines of `shader_sets.txt` (dxc, od, fxc): the
/// compiler and the arguments of each set, for the log of the library.
fn shader_sets(shaders: &Path, table: &Path, out: &Path) -> [String; 3] {
    let file = shaders.join("shader_sets.txt");
    println!("cargo:rerun-if-changed={}", file.display());
    let text = fs::read_to_string(&file).unwrap_or_else(|_| {
        fail(&format!(
            "the shader-pick feature needs the shader-pick variant: there is no {} \
             (testing/shader_cache/shaders.py build --dxc <dxc> --fxc <fxc> --variant shader-pick --root <dir>)",
            file.display()
        ))
    });
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let line = |index: usize, prefix: &str| -> String {
        match lines.get(index).and_then(|line| line.strip_prefix(prefix)) {
            Some(rest) if lines.len() == 3 && rest.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) => {
                rest.trim().to_string()
            }
            _ => fail(&format!("{}: not the three lines dxc:, od:, fxc: of a DXC build", file.display())),
        }
    };
    let sets = [line(0, "dxc:"), line(1, "od:"), line(2, "fxc:")];

    let source = fs::read_to_string(table).unwrap_or_else(|_| fail(&format!("cannot read {}", table.display())));
    let names: Vec<&str> =
        source.lines().filter_map(|line| line.trim().strip_prefix("RAV1G_SHADER(")?.strip_suffix("),")).collect();
    if names.len() < 80 {
        fail(&format!("{}: {} RAV1G_SHADER lines", table.display(), names.len()));
    }
    let mut text = String::from("/* Written by crates/rav1g-sys/build.rs from shader_table.c for shader-pick. */\n");
    for (directory, suffix) in [("h_od", "_od"), ("h_fxc", "_fxc")] {
        let headers = shaders.join("dx").join("shaders").join(directory);
        println!("cargo:rerun-if-changed={}", headers.display());
        for name in &names {
            if !headers.join(format!("{name}.h")).is_file() {
                fail(&format!("the shader-pick feature: missing {}", headers.join(format!("{name}.h")).display()));
            }
            text += &format!("#include \"dx/shaders/{directory}/{name}.h\"\n");
        }
        text += &format!("static const rav1g_shader_bytecode rav1g_shaders{suffix}[] = {{\n");
        for name in &names {
            text += &format!("  {{ \"{name}\", {name}{suffix}, (unsigned int)sizeof({name}{suffix}) }},\n");
        }
        text += "};\n";
    }
    text += "const rav1g_shader_bytecode *rav1g_shader_table_set(unsigned int set, unsigned int *count) {\n  \
             *count = (unsigned int)(sizeof(rav1g_shaders) / sizeof(rav1g_shaders[0]));\n  \
             return set == 1 ? rav1g_shaders_od : set == 2 ? rav1g_shaders_fxc : rav1g_shaders;\n}\n";
    if let Err(error) = fs::write(out.join("rav1g_shader_sets.inc"), text) {
        fail(&format!("cannot write rav1g_shader_sets.inc: {error}"));
    }
    sets
}

/// A compiler call with the settings every file of the library has.
fn base_build(shaders: Option<&Path>) -> cc::Build {
    let mut build = cc::Build::new();
    build.warnings(false);
    if let Some(shaders) = shaders {
        // The headers of this build first: shader_table.c includes them as "dx/shaders/h/<name>.h".
        build.include(shaders);
    }
    build
        .define("_CRT_SECURE_NO_WARNINGS", None)
        .define("_UNICODE", None)
        .define("UNICODE", None)
        .define("_CONSOLE", None)
        // No NDEBUG, whatever the profile: the asserts of the library stay, as in the player.
        .flag_if_supported("/sdl")
        .flag_if_supported("/wd4996");
    if env::var_os("CARGO_FEATURE_UAVSYNC").is_some() {
        build.define("RAV1G_UAVSYNC", None);
    }
    if env::var_os("CARGO_FEATURE_APP_CONTAINER").is_some() {
        // What MSBuild defines for a project with AppContainerApplication: the headers of the Windows SDK
        // declare the API of an app container application only, so a call outside of it does not compile.
        build.define("WINAPI_FAMILY", "WINAPI_FAMILY_APP");
    }
    build
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
    println!("cargo:rerun-if-changed={}", manifest.join("shader_table.c").display());

    let pick = env::var_os("CARGO_FEATURE_SHADER_PICK").is_some();
    if pick && env::var_os("CARGO_FEATURE_UAVSYNC").is_some() {
        fail("shader-pick and uavsync require different shader builds");
    }
    let shaders = windows.then(shader_dir);
    let shaders = shaders.as_deref();
    // What compiled the shaders, for the version text of the library.
    println!("cargo:rustc-env=RAV1G_SHADER_SET={}", shader_set(shaders));

    // The C of the decoder, one static library: the shader table, at /O1 /Ot whatever the cargo profile.
    let mut c_size = base_build(shaders);
    // The shader-pick feature embeds the two extra sets.
    let sets = match shaders {
        Some(shaders) if pick => {
            c_size.define("RAV1G_SHADER_SETS", None).include(&out);
            shader_sets(shaders, &manifest.join("shader_table.c"), &out)
        }
        _ => Default::default(),
    };
    for (name, line) in ["DXC", "OD", "FXC"].iter().zip(&sets) {
        println!("cargo:rustc-env=RAV1G_SHADER_SET_{name}={line}");
    }
    c_size.opt_level(1).flag_if_supported("/Ot");
    let lto = if windows { lto() } else { Lto::Off };
    // What built the C, for the version text of the library (rav1g_version of rav1g.h).
    println!(
        "cargo:rustc-env=RAV1G_C_BUILD={}, {}, {} runtime",
        if !windows {
            "the C compiler of the target"
        } else if c_size.get_compiler().is_like_clang_cl() {
            "clang-cl"
        } else {
            "MSVC"
        },
        match lto {
            Lto::Off => "no link time optimisation",
            Lto::Msvc => "/GL",
            Lto::Thin => "thin LTO",
            Lto::Full => "full LTO",
        },
        if env::var("CARGO_CFG_TARGET_FEATURE").is_ok_and(|features| features.split(',').any(|f| f == "crt-static")) {
            "static"
        } else {
            "DLL"
        }
    );
    match lto {
        Lto::Off => {}
        Lto::Msvc => {
            c_size.flag("/GL");
        }
        Lto::Thin => {
            c_size.flag("-flto=thin");
        }
        Lto::Full => {
            c_size.flag("-flto");
        }
    }
    // The library goes to the linker as it is, not through the .rlib of this crate (see Lto).
    c_size.cargo_metadata(false);
    c_size.include(&manifest);
    if windows {
        // The bytecode of the shaders, from the headers of RAV1G_SHADER_DIR (shader_table.c).
        c_size.file(manifest.join("shader_table.c"));
        c_size.compile("rav1g_c");
        println!("cargo:rustc-link-search=native={}", out.display());
        println!("cargo:rustc-link-lib=static:-bundle=rav1g_c");
        // An application that links the static library of `rav1g` itself (the UWP test player) needs the
        // library of the C as well: it is not in rav1g.lib (see Lto). RAV1G_C_LIB_DIR names a directory it
        // is copied to.
        println!("cargo:rerun-if-env-changed=RAV1G_C_LIB_DIR");
        if let Some(dir) = env::var_os("RAV1G_C_LIB_DIR").filter(|dir| !dir.is_empty()).map(PathBuf::from) {
            let library = "rav1g_c.lib";
            if let Err(error) = fs::create_dir_all(&dir).and_then(|()| fs::copy(out.join(library), dir.join(library))) {
                fail(&format!("RAV1G_C_LIB_DIR={}: cannot copy {library}: {error}", dir.display()));
            }
        }
        if lto == Lto::Msvc {
            // For the tests of this crate; a build script cannot give it to the programs of other crates.
            println!("cargo:rustc-link-arg=/LTCG:incremental");
        }
        println!("cargo:rustc-link-lib=d3d12");
        println!("cargo:rustc-link-lib=dxgi");
        println!("cargo:rustc-link-lib=dxguid");
    }
}
