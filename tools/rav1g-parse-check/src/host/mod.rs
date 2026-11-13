// SPDX-License-Identifier: BSD-2-Clause

//! The host's code for the block lists, as this program has it: `blocks.rs` and `tables.rs` are the
//! files of the D3D12 host (`crates/rav1g/src/host`), compiled here by path and unchanged. What
//! `blocks.rs` uses of the host's other modules is here in the form this program needs: `types` (the
//! names of the lists, read from the host's file by build.rs, and the two structs of the host the
//! function reads the frame from) and `intra_v2` (a path of the host this program does not take).

#[path = "../../../../crates/rav1g/src/host/blocks.rs"]
#[allow(clippy::needless_range_loop, reason = "the host's file, under the lints of its own crate there")]
pub mod blocks;
pub mod intra_v2;
#[path = "../../../../crates/rav1g/src/host/tables.rs"]
#[allow(dead_code)]
pub mod tables;
pub mod types;
