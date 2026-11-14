// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Writes the bindings of the decoder to the headers of `libav1`, and builds the little C the decoder
//! has: the shader table.
//!
//! No file of `libav1` is compiled. The decoder's parser is rav1d's parse pass (`crates/rav1g-parse`,
//! docs/phase2-rav1d.md), its host is Rust (`crates/rav1g`), and what the host still needs of libaom's
//! common code is Rust there too (`host/libaom.rs`). What stays of `libav1` for the decoder is its
//! headers: the host fills and reads the structs of the frame header and of the blocks in the layouts
//! the shaders and the sink were written for, and bindgen reads those from the headers (`wrapper.h`,
//! `parser.h`). The records the shaders read are not among them: they are Rust with their layouts
//! (`src/records.rs`), and the layout probe below holds them against the C structs.
//!
//! **For a Windows target (the decoder)** one C file is compiled into `rav1g_c.lib`: `shader_table.c`,
//! the bytecode of the shaders. The shaders are not compiled here. `RAV1G_SHADER_DIR` names the directory
//! whose `dx/shaders/h` has the bytecode headers of the FXC build (`testing/shader_cache/shaders.py build
//! --root <dir>`; in CI the `normal` output of `.github/actions/shaders`).
//!
//! **For a target that is not Windows** the crate is the bindings and the layout probe alone: the D3D12
//! host does not build there. `crates/rav1g-parse` and `tools/rav1g-parse-check` use the bindings on
//! Linux (docs/phase2-rav1d.md, step 2).

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

/// Writes bindings. The Rust of the host is written against the bindings of the Windows targets, where
/// the type behind a C enum is `int`. Elsewhere the compiler takes `unsigned int` for an enum without
/// negative values; the bindings are written with `int` there too, so that the same Rust compiles (the
/// size is the same, and no enumerator of the decoder is out of the range of `int`).
fn write_bindings(bindings: &bindgen::Bindings, path: &Path, windows: bool) {
    let mut text = bindings.to_string();
    if !windows {
        // The types of the enumerators: `pub const NAME: TYPE = value;` with a type that is an enum's.
        let mut enums: Vec<String> = Vec::new();
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("pub const ") else { continue };
            let Some((_, rest)) = rest.split_once(": ") else { continue };
            let Some((name, _)) = rest.split_once(" = ") else { continue };
            if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !enums.iter().any(|known| known == name) {
                enums.push(String::from(name));
            }
        }
        for name in enums {
            text = text.replace(
                &format!("pub type {name} = ::std::os::raw::c_uint;"),
                &format!("pub type {name} = ::std::os::raw::c_int;"),
            );
        }
    }
    fs::write(path, text).unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", path.display())));
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

/// A compiler call with the settings every file of the library has.
fn base_build(repo: &Path, shaders: Option<&Path>) -> cc::Build {
    let mut build = cc::Build::new();
    build.warnings(false);
    if let Some(shaders) = shaders {
        // The headers of this build first: shader_table.c includes them as "dx/shaders/h/<name>.h".
        build.include(shaders);
    }
    build
        .include(repo.join("libav1"))
        .include(repo.join("libav1").join("build"))
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
    clang_cl_flags(&mut build);
    build
}

/// clang-cl (the cross build from Linux, `cargo xwin`) rejects what MSVC compiles with a warning. The
/// sources stay as they are; this is the diagnostic that is an error in clang by default:
/// `-Wincompatible-function-pointer-types`: `aom_util/aom_thread.h` hands a
/// `unsigned int __stdcall (void *)` to `CreateThread`, which wants a `DWORD` result. Both are 32 bits in
/// the same register.
fn clang_cl_flags(build: &mut cc::Build) {
    if build.get_compiler().is_like_clang_cl() {
        build.flag("-Wno-error=incompatible-function-pointer-types");
    }
}

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    // The decoder is for Windows. Elsewhere the crate is the bindings and the layout probe (see the top).
    let windows = target.ends_with("-windows-msvc");
    if !windows && env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        fail(&format!("target {target}: of the Windows targets the decoder builds for *-windows-msvc only"));
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // Two levels up, without canonicalize: on Windows that gives a \\?\ path, which cl does not take.
    let repo = manifest.parent().and_then(Path::parent).expect("the workspace root").to_path_buf();
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rerun-if-changed={}", repo.join("libav1").display());
    println!("cargo:rerun-if-changed={}", manifest.join("wrapper.h").display());
    println!("cargo:rerun-if-changed={}", manifest.join("src").join("records.rs").display());
    println!("cargo:rerun-if-changed={}", manifest.join("src").join("host.rs").display());
    println!("cargo:rerun-if-changed={}", manifest.join("parser.h").display());
    println!("cargo:rerun-if-changed={}", manifest.join("shader_table.c").display());

    let shaders = windows.then(shader_dir);
    let shaders = shaders.as_deref();

    // The C of the decoder, one static library: the shader table, at /O1 /Ot whatever the cargo profile.
    let mut c_size = base_build(&repo, shaders);
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

    let bindings = bindgen::Builder::default()
        .header(manifest.join("wrapper.h").to_string_lossy())
        .clang_arg(format!("-I{}", repo.join("libav1").display()))
        .clang_arg(format!("-I{}", repo.join("libav1").join("build").display()))
        .allowlist_function("aom_codec_.*|av1_.*|rav1g_.*")
        .allowlist_type("aom_.*|av1_.*|frame_buffer_type|cfg_options_t")
        .allowlist_var("AOM_.*|AV1_.*|AV1D_.*")
        .prepend_enum_name(false)
        .layout_tests(true)
        .derive_default(true)
        .generate_comments(false)
        .generate()
        .unwrap_or_else(|error| fail(&format!("bindgen: {error}")));
    write_bindings(&bindings, &out.join("bindings.rs"), windows);

    // The enums and limits of the bitstream and the constants of the test instrumentation (parser.h).
    let parser = bindgen::Builder::default()
        .header(manifest.join("parser.h").to_string_lossy())
        .clang_arg(format!("-I{}", repo.join("libav1").display()))
        .clang_arg(format!("-I{}", repo.join("libav1").join("build").display()))
        // The enums and limits of the bitstream (block sizes, transform sizes, ...), and the constants of
        // the timing run and of the test switches.
        .allowlist_file(r".*[/\\]av1[/\\]common[/\\]enums\.h")
        .allowlist_file(r".*[/\\]dx[/\\]av1_perf\.h")
        .allowlist_type(PARSER_TYPES.join("|"))
        .allowlist_var(PARSER_VARS.join("|"))
        // The records shared with the shaders are not bindings any more: `src/records.rs` has them.
        .blocklist_type(RECORD_NAMES.join("|"))
        .raw_line("pub use crate::records::*;")
        // Nor is the state of the host (`src/host.rs`), and the types of the C interface and of the test
        // instrumentation are those of the root.
        .blocklist_type(type_names(&manifest.join("src").join("host.rs")).join("|"))
        .raw_line("pub use crate::host::*;")
        .blocklist_type("aom_.*|av1_.*|cfg_options.*|frame_buffer_type")
        .raw_line("pub use crate::*;")
        .blocklist_function(".*")
        .prepend_enum_name(false)
        .layout_tests(true)
        .derive_default(false)
        .generate_comments(false)
        .generate()
        .unwrap_or_else(|error| fail(&format!("bindgen (parser.h): {error}")));
    write_bindings(&parser, &out.join("parser.rs"), windows);

    // The layout probe: for every struct of the two lists, its size and alignment and the offset and size
    // of each of its fields, once as the C compiler of this build has them (layout_probe.c, compiled into
    // the library) and once as the bindings have them (layout_table.rs, compiled into the tests of this
    // crate, which compare the two). The fields are read from the bindings, so none is left out.
    let mut probes = read_probes(&bindings.to_string(), INTERFACE_TYPES, "");
    let records = manifest.join("src").join("records.rs");
    let records = fs::read_to_string(&records)
        .unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", records.display())));
    probes.extend(read_probes(&records, RECORD_TYPES, "records::"));
    let probe_c = out.join("layout_probe.c");
    fs::write(&probe_c, probe_source_c(&probes))
        .unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", probe_c.display())));
    fs::write(out.join("layout_table.rs"), probe_source_rust(&probes, "rust_layouts"))
        .unwrap_or_else(|error| fail(&format!("cannot write layout_table.rs: {error}")));
    let mut probe = base_build(&repo, shaders);
    probe.opt_level(1).include(&manifest).file(&probe_c).compile("rav1g_layout_probe");
}

/// The structs of the C interface that `rav1g` and `rav1g-dec` fill or read (wrapper.h).
const INTERFACE_TYPES: &[&str] = &[
    "aom_codec_ctx_t",
    "aom_codec_dec_cfg_t",
    "aom_codec_stream_info_t",
    "aom_image_t",
    "av1_decoded_frame_buffer_t",
    "av1_out_buffers_cb_t",
    "av1_perf_frame",
    "av1_perf_inter_detail",
    "av1_stage_dump",
    "av1_stage_dump_plane",
];

/// The records of `src/records.rs`, which are written in Rust with their layouts and not read from
/// `av1/common/blockd.h` and `mv.h` any more: the mode info record, which the shaders read as
/// `MB_MODE_INFO` of `dx/shaders/mode_info.h`, the record of a transform block, and the tile's cursors
/// into the buffers of a pipeline slot. The layout probe holds them against the C structs they were.
const RECORD_TYPES: &[&str] = &[
    "MB_MODE_INFO",
    "PALETTE_MODE_INFO",
    "FILTER_INTRA_MODE_INFO",
    "INTERINTER_COMPOUND_DATA",
    "WarpedMotionParams",
    "int_mv",
    "MV",
    "tx_block_info_gpu",
    "av1_tile_data",
];

/// Every type name of `src/records.rs`: the bindings of the parser's structs (parser.h) take these from
/// there.
const RECORD_NAMES: &[&str] = &[
    "MB_MODE_INFO",
    "PALETTE_MODE_INFO",
    "FILTER_INTRA_MODE_INFO",
    "INTERINTER_COMPOUND_DATA",
    "WarpedMotionParams",
    "int_mv",
    "MV",
    "mv",
    "tx_block_info_gpu",
    "av1_tile_data",
    "tran_low_t",
    "BLOCK_SIZE",
    "PARTITION_TYPE",
    "TX_SIZE",
    "PREDICTION_MODE",
    "UV_PREDICTION_MODE",
    "MOTION_MODE",
    "INTERINTRA_MODE",
    "COMPOUND_TYPE",
    "FILTER_INTRA_MODE",
    "MV_REFERENCE_FRAME",
    "TransformationType",
    "InterpFilters",
    "DIFFWTD_MASK_TYPE",
];

/// Integer types of libaom's headers outside `enums.h` that the Rust host and `src/host.rs` name.
const PARSER_TYPES: &[&str] = &["FRAME_TYPE", "RestorationType", "InterpFilter"];

/// The names of the types a Rust file of this crate defines (`pub struct`, `pub union`, `pub type`).
fn type_names(path: &Path) -> Vec<String> {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", path.display())));
    let mut names = Vec::new();
    for line in text.lines() {
        for kind in ["pub struct ", "pub union ", "pub type "] {
            if let Some(rest) = line.strip_prefix(kind) {
                names.push(
                    rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap_or("").to_string(),
                );
            }
        }
    }
    names
}

/// Tables and limits of libaom's headers that the Rust host names: the tables are `extern` there, so the
/// bindings declare a C symbol, which nothing defines and nothing may use (the host has the tables in
/// Rust, `host/libaom.rs`); and constants that are macros or enumerators outside `enums.h`.
const PARSER_VARS: &[&str] = &[
    "warped_filter",
    "wedge_params_lookup",
    "wedge_mask_buf",
    "ii_weights1d",
    "ii_size_scales",
    "av1_idct_scans",
    "sgr_params",
    "av1_filter_intra_taps",
    "REF_NO_SCALE",
    "SCALE_SUBPEL_BITS",
    "INVALID_IDX",
    "MAX_FRAME_DISTANCE",
    "AOM_CICP_MC_IDENTITY",
    "RESTORE_.*",
    "COMPOUND_.*",
    "fbt.*",
    "BILINEAR",
    "OBMC_CAUSAL",
    "WARPED_CAUSAL",
    "AV1_LOW_MEMORY_.*",
    "AV1_MEMORY_GUARD_.*",
];

/// Tables that are `static` in a header of the decoder and that the Rust host modules have a copy of
/// (`crates/rav1g/src/host/tables.rs`): name and the expression of its bytes. The C probe exports them so
/// that a test can hold the copy against the header ([`table_source`]).
const C_TABLES: &[&str] = &[
    "block_size_wide",
    "block_size_high",
    "ss_size_lookup",
    "tx_size_wide",
    "tx_size_high",
    "mi_size_wide_log2",
    "mi_size_high_log2",
    "max_txsize_rect_lookup",
    "interintra_to_intra_mode",
    "sm_weight_arrays",
    "tx_size_wide_log2",
    "tx_size_high_log2",
    "max_neighbor_obmc",
    "quant_dist_weight",
    "quant_dist_lookup_table",
    "av1_sub_pel_filters_4",
    "av1_sub_pel_filters_4smooth",
    "av1_bilinear_filters",
    "av1_sub_pel_filters_8",
    "av1_sub_pel_filters_8smooth",
    "av1_sub_pel_filters_8sharp",
];

/// Inline functions of libaom's headers that the Rust host modules have a port of, as
/// functions of the C probe: the tests of the ports call them.
const C_FUNCTION_PROBES: &str = r#"
#include <string.h>

int rav1g_probe_scale_chroma_bsize(int bsize, int ss_x, int ss_y) {
  return scale_chroma_bsize((BLOCK_SIZE)bsize, ss_x, ss_y);
}

int rav1g_probe_max_uv_txsize(int bsize, int ss_x, int ss_y) {
  return av1_get_max_uv_txsize((BLOCK_SIZE)bsize, ss_x, ss_y);
}

int rav1g_probe_is_interintra_pred(int sb_type, int mode, int ref0, int ref1) {
  MB_MODE_INFO mi;
  memset(&mi, 0, sizeof(mi));
  mi.sb_type = (BLOCK_SIZE)sb_type;
  mi.mode = (PREDICTION_MODE)mode;
  mi.ref_frame[0] = (MV_REFERENCE_FRAME)ref0;
  mi.ref_frame[1] = (MV_REFERENCE_FRAME)ref1;
  return is_interintra_pred(&mi);
}

int rav1g_probe_extract_interp_filter(unsigned int filters, int x_filter) {
  return av1_extract_interp_filter(filters, x_filter);
}
"#;

/// A struct or union to probe: its C name, the path of its bindings and its fields.
struct Probe {
    c_type: String,
    rust_type: String,
    is_union: bool,
    /// The names of the fields in the bindings.
    fields: Vec<String>,
}

/// The fields of the listed types, read from the text of the bindings: `pub struct <name> {`, then one
/// `pub <field>: <type>,` per field. A name that is a type alias (`pub type <name> = <other>;`) is followed.
fn read_probes(bindings: &str, types: &[&str], module: &str) -> Vec<Probe> {
    let lines: Vec<&str> = bindings.lines().collect();
    let mut probes = Vec::new();
    for &c_type in types {
        let mut name = c_type.to_string();
        let found = loop {
            let header = |kind: &str| format!("pub {kind} {name} {{");
            if let Some(start) = lines.iter().position(|line| *line == header("struct") || *line == header("union")) {
                break Some((start, lines[start].starts_with("pub union")));
            }
            let alias = format!("pub type {name} = ");
            match lines.iter().find_map(|line| line.strip_prefix(alias.as_str())) {
                Some(target) => name = target.trim_end_matches(';').to_string(),
                None => break None,
            }
        };
        let Some((start, is_union)) = found else {
            fail(&format!("the bindings have no struct or union {c_type}"));
        };
        let mut fields = Vec::new();
        for line in &lines[start + 1..] {
            if *line == "}" {
                break;
            }
            let Some(rest) = line.strip_prefix("    pub ") else {
                continue;
            };
            let Some((field, _)) = rest.split_once(':') else {
                continue;
            };
            // Bit fields and anonymous members have no name C could take an offset of; the size of the
            // struct still covers them.
            if field.starts_with("_bitfield") || field.starts_with("__bindgen") {
                continue;
            }
            fields.push(field.to_string());
        }
        if fields.is_empty() {
            fail(&format!("the bindings of {c_type} have no fields"));
        }
        probes.push(Probe { c_type: c_type.to_string(), rust_type: format!("{module}{c_type}"), is_union, fields });
    }
    probes
}

/// The C name of a field of the bindings: bindgen puts an underscore behind a name that is a Rust keyword.
fn c_field(field: &str) -> &str {
    const KEYWORDS: &[&str] = &["type", "priv", "impl", "ref", "match", "fn", "in", "use", "mod", "move", "loop"];
    match field.strip_suffix('_') {
        Some(stem) if KEYWORDS.contains(&stem) => stem,
        _ => field,
    }
}

fn probe_source_c(probes: &[Probe]) -> String {
    let mut text = String::from(
        "/* Written by build.rs: the layouts as this compiler has them. */\n\
         #include <stddef.h>\n#include <stdint.h>\n\n#include \"wrapper.h\"\n\
         #include \"parser.h\"\n\
         #include \"av1/common/onyxc_int.h\"\n#include \"av1/common/filter.h\"\n\
         #include \"av1/common/reconintra.h\"\n#include \"aom_dsp/intrapred_common.h\"\n\n\
         typedef struct {\n  const char *name;\n  uint32_t size;\n  uint32_t offset;\n  uint32_t align;\n\
         } rav1g_layout_entry;\n\nstatic const rav1g_layout_entry rav1g_layout_entries[] = {\n",
    );
    for probe in probes {
        let t = &probe.c_type;
        text.push_str(&format!("  {{ \"{t}\", (uint32_t)sizeof({t}), 0, (uint32_t)__alignof({t}) }},\n"));
        for field in &probe.fields {
            let f = c_field(field);
            text.push_str(&format!(
                "  {{ \"{t}.{f}\", (uint32_t)sizeof((({t} *)0)->{f}), (uint32_t)offsetof({t}, {f}), 0 }},\n"
            ));
        }
    }
    text.push_str(
        "};\n\nconst rav1g_layout_entry *rav1g_layout_table(uint32_t *count) {\n\
         \x20 *count = (uint32_t)(sizeof(rav1g_layout_entries) / sizeof(rav1g_layout_entries[0]));\n\
         \x20 return rav1g_layout_entries;\n}\n",
    );
    text.push_str(&table_source(C_TABLES, "rav1g_c_tables"));
    text.push_str(C_FUNCTION_PROBES);
    text
}

/// The tables of a list as (name, address, bytes), behind a function that returns them.
fn table_source(tables: &[&str], function: &str) -> String {
    let mut text = format!(
        "\ntypedef struct {{\n  const char *name;\n  const void *data;\n  uint32_t size;\n}} {function}_entry;\n\n\
         static const {function}_entry {function}_entries[] = {{\n"
    );
    for table in tables {
        text.push_str(&format!("  {{ \"{table}\", {table}, (uint32_t)sizeof({table}) }},\n"));
    }
    text.push_str(&format!(
        "}};\n\nconst void *{function}(uint32_t index, const char **name, uint32_t *size) {{\n\
         \x20 if (index >= sizeof({function}_entries) / sizeof({function}_entries[0])) return 0;\n\
         \x20 *name = {function}_entries[index].name;\n\
         \x20 *size = {function}_entries[index].size;\n\
         \x20 return {function}_entries[index].data;\n}}\n"
    ));
    text
}

fn probe_source_rust(probes: &[Probe], function: &str) -> String {
    let mut text = format!(
        "// Written by build.rs: the layouts as the bindings have them (name, size, offset, alignment).\n\
         pub fn {function}() -> Vec<(&'static str, usize, usize, usize)> {{\n    vec![\n"
    );
    for probe in probes {
        let (c, r) = (&probe.c_type, &probe.rust_type);
        text.push_str(&format!("        (\"{c}\", size_of::<{r}>(), 0, align_of::<{r}>()),\n"));
        for field in &probe.fields {
            let f = c_field(field);
            let access = if probe.is_union { format!("unsafe {{ &s.{field} }}") } else { format!("&s.{field}") };
            text.push_str(&format!(
                "        (\"{c}.{f}\", field_size(|s: &{r}| {access}), ::core::mem::offset_of!({r}, {field}), 0),\n"
            ));
        }
    }
    text.push_str("    ]\n}\n");
    text
}
