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
//! The C++ files of the host that Rust replaces (`crates/rav1g/src/host`, step 3 of `docs/rust-port.md`) are
//! left out: [`REPLACED_BY_RUST`]. The C++ that remains calls the Rust through the files of `glue/`, which
//! define the C++ functions of a replaced file as calls of `extern "C"` functions of the `rav1g` crate.
//! The sources of `libav1/dx` stay as they are: the MSBuild projects (the test player, `warp_decoder`)
//! compile all of them until step 5.
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
    /// Rust has the functions of this file ([`REPLACED_BY_RUST`]).
    replaced: bool,
}

/// The C++ files of `libav1/dx` whose functions the `rav1g` crate has in Rust (`crates/rav1g/src/host`).
/// This build does not compile them. `libav1_sources.props` still lists them, for the MSBuild projects.
const REPLACED_BY_RUST: &[&str] = &[
    "libav1/dx/av1_compute.cpp",
    "libav1/dx/av1_memory.cpp",
    "libav1/dx/av1_thread.cpp",
    "libav1/dx/vp9dx_linear_allocator.cpp",
];

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
            sources.push(Source { path, max_speed: false, replaced: REPLACED_BY_RUST.contains(&relative.as_str()) });
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
    for replaced in REPLACED_BY_RUST {
        if !sources.iter().any(|source| source.replaced && source.path.ends_with(replaced)) {
            fail(&format!("{} does not list {replaced}, which this build replaces", props.display()));
        }
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
    println!("cargo:rerun-if-changed={}", manifest.join("host.hpp").display());
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
    for source in sources.iter().filter(|source| !source.replaced) {
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
    // The C++ side of the functions Rust has taken over (glue/): compiled with the host, as the files they
    // stand in for were.
    let glue = manifest.join("glue");
    println!("cargo:rerun-if-changed={}", glue.display());
    let mut glue_files: Vec<PathBuf> = fs::read_dir(&glue)
        .unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", glue.display())))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "cpp"))
        .collect();
    glue_files.sort();
    cpp.files(&glue_files);
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

    // The C++ host as the Rust host modules see it (host.hpp): the structs of libav1/dx and what they hold
    // of the entropy decoder, read as C++ with the settings of the build. Types and constants only; the
    // functions the Rust calls are declared where they are called.
    let host = bindgen::Builder::default()
        .header(manifest.join("host.hpp").to_string_lossy())
        .clang_args(["-x", "c++", "-std=c++17", "-fms-extensions", "-fms-compatibility"])
        .clang_args(["-D_UNICODE", "-DUNICODE", "-D_CONSOLE", "-D_CRT_SECURE_NO_WARNINGS"])
        // libclang may be older than the C++ standard library of the MSVC runtime asks for; nothing of
        // that library is bound.
        .clang_arg("-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH")
        .clang_arg(format!("-I{}", repo.join("libav1").display()))
        .clang_arg(format!("-I{}", repo.join("libav1").join("build").display()))
        .allowlist_file(r".*[/\\]libav1[/\\]dx[/\\][A-Za-z0-9_]+\.h")
        // The enums and limits of the bitstream (block sizes, transform sizes, ...).
        .allowlist_file(r".*[/\\]av1[/\\]common[/\\]enums\.h")
        .allowlist_type(HOST_TYPES.join("|"))
        .allowlist_var(HOST_VARS.join("|"))
        // COM pointers and interfaces are handed to the `windows` crate as raw pointers.
        .opaque_type("Microsoft::WRL::.*|std::.*")
        .blocklist_type("ID3D12.*|IDXGI.*|IUnknown|ID3D10Blob")
        .raw_line("pub type ID3D12Device = ::std::os::raw::c_void;")
        .raw_line("pub type ID3D12Resource = ::std::os::raw::c_void;")
        .raw_line("pub type ID3D12RootSignature = ::std::os::raw::c_void;")
        // No functions: bindgen declares C++ functions by their mangled names, and the Rust host modules
        // declare the C functions they call themselves.
        .blocklist_function(".*")
        .ignore_methods()
        .prepend_enum_name(false)
        .layout_tests(true)
        .derive_default(false)
        .generate_comments(false)
        .generate()
        .unwrap_or_else(|error| fail(&format!("bindgen (host.hpp): {error}")));
    host.write_to_file(out.join("host.rs"))
        .unwrap_or_else(|error| fail(&format!("cannot write the bindings of host.hpp: {error}")));

    // The same probe for the structs of the C++ host: host_layout_probe.cpp, compiled by the C++ compiler
    // of the build, against the table of the bindings of host.hpp.
    let host_probes = read_probes(&host.to_string(), HOST_PROBED, "host::");
    let host_probe_cpp = out.join("host_layout_probe.cpp");
    fs::write(&host_probe_cpp, probe_source_cpp(&host_probes))
        .unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", host_probe_cpp.display())));
    fs::write(out.join("host_layout_table.rs"), probe_source_rust(&host_probes, "host_rust_layouts"))
        .unwrap_or_else(|error| fail(&format!("cannot write host_layout_table.rs: {error}")));
    let mut host_probe = base_build(&repo, &shaders, true);
    host_probe.opt_level(1).include(&manifest).file(&host_probe_cpp);
    if host_probe.get_compiler().is_like_clang_cl() {
        // offsetof of a struct with COM pointers or a base class: defined by both compilers, a warning in clang.
        host_probe.flag("-Wno-invalid-offsetof");
    }
    host_probe.compile("rav1g_host_layout_probe");

    // The layout probe: for every struct of the two lists, its size and alignment and the offset and size
    // of each of its fields, once as the C compiler of this build has them (layout_probe.c, compiled into
    // the library) and once as the bindings have them (layout_table.rs, compiled into the tests of this
    // crate, which compare the two). The fields are read from the bindings, so none is left out.
    let mut probes = read_probes(&bindings.to_string(), INTERFACE_TYPES, "");
    probes.extend(read_probes(&shared.to_string(), SHARED_TYPES, "shared::"));
    let probe_c = out.join("layout_probe.c");
    fs::write(&probe_c, probe_source_c(&probes))
        .unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", probe_c.display())));
    fs::write(out.join("layout_table.rs"), probe_source_rust(&probes, "rust_layouts"))
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

/// Types of the entropy decoder that the Rust host modules use and that no struct of `libav1/dx` holds
/// (those come with the headers of `libav1/dx`, which are bound whole).
const HOST_TYPES: &[&str] = &["AV1Decoder", "AV1_COMMON", "MACROBLOCKD"];

/// Tables and limits of the entropy decoder that the Rust host modules read: `extern` in its headers, so
/// the bindings declare the C symbol.
const HOST_VARS: &[&str] =
    &["warped_filter", "wedge_params_lookup", "ii_weights1d", "ii_size_scales", "WARPEDPIXEL_PREC_SHIFTS"];

/// Tables that are `static` in a header of the decoder and that the Rust host modules have a copy of
/// (`crates/rav1g/src/host/tables.rs`): name and the expression of its bytes. The C probe exports them so
/// that a test can hold the copy against the header ([`table_source`]).
const C_TABLES: &[&str] = &["block_size_wide", "block_size_high", "ss_size_lookup"];

/// The structs of the C++ host whose layout the tests hold against the C++ compiler of the build
/// (host_layout_probe.cpp): what the Rust host modules read or write in place.
const HOST_PROBED: &[&str] = &[
    "GpuBufferObject",
    "dx_compute_context",
    "av1_memory_allocator",
    "av1_memory_allocator_dummy",
    "av1_video_memory",
    "ComputeCommandBuffer",
    "ComputeShader",
    "compute_shader_lib",
    "DataPtrQueue",
    "DataPtrQueueMT",
    "HwFrameBuffer",
    "av1_frame_thread_data",
    "Av1Core",
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
            // The same for the base class and the virtual function table of a C++ struct.
            if field.starts_with("_bitfield")
                || field.starts_with("__bindgen")
                || field.starts_with("_base")
                || field == "vtable_"
            {
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
    text.push_str(&table_source(C_TABLES, "rav1g_c_tables"));
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

/// The probe of the C++ host. The private members of the allocators are probed like the others; the
/// system headers are read before `private` is a macro.
fn probe_source_cpp(probes: &[Probe]) -> String {
    let mut text = String::from(
        "// Written by build.rs: the layouts of the C++ host as this compiler has them.\n\
         #define _ALLOW_KEYWORD_MACROS\n\
         #include <stddef.h>\n#include <stdint.h>\n#include <windows.h>\n#include <wrl.h>\n#include <d3d12.h>\n\
         #include <dxgi1_4.h>\n#include <d3dx12.h>\n#include <queue>\n#include <algorithm>\n#include <cstring>\n\
         #define private public\n#define protected public\n#include \"host.hpp\"\n\n\
         struct rav1g_layout_entry {\n  const char *name;\n  uint32_t size;\n  uint32_t offset;\n  uint32_t align;\n\
         };\n\nstatic const rav1g_layout_entry rav1g_host_layout_entries[] = {\n",
    );
    for probe in probes {
        let t = &probe.c_type;
        text.push_str(&format!("  {{ \"{t}\", (uint32_t)sizeof({t}), 0, (uint32_t)alignof({t}) }},\n"));
        for field in &probe.fields {
            let f = c_field(field);
            text.push_str(&format!(
                "  {{ \"{t}.{f}\", (uint32_t)sizeof((({t} *)0)->{f}), (uint32_t)offsetof({t}, {f}), 0 }},\n"
            ));
        }
    }
    text.push_str(
        "};\n\nextern \"C\" const rav1g_layout_entry *rav1g_host_layout_table(uint32_t *count) {\n\
         \x20 *count = (uint32_t)(sizeof(rav1g_host_layout_entries) / sizeof(rav1g_host_layout_entries[0]));\n\
         \x20 return rav1g_host_layout_entries;\n}\n",
    );
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
