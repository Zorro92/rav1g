// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The bytecode of the shaders, as a table by name: the shader library of the host
//! (`crates/rav1g/src/host/compute.rs`) makes its pipelines from it.
//!
//! `build.rs` writes the table from `shaders.txt` (the names, in the order the library creates the
//! pipelines) and the DXC build in `RAV1G_SHADER_DIR`: one `include_bytes!` of
//! `dx/shaders/dxil/<name>.dxil` per shader, so the library has the files DXC wrote, byte for byte.
//! [`SHADERS`] is the default set. With the feature `shader-pick` there is `SHADERS_OD` as well, the
//! DXC `-Od` set from `dx/shaders/dxil_od`, in the same order (docs/test-player.md, "Shader pick").
//! For a target that is not Windows [`SHADERS`] is empty.

/// A shader of the build: its name (the name of its source file) and its bytecode, a signed DXIL
/// container as DXC wrote it.
pub struct Shader {
    pub name: &'static str,
    pub code: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/shader_table.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    /// As in `build.rs`.
    fn fnv1a(data: &[u8]) -> u64 {
        data.iter()
            .fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3))
    }

    /// What the library embeds of a set is what the build script read from the files of the DXC build
    /// (their length and hash), each a whole signed DXBC container, under the names of `shaders.txt` in
    /// its order.
    fn embedded_as_read(table: &[Shader], read: &[(usize, u64)]) {
        let names: Vec<&str> = include_str!("../shaders.txt")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        if cfg!(windows) {
            assert_eq!(table.iter().map(|shader| shader.name).collect::<Vec<_>>(), names);
        } else {
            assert!(table.is_empty());
        }
        assert_eq!(table.len(), read.len());
        for (shader, (size, hash)) in table.iter().zip(read) {
            let code = shader.code;
            assert_eq!(code.len(), *size, "{}: length", shader.name);
            assert_eq!(fnv1a(code), *hash, "{}: bytes", shader.name);
            assert_eq!(&code[..4], b"DXBC", "{}", shader.name);
            assert_eq!(u32::from_le_bytes(code[24..28].try_into().unwrap()) as usize, code.len(), "{}", shader.name);
            assert!(code[4..20].iter().any(|byte| *byte != 0), "{}: no validation hash", shader.name);
        }
    }

    #[test]
    fn embedded_bytecode_is_the_file_of_the_shader_build() {
        embedded_as_read(SHADERS, SHADERS_READ);
        #[cfg(feature = "shader-pick")]
        embedded_as_read(SHADERS_OD, SHADERS_OD_READ);
    }
}
