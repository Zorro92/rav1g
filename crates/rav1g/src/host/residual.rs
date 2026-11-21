// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Byte strides and offsets of the 16-bit residual planes of a coded 4:2:0 frame.

use std::ffi::c_int;
use std::mem::size_of;

/// The layout includes the full last superblock of each plane. Chroma has half the dimensions
/// of luma (rounded up) and uses the same 128-sample alignment.
pub(crate) fn plane_layout(width: c_int, height: c_int) -> [(c_int, c_int); 3] {
    let align = |samples: c_int| (samples + 127) & !127;
    let bpp = size_of::<i16>() as c_int;
    let y_stride = bpp * align(width);
    let uv_stride = bpp * align((width + 1) / 2);
    let u = y_stride * align(height);
    let v = u + uv_stride * align((height + 1) / 2);
    [(y_stride, 0), (uv_stride, u), (uv_stride, v)]
}

/// End of V, which has the same stride and height as U.
pub(crate) fn frame_bytes(u_offset: c_int, v_offset: c_int) -> usize {
    v_offset as usize + (v_offset - u_offset) as usize
}

/// Number of Store4 operations in the residual clear. Every plane size is a multiple of 16 bytes.
pub(crate) fn clear_work_items(allocated: usize, u_offset: c_int, v_offset: c_int) -> c_int {
    let bytes = frame_bytes(u_offset, v_offset).min(allocated);
    (bytes / size_of::<[u32; 4]>()) as c_int
}

#[cfg(test)]
mod tests {
    use super::*;

    // The original frame.rs layout and allocation formula, including the intermediate rounding to 8.
    fn original_layout(width: c_int, height: c_int) -> ([(c_int, c_int); 3], usize) {
        let y_width = (width + 7) & !7;
        let y_height = (height + 7) & !7;
        let y_stride = 2 * ((y_width + 127) & !127);
        let uv_stride = 2 * (((y_width >> 1) + 127) & !127);
        let u = y_stride * ((y_height + 127) & !127);
        let chroma_bytes = uv_stride * (((y_height >> 1) + 127) & !127);
        let v = u + chroma_bytes;
        ([(y_stride, 0), (uv_stride, u), (uv_stride, v)], (v + chroma_bytes) as usize)
    }

    fn random(state: &mut u32, range: u32) -> u32 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        (*state >> 8) % range
    }

    #[test]
    fn layouts_match_the_original() {
        let mut seed = 0x38b96d09;
        let edges = [1, 7, 8, 63, 64, 65, 127, 128, 129, 255, 256, 257, 2160, 3840];
        let edge_cases = edges.iter().flat_map(|&w| edges.iter().map(move |&h| (w, h)));
        let random_cases =
            (0..8192).map(|_| (1 + random(&mut seed, 4096) as c_int, 1 + random(&mut seed, 2304) as c_int));
        for (w, h) in edge_cases.chain(random_cases) {
            let (original, bytes) = original_layout(w, h);
            let layout = plane_layout(w, h);
            assert_eq!(layout, original);
            assert_eq!(frame_bytes(layout[1].1, layout[2].1), bytes);
            assert_eq!(bytes % 16, 0);
        }
    }

    #[test]
    fn clears_active_planes_on_slot_reuse() {
        let (max_width, max_height) = (512, 640);
        let (_, allocated) = original_layout(max_width, max_height);
        let mut reference = vec![0xcd; allocated];
        let mut ranged = reference.clone();
        let mut seed = 0x6d538c54;
        // Alternate growth and shrinkage, including an odd crop and a minimum-sized frame.
        let sizes = [(512, 640), (131, 127), (1, 1), (256, 257), (385, 513), (383, 65), (512, 640)];
        for (w, h) in sizes {
            let (planes, bytes) = original_layout(w, h);
            let [_, (_, u), (_, v)] = plane_layout(w, h);
            let prefix = clear_work_items(allocated, u, v) as usize * 16;
            if bytes < allocated {
                // A larger frame or a previous use of this memory could have left dirty data here.
                reference[bytes] = 0x43;
                ranged[bytes] = 0x43;
            }
            assert_eq!(prefix, bytes);
            // fill_buffer.hlsl writes one 16-byte word per work item to the start of the buffer.
            reference.fill(0);
            ranged[..prefix].fill(0);
            assert!(reference[..bytes] == ranged[..bytes]);
            if bytes < allocated {
                assert_eq!(reference[bytes], 0);
                assert_eq!(ranged[bytes], 0x43);
            }

            // Sparse IDCT writes leave most of the plane, including skipped blocks, untouched.
            for (plane, (stride, offset)) in planes.into_iter().enumerate() {
                let w = if plane == 0 { w } else { (w + 1) / 2 };
                let h = if plane == 0 { h } else { (h + 1) / 2 };
                for _ in 0..128 {
                    let x = random(&mut seed, w as u32) as c_int;
                    let y = random(&mut seed, h as u32) as c_int;
                    let address = (offset + y * stride + 2 * x) as usize;
                    let word = (1 + random(&mut seed, 65535) as u16).to_le_bytes();
                    reference[address..address + 2].copy_from_slice(&word);
                    ranged[address..address + 2].copy_from_slice(&word);
                }
            }
            // The next smaller frame can leave the discarded tail dirty. A frame that grows
            // clears that tail before any predictor reads it again.
            assert!(reference[..bytes] == ranged[..bytes]);
        }
    }
}
