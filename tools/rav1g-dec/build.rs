// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Builds the C++ that rav1g-dec shares with the test player (shim/shim.h): the stream readers of
//! testing/common with libwebm's parser, the timing summary, the stage dump writer, the Shader Model 6
//! test, and for -playersmoke the decoder code of the player itself (testing/player_common), which calls
//! the functions of rav1g.h that the `rav1g` crate of this program has.
//!
//! `RAV1G_SM6TEST_SHADER_DIR` (optional) names the directory whose `sm6test_generated/shaders.h` has the
//! DXC shaders of the Shader Model 6 test (`testing/sm6test/compile.py --out <dir>`), the directory MSBuild
//! gets as `/p:Rav1gSm6TestShaders`. Without it `-sm6test` reports that the test shaders are not built in.

use std::env;
use std::path::{Path, PathBuf};

fn base_build(repo: &Path, shim: &Path, sm6: Option<&Path>, cpp: bool) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .cpp(cpp)
        .warnings(false)
        .include(shim)
        .include(repo.join("libav1"))
        .include(repo.join("libav1").join("build"))
        .include(repo.join("testing"))
        // rav1g.h, for the player's code.
        .include(repo.join("crates").join("rav1g").join("include"))
        .include(repo.join("third_party").join("libwebm"))
        .define("_CRT_SECURE_NO_WARNINGS", None)
        .define("_UNICODE", None)
        .define("UNICODE", None)
        .define("_CONSOLE", None)
        .flag_if_supported("/sdl")
        .flag_if_supported("/wd4996");
    // /O1 /Ot, whatever the cargo profile: what the desktop program of the C++ decoder (warp_decoder) has.
    build.opt_level(1).flag_if_supported("/Ot");
    if cpp {
        // The exception model of the MSBuild projects; cc does not choose one.
        build.flag_if_supported("/EHsc");
    }
    if let Some(dir) = sm6 {
        build.include(dir).define("RAV1G_SM6TEST_SHADERS", None);
    }
    build
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // Two levels up, without canonicalize: on Windows that gives a \\?\ path, which cl does not take.
    let repo = manifest.parent().and_then(Path::parent).expect("the workspace root").to_path_buf();
    let shim = manifest.join("shim");
    println!("cargo:rerun-if-changed={}", shim.display());
    println!("cargo:rerun-if-changed={}", repo.join("testing").join("common").display());
    println!("cargo:rerun-if-changed={}", repo.join("testing").join("player_common").display());
    println!("cargo:rerun-if-changed={}", repo.join("crates").join("rav1g").join("include").display());
    println!("cargo:rerun-if-changed={}", repo.join("testing").join("sm6test").display());
    println!("cargo:rerun-if-changed={}", repo.join("third_party").join("libwebm").display());
    println!("cargo:rerun-if-env-changed=RAV1G_SM6TEST_SHADER_DIR");

    let sm6 = env::var_os("RAV1G_SM6TEST_SHADER_DIR").filter(|value| !value.is_empty()).map(PathBuf::from);
    if let Some(dir) = &sm6 {
        let header = dir.join("sm6test_generated").join("shaders.h");
        if !header.is_file() {
            eprintln!(
                "rav1g-dec: RAV1G_SM6TEST_SHADER_DIR={}: there is no {} (testing/sm6test/compile.py --out <dir> \
                 writes it); unset the variable to build without the Shader Model 6 test shaders",
                dir.display(),
                header.display()
            );
            std::process::exit(1);
        }
        println!("cargo:rerun-if-changed={}", header.display());
    }

    let common = repo.join("testing").join("common");
    let webm = repo.join("third_party").join("libwebm").join("mkvparser");
    let mut c = base_build(&repo, &shim, sm6.as_deref(), false);
    for name in ["ivfdec.c", "md5_utils.c", "obudec.c", "tools_common.c"] {
        c.file(common.join(name));
    }
    c.compile("rav1g_dec_c");
    let mut cpp = base_build(&repo, &shim, sm6.as_deref(), true);
    for name in ["frame_outs.cpp", "log.cpp", "mp4parser.cc", "stream_reader.cpp", "webmdec.cc"] {
        cpp.file(common.join(name));
    }
    // The decoder code of the test player, for -playersmoke. usage_exit, which the stream readers call, is
    // the one of shim.cpp.
    let player = repo.join("testing").join("player_common");
    cpp.define("RAV1G_PLAYER_NO_USAGE_EXIT", None);
    for name in ["cmd_parser.cpp", "decoder.cpp", "decoder_creator.cpp"] {
        cpp.file(player.join(name));
    }
    cpp.file(webm.join("mkvparser.cc"))
        .file(webm.join("mkvreader.cc"))
        .file(repo.join("testing").join("sm6test").join("sm6test.cpp"))
        .file(shim.join("player_smoke.cpp"))
        .file(shim.join("shim.cpp"));
    cpp.compile("rav1g_dec_shim");

    // The decoder's C is compiled for link time optimisation (crates/rav1g-sys/build.rs, Lto): under MSVC
    // the link of the program is where it is compiled, which link.exe is told here and would otherwise find
    // out and start again. Incremental, as the MSBuild projects link: see there.
    println!("cargo:rerun-if-env-changed=RAV1G_C_LTO");
    let msvc = !cc::Build::new().get_compiler().is_like_clang_cl();
    if msvc && env::var("RAV1G_C_LTO").map_or(true, |choice| choice != "off") {
        println!("cargo:rustc-link-arg=/LTCG:incremental");
    }
}
