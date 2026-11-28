// SPDX-License-Identifier: BSD-2-Clause

//! The host's code for the block lists, as this program has it: `blocks.rs`, `tables.rs` and `libaom.rs`
//! are the files of the D3D12 host (`crates/rav1g/src/host`), compiled here by path and unchanged. What
//! `blocks.rs` uses of the host's other modules is here in the form this program needs: `types` (the
//! names of the lists, read from the host's file by build.rs, and the struct of the host the function
//! reads the frame from) and `step_time` (the intra step timing, which needs the GPU). Of `libaom.rs`
//! the block lists ask two functions (which blocks have their top right and their bottom left decoded);
//! its tables are written by the host's build script, which the build script of this program runs.

#[path = "../../../../crates/rav1g/src/host/blocks.rs"]
#[allow(clippy::needless_range_loop, reason = "the host's file, under the lints of its own crate there")]
pub mod blocks;
#[path = "../../../../crates/rav1g/src/host/libaom.rs"]
#[allow(dead_code, clippy::needless_range_loop, reason = "the host's file: this program uses two functions of it")]
pub mod libaom;
pub mod step_time;
#[path = "../../../../crates/rav1g/src/host/tables.rs"]
#[allow(dead_code)]
pub mod tables;
pub mod types;

/// What the tests of the host's `libaom.rs` take from its `rav1d_frames`.
#[cfg(test)]
mod rav1d_frames {
    /// A cleared C struct on the heap.
    ///
    /// # Safety
    /// All zero bytes are a value of `T`.
    pub(super) unsafe fn zeroed_box<T>() -> Box<T> {
        let layout = std::alloc::Layout::new::<T>();
        // SAFETY: the caller's contract; the allocation has the layout of `T`.
        unsafe {
            let memory = std::alloc::alloc_zeroed(layout);
            if memory.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            Box::from_raw(memory.cast::<T>())
        }
    }
}
