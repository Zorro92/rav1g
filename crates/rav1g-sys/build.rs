// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Builds the decoder's C and C++ sources (the libaom-derived entropy decoder of `libav1/av1`, `libav1/aom*`
//! and the D3D12 host of `libav1/dx`) into a static library, and writes the bindings.
//!
//! The files and their settings are those of `libav1/build/libav1.vcxproj` in Release x64, read from
//! `testing/warp_decoder/libav1_sources.props` (written from that project by `sync_sources.py`; CI checks
//! that it is up to date): `/O1 /Ot`, `/O2` for the files the project marks, no `NDEBUG` (the library's
//! asserts are compiled in), `/sdl`, `/EHsc`. Whole program optimisation (`/GL`) is not used: the objects
//! go through rustc's archives, and the Linux build links with lld.
//!
//! The shaders are not compiled here. `RAV1G_SHADER_DIR` names the directory whose `dx/shaders/h` has the
//! bytecode headers of the FXC build (`testing/shader_cache/shaders.py build --root <dir>`, or what a
//! build of `libav1.vcxproj` left in `libav1`), the directory MSBuild gets as `/p:Rav1gPrebuiltShaders`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// A source of the library and whether the project compiles it with `/O2` (`MaxSpeed`).
struct Source {
    path: PathBuf,
    max_speed: bool,
}

fn fail(message: &str) -> ! {
    eprintln!("rav1g-sys: {message}");
    std::process::exit(1);
}

/// The `ClCompile` items of `libav1_sources.props`: one item per line, an optional `Optimization` line
/// behind it.
fn read_sources(props: &Path, repo: &Path) -> Vec<Source> {
    let text =
        fs::read_to_string(props).unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", props.display())));
    let mut sources: Vec<Source> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("<ClCompile Include=\"$(RepoRoot)") {
            let Some(end) = rest.find('"') else {
                fail(&format!("{}: no closing quote in {line}", props.display()));
            };
            let relative = rest[..end].replace('\\', "/");
            let path = repo.join(&relative);
            if !path.is_file() {
                fail(&format!("{} lists {relative}, which is not there", props.display()));
            }
            sources.push(Source { path, max_speed: false });
        } else if line.starts_with("<Optimization") {
            let Some(last) = sources.last_mut() else {
                fail(&format!("{}: an Optimization line before any source", props.display()));
            };
            if !line.contains(">MaxSpeed<") {
                fail(&format!("{}: a setting this build does not carry over: {line}", props.display()));
            }
            last.max_speed = true;
        }
    }
    if sources.len() < 70 {
        fail(&format!("{}: only {} sources found", props.display(), sources.len()));
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
fn base_build(repo: &Path, shaders: &Path, cpp: bool) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .cpp(cpp)
        .warnings(false)
        // The headers of this build first: av1_compute.cpp includes them as "dx/shaders/h/<name>.h".
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
    if cpp {
        // The exception model of the MSBuild projects; cc does not choose one.
        build.flag_if_supported("/EHsc");
    }
    if env::var_os("CARGO_FEATURE_UAVSYNC").is_some() {
        build.define("RAV1G_UAVSYNC", None);
    }
    clang_cl_flags(&mut build);
    build
}

/// clang-cl (the cross build from Linux, `cargo xwin`) rejects what MSVC compiles with a warning. The
/// sources stay as they are; these are the diagnostics that are errors in clang by default:
///
/// - `-Wincompatible-function-pointer-types`: `aom_util/aom_thread.h` hands a
///   `unsigned int __stdcall (void *)` to `CreateThread`, which wants a `DWORD` result. Both are 32 bits in
///   the same register.
/// - `-Waddress-of-temporary`: the host passes `&CD3DX12_RESOURCE_BARRIER::Transition(...)`,
///   `&CD3DX12_RANGE(...)` and the like straight to a D3D12 call (an MSVC extension). The temporary lives
///   to the end of the full expression, which is the call.
fn clang_cl_flags(build: &mut cc::Build) {
    if build.get_compiler().is_like_clang_cl() {
        build.flag("-Wno-error=incompatible-function-pointer-types");
        build.flag("-Wno-error=address-of-temporary");
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
    let props = repo.join("testing").join("warp_decoder").join("libav1_sources.props");
    println!("cargo:rerun-if-changed={}", props.display());
    println!("cargo:rerun-if-changed={}", repo.join("libav1").display());
    println!("cargo:rerun-if-changed={}", manifest.join("wrapper.h").display());
    println!("cargo:rerun-if-changed={}", manifest.join("shared.h").display());
    println!("cargo:rerun-if-changed={}", manifest.join("helpers.c").display());

    let shaders = shader_dir();
    let sources = read_sources(&props, &repo);

    // Three calls, one static library each: the C files at /O1 /Ot, the C files the project marks at /O2,
    // and the C++ host at /O1 /Ot. The optimisation is the project's and does not follow the cargo profile.
    let mut c_size = base_build(&repo, &shaders, false);
    c_size.opt_level(1).flag_if_supported("/Ot");
    let mut c_speed = base_build(&repo, &shaders, false);
    c_speed.opt_level(2);
    let mut cpp = base_build(&repo, &shaders, true);
    cpp.opt_level(1).flag_if_supported("/Ot");
    for source in &sources {
        let is_cpp = source.path.extension().is_some_and(|extension| extension != "c");
        match (is_cpp, source.max_speed) {
            (true, false) => {
                cpp.file(&source.path);
            }
            (true, true) => fail(&format!("{}: a C++ file at /O2 is not carried over", source.path.display())),
            (false, false) => {
                c_size.file(&source.path);
            }
            (false, true) => {
                c_speed.file(&source.path);
            }
        }
    }
    // The entry points that are macros in the C headers, as functions (helpers.c).
    c_size.include(&manifest).file(manifest.join("helpers.c"));
    c_size.compile("rav1g_c");
    c_speed.compile("rav1g_c_speed");
    cpp.compile("rav1g_dx");
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

    // The layout probe: for every struct of the two lists, its size and alignment and the offset and size
    // of each of its fields, once as the C compiler of this build has them (layout_probe.c, compiled into
    // the library) and once as the bindings have them (layout_table.rs, compiled into the tests of this
    // crate, which compare the two). The fields are read from the bindings, so none is left out.
    let mut probes = read_probes(&bindings.to_string(), INTERFACE_TYPES, "");
    probes.extend(read_probes(&shared.to_string(), SHARED_TYPES, "shared::"));
    let probe_c = out.join("layout_probe.c");
    fs::write(&probe_c, probe_source_c(&probes))
        .unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", probe_c.display())));
    fs::write(out.join("layout_table.rs"), probe_source_rust(&probes))
        .unwrap_or_else(|error| fail(&format!("cannot write layout_table.rs: {error}")));
    let mut probe = base_build(&repo, &shaders, false);
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
         #include <stddef.h>\n#include <stdint.h>\n\n#include \"wrapper.h\"\n#include \"shared.h\"\n\n\
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
    text
}

fn probe_source_rust(probes: &[Probe]) -> String {
    let mut text = String::from(
        "// Written by build.rs: the layouts as the bindings have them (name, size, offset, alignment).\n\
         pub fn rust_layouts() -> Vec<(&'static str, usize, usize, usize)> {\n    vec![\n",
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
