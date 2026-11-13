#![deny(clippy::all)]

//! rav1g: the build script of upstream assembles 47 files for x86 and the files of three other
//! architectures. What is left here is the x86-64 assembly the parse pass calls: `msac.asm` (the entropy
//! decoder) and `refmvs.asm` (motion vector lists). The symbols get the prefix `rav1g_dav1d` instead of
//! `dav1d`, so that an application can link dav1d or rav1d next to this copy (PATCHES.md).

#[cfg(feature = "asm")]
mod asm {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::{env, fs};

    /// The files of `src/x86` that are assembled.
    const FILES: &[&str] = &["msac", "refmvs"];

    pub fn main() {
        let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
        #[cfg(debug_assertions)]
        let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
        let features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap();
        let features = features.split(',').collect::<HashSet<_>>();

        if arch != "x86_64" {
            panic!(
                "the assembly of this copy of rav1d is x86-64 only; build for {arch} without the \"asm\" feature"
            );
        }

        let defines = [
            ("CONFIG_ASM", "1".to_string()),
            ("CONFIG_LOG", "1".to_string()),
            ("private_prefix", "rav1g_dav1d".to_string()),
            ("ARCH_X86_32", "0".to_string()),
            ("ARCH_X86_64", "1".to_string()),
            ("STACK_ALIGNMENT", "16".to_string()),
            ("PIC", "1".to_string()),
            // Convert SSE asm into (128-bit) AVX when compiler flags are set to use AVX instructions.
            // Note that this checks compile-time CPU features, not runtime features,
            // but that does seem to be what `dav1d` does, too.
            (
                "FORCE_VEX_ENCODING",
                (features.contains("avx") as u8).to_string(),
            ),
        ];
        let config_contents = defines
            .iter()
            .map(|(name, value)| format!("%define {name} {value}"))
            .collect::<Vec<_>>()
            .join("\n");
        let config_path = out_dir.join("config.asm");
        fs::write(&config_path, &config_contents).unwrap();

        // Note that we don't add `rerun-if-changed` for `config.asm`: it is written by this script.
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-changed=src/ext/x86/x86inc.asm");

        let asm_file_paths = FILES.iter().map(|file_name| {
            let path = ["src", "x86", file_name].into_iter().collect::<PathBuf>().with_extension("asm");
            println!("cargo:rerun-if-changed={}", path.to_str().unwrap());
            path
        });

        let rav1dasm = "rav1dasm";

        let mut nasm = nasm_rs::Build::new();
        nasm.min_version(2, 14, 0);
        nasm.files(asm_file_paths);
        #[cfg(debug_assertions)]
        nasm.flag("-g");
        #[cfg(debug_assertions)]
        nasm.flag(match os.as_str() {
            "windows" => "-fwin64",
            _ => "-Fdwarf",
        });
        nasm.flag(&format!("-I{}/", out_dir.to_str().unwrap()));
        nasm.flag("-Isrc/");
        let obj = nasm.compile_objects().unwrap_or_else(|e| {
            println!("cargo:warning={e}");
            panic!(
                "NASM build failed. Make sure you have nasm installed or disable the \"asm\" feature.\n\
                You can get NASM from https://nasm.us or your system's package manager.\n\nerror: {e}"
            );
        });

        // cc is better at finding the correct archiver
        let mut cc = cc::Build::new();
        for o in obj {
            cc.object(o);
        }
        cc.compile(rav1dasm);

        println!("cargo:rustc-link-lib=static={rav1dasm}");
    }
}

fn main() {
    #[cfg(feature = "asm")]
    {
        asm::main();
    }
}
