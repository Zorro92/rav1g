// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Builds the C of the decoder (the libaom-derived parser and entropy decoder of `libav1/av1`,
//! `libav1/aom*`) into static libraries, and writes the bindings. The D3D12 host is Rust (`crates/rav1g`).
//!
//! The files are those of `c_sources.txt`, with the settings the library project of the C++ decoder
//! (branch `cpp`) has for them in Release x64: `/O1 /Ot`, `/O2` for the files the list marks, no `NDEBUG`
//! (the library's asserts are compiled in), `/sdl`, and link time optimisation ([`Lto`]): `/GL` under
//! MSVC, LTO under clang-cl.
//!
//! Three C files of this crate are compiled with them: `helpers.c` (the entry points of the C interface
//! that are macros), `host_entry.c` (the functions of the host that end with the decoder's error, which C
//! has to raise) and `shader_table.c` (the bytecode of the shaders).
//!
//! The shaders are not compiled here. `RAV1G_SHADER_DIR` names the directory whose `dx/shaders/h` has the
//! bytecode headers of the FXC build (`testing/shader_cache/shaders.py build --root <dir>`; in CI the
//! `normal` output of `.github/actions/shaders`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// A source of the library and whether it is compiled with `/O2` (`c_sources.txt`).
struct Source {
    path: PathBuf,
    max_speed: bool,
}

/// Link time optimisation of the C of the decoder, which the MSBuild projects of the C++ decoder have
/// as whole program optimisation (`/GL`, `/LTCG`). Without it the entropy decoder is 7 to 12 % slower in
/// the tile threads than in those builds; the numbers are in docs/rust-port.md ("The entropy decoder with
/// link time optimisation").
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

/// The files of `c_sources.txt`: one per line, relative to `libav1`, `/O2` behind the ones compiled for
/// speed; `#` starts a comment.
fn read_sources(list: &Path, repo: &Path) -> Vec<Source> {
    let text =
        fs::read_to_string(list).unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", list.display())));
    let mut sources: Vec<Source> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let relative = words.next().unwrap_or("");
        let max_speed = match (words.next(), words.next()) {
            (None, _) => false,
            (Some("/O2"), None) => true,
            _ => fail(&format!(
                "{}, line {}: a file name, and /O2 or nothing behind it: {line}",
                list.display(),
                number + 1
            )),
        };
        let path = repo.join("libav1").join(relative);
        if !relative.ends_with(".c") || !path.is_file() {
            fail(&format!("{}, line {}: libav1/{relative} is not a C file of the tree", list.display(), number + 1));
        }
        if sources.iter().any(|source| source.path == path) {
            fail(&format!("{}, line {}: {relative} is listed twice", list.display(), number + 1));
        }
        sources.push(Source { path, max_speed });
    }
    if sources.len() < 70 {
        fail(&format!("{}: only {} sources found", list.display(), sources.len()));
    }
    sources
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
fn base_build(repo: &Path, shaders: &Path) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .warnings(false)
        // The headers of this build first: shader_table.c includes them as "dx/shaders/h/<name>.h".
        .include(shaders)
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
    if !target.ends_with("-windows-msvc") {
        fail(&format!(
            "target {target}: the decoder host is D3D12 code and builds for *-windows-msvc only \
             (on Linux: cargo xwin build --target x86_64-pc-windows-msvc)"
        ));
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // Two levels up, without canonicalize: on Windows that gives a \\?\ path, which cl does not take.
    let repo = manifest.parent().and_then(Path::parent).expect("the workspace root").to_path_buf();
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let list = manifest.join("c_sources.txt");
    println!("cargo:rerun-if-changed={}", list.display());
    println!("cargo:rerun-if-changed={}", repo.join("libav1").display());
    println!("cargo:rerun-if-changed={}", manifest.join("wrapper.h").display());
    println!("cargo:rerun-if-changed={}", manifest.join("shared.h").display());
    println!("cargo:rerun-if-changed={}", manifest.join("parser.h").display());
    println!("cargo:rerun-if-changed={}", manifest.join("shader_table.c").display());
    println!("cargo:rerun-if-changed={}", manifest.join("helpers.c").display());
    println!("cargo:rerun-if-changed={}", manifest.join("host_entry.c").display());

    let shaders = shader_dir();
    let sources = read_sources(&list, &repo);

    // Two calls, one static library each: the C files at /O1 /Ot and the C files the list marks at /O2.
    // The optimisation is the list's and does not follow the cargo profile.
    let mut c_size = base_build(&repo, &shaders);
    c_size.opt_level(1).flag_if_supported("/Ot");
    let mut c_speed = base_build(&repo, &shaders);
    c_speed.opt_level(2);
    let lto = lto();
    // What built the C, for the version text of the library (rav1g_version of rav1g.h): the logs of a
    // timing run then say which build of the entropy decoder they are of.
    println!(
        "cargo:rustc-env=RAV1G_C_BUILD={}, {}, {} runtime",
        if c_size.get_compiler().is_like_clang_cl() { "clang-cl" } else { "MSVC" },
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
    for build in [&mut c_size, &mut c_speed] {
        match lto {
            Lto::Off => {}
            Lto::Msvc => {
                build.flag("/GL");
            }
            Lto::Thin => {
                build.flag("-flto=thin");
            }
            Lto::Full => {
                build.flag("-flto");
            }
        }
        // The libraries go to the linker as they are, not through the .rlib of this crate (see Lto).
        build.cargo_metadata(false);
    }
    for source in &sources {
        if source.max_speed {
            c_speed.file(&source.path);
        } else {
            c_size.file(&source.path);
        }
    }
    // The entry points that are macros in the C headers, as functions (helpers.c).
    c_size.include(&manifest).file(manifest.join("helpers.c"));
    // The entry points of the host that can end with the decoder's error, which C has to raise
    // (host_entry.c).
    c_size.file(manifest.join("host_entry.c"));
    // The bytecode of the shaders, from the headers of RAV1G_SHADER_DIR (shader_table.c).
    c_size.file(manifest.join("shader_table.c"));
    c_size.compile("rav1g_c");
    c_speed.compile("rav1g_c_speed");
    println!("cargo:rustc-link-search=native={}", out.display());
    for library in ["rav1g_c", "rav1g_c_speed"] {
        println!("cargo:rustc-link-lib=static:-bundle={library}");
    }
    // An application that links the static library of `rav1g` itself (the UWP test player) needs the two
    // libraries of the C as well: they are not in rav1g.lib (see Lto). RAV1G_C_LIB_DIR names a directory
    // they are copied to.
    println!("cargo:rerun-if-env-changed=RAV1G_C_LIB_DIR");
    if let Some(dir) = env::var_os("RAV1G_C_LIB_DIR").filter(|dir| !dir.is_empty()).map(PathBuf::from) {
        for library in ["rav1g_c.lib", "rav1g_c_speed.lib"] {
            if let Err(error) = fs::create_dir_all(&dir).and_then(|()| fs::copy(out.join(library), dir.join(library))) {
                fail(&format!("RAV1G_C_LIB_DIR={}: cannot copy {library}: {error}", dir.display()));
            }
        }
    }
    if lto == Lto::Msvc {
        // For the tests of this crate; a build script cannot give it to the programs of other crates.
        println!("cargo:rustc-link-arg=/LTCG:incremental");
    }
    println!("cargo:rustc-link-lib=d3d12");
    println!("cargo:rustc-link-lib=dxgi");
    println!("cargo:rustc-link-lib=dxguid");

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
    bindings
        .write_to_file(out.join("bindings.rs"))
        .unwrap_or_else(|error| fail(&format!("cannot write the bindings: {error}")));

    // The layouts shared with the GPU and between the entropy decoder and the host (shared.h): types only.
    let shared = bindgen::Builder::default()
        .header(manifest.join("shared.h").to_string_lossy())
        .clang_arg(format!("-I{}", repo.join("libav1").display()))
        .clang_arg(format!("-I{}", repo.join("libav1").join("build").display()))
        .allowlist_type(SHARED_TYPES.join("|"))
        .prepend_enum_name(false)
        .layout_tests(true)
        .derive_default(true)
        .generate_comments(false)
        .generate()
        .unwrap_or_else(|error| fail(&format!("bindgen (shared.h): {error}")));
    shared
        .write_to_file(out.join("shared.rs"))
        .unwrap_or_else(|error| fail(&format!("cannot write the bindings of shared.h: {error}")));

    // The entropy decoder as the Rust host sees it (parser.h): the structs the host reads and fills in
    // place, the enums and limits of the bitstream, and a few tables. Types and constants only; the
    // functions the Rust calls are declared where they are called.
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
        .blocklist_function(".*")
        .prepend_enum_name(false)
        .layout_tests(true)
        .derive_default(false)
        .generate_comments(false)
        .generate()
        .unwrap_or_else(|error| fail(&format!("bindgen (parser.h): {error}")));
    parser
        .write_to_file(out.join("parser.rs"))
        .unwrap_or_else(|error| fail(&format!("cannot write the bindings of parser.h: {error}")));

    // The layout probe: for every struct of the two lists, its size and alignment and the offset and size
    // of each of its fields, once as the C compiler of this build has them (layout_probe.c, compiled into
    // the library) and once as the bindings have them (layout_table.rs, compiled into the tests of this
    // crate, which compare the two). The fields are read from the bindings, so none is left out.
    let mut probes = read_probes(&bindings.to_string(), INTERFACE_TYPES, "");
    probes.extend(read_probes(&shared.to_string(), SHARED_TYPES, "shared::"));
    probes.extend(read_probes(&parser.to_string(), PARSER_PROBED, "parser::"));
    let probe_c = out.join("layout_probe.c");
    fs::write(&probe_c, probe_source_c(&probes))
        .unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", probe_c.display())));
    fs::write(out.join("layout_table.rs"), probe_source_rust(&probes, "rust_layouts"))
        .unwrap_or_else(|error| fail(&format!("cannot write layout_table.rs: {error}")));
    let mut probe = base_build(&repo, &shaders);
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

/// The structs of `av1/common/blockd.h` and `mv.h` that cross a boundary (shared.h): the mode info record,
/// which the shaders read as `MB_MODE_INFO` of `dx/shaders/mode_info.h`, the block records of the
/// prediction and transform stages, and the tile's cursors into the buffers of a pipeline slot, which the
/// entropy decoder and the host both use.
const SHARED_TYPES: &[&str] = &[
    "MB_MODE_INFO",
    "PALETTE_MODE_INFO",
    "FILTER_INTRA_MODE_INFO",
    "INTERINTER_COMPOUND_DATA",
    "WarpedMotionParams",
    "int_mv",
    "MV",
    "PredictionBlock",
    "InterWarpBlock",
    "PredictionBlockSortingInfo",
    "tx_block_info_gpu",
    "tx_block_info",
    "av1_tile_data",
];

/// Types of the entropy decoder and of its interface that the Rust host uses (parser.h); what they hold
/// comes along.
const PARSER_TYPES: &[&str] = &[
    "AV1Decoder",
    "AV1_COMMON",
    "MACROBLOCKD",
    "ThreadData",
    "TileInfo",
    "Av1ColorMapParam",
    "BufferPool",
    "RefCntBuffer",
    "YV12_BUFFER_CONFIG",
    "HwFrameBuffer",
    "HwOutputImage",
    "av1_tile_data",
    "tx_block_info_gpu",
    "MV_REF",
    "TPL_MV_REF",
    "RestorationUnitInfo",
    "AV1LrStruct",
    "OrderHintInfo",
    "WarpedMotionParams",
    "scale_factors",
    "int_mv",
    "aom_codec_dec_cfg_t",
    "aom_image_t",
    "av1_decoded_frame_buffer_t",
    "av1_perf_.*",
    "av1_stage_dump.*",
];

/// Tables and limits of the entropy decoder that the Rust host reads: `extern` in its headers, so the
/// bindings declare the C symbol; and constants that are macros or enumerators outside `enums.h`.
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

/// The structs of the entropy decoder that the Rust host reads or writes in place: the layout probe
/// holds their bindings against the C compiler of the build.
const PARSER_PROBED: &[&str] = &[
    "AV1Decoder",
    "AV1_COMMON",
    "MACROBLOCKD",
    "ThreadData",
    "TileInfo",
    "Av1ColorMapParam",
    "BufferPool",
    "RefCntBuffer",
    "YV12_BUFFER_CONFIG",
    "HwFrameBuffer",
    "HwOutputImage",
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

/// Inline functions of the entropy decoder's headers that the Rust host modules have a port of, as
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
         #include <stddef.h>\n#include <stdint.h>\n\n#include \"wrapper.h\"\n#include \"shared.h\"\n\
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
