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
//! the bytecode of the shaders. The shaders are not compiled here. `RAV1G_SHADER_DIR` names the directory
//! whose `dx/shaders/h` has the bytecode headers of the FXC build (`testing/shader_cache/shaders.py build
//! --root <dir>`; in CI the `normal` output of `.github/actions/shaders`). With the feature `dxc` it is
//! the directory of the DXC build of the same sources (`--variant dxc`; docs/gpu-av1-decoder.md, "DXC
//! experiment"), and the library says so (`SHADER_SET`).
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
    let how = "set it to the directory whose dx/shaders/h has the bytecode headers of the FXC build \
               (testing/shader_cache/shaders.py build --root <dir>; in CI the 'normal' output of \
               .github/actions/shaders)";
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
            "RAV1G_SHADER_DIR={}: {} has {count} headers, the decoder has 92 shaders: {how}",
            dir.display(),
            headers.display()
        ));
    }
    println!("cargo:rerun-if-changed={}", headers.display());
    dir
}

/// What compiled the shaders, for the version text of the library: "FXC cs_5_0", or with the feature
/// `dxc` the line of `shader_set.txt` in the directory of the shaders ("DXC 1.9.2602.17 cs_6_0"), which
/// the DXC build of testing/shader_cache/shaders.py writes and no other. The feature and the file go
/// together, so a library cannot say FXC about DXC's shaders or the other way round.
fn shader_set(shaders: Option<&Path>) -> String {
    let dxc = env::var_os("CARGO_FEATURE_DXC").is_some();
    if dxc && env::var_os("CARGO_FEATURE_UAVSYNC").is_some() {
        fail("the features dxc and uavsync are two builds of the shaders: one of them");
    }
    let Some(shaders) = shaders else {
        return "none (not a Windows target)".to_string();
    };
    let file = shaders.join("shader_set.txt");
    println!("cargo:rerun-if-changed={}", file.display());
    let text = fs::read_to_string(&file).ok().map(|text| text.trim().to_string()).filter(|text| !text.is_empty());
    match (dxc, text) {
        (false, None) => "FXC cs_5_0".to_string(),
        (true, Some(text)) => {
            if !text.starts_with("DXC ") || !text.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) {
                fail(&format!("{}: not the line of a DXC build: {text:?}", file.display()));
            }
            text
        }
        (true, None) => fail(&format!(
            "the feature dxc needs the shaders of the dxc variant: there is no {} \
             (testing/shader_cache/shaders.py build --variant dxc --root <dir>)",
            file.display()
        )),
        (false, Some(text)) => fail(&format!(
            "RAV1G_SHADER_DIR={} has the shaders of the dxc variant ({text}): build with the feature dxc",
            shaders.display()
        )),
    }
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

    let shaders = windows.then(shader_dir);
    let shaders = shaders.as_deref();
    // What compiled the shaders, for the version text of the library.
    println!("cargo:rustc-env=RAV1G_SHADER_SET={}", shader_set(shaders));

    // The C of the decoder, one static library: the shader table, at /O1 /Ot whatever the cargo profile.
    let mut c_size = base_build(shaders);
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
