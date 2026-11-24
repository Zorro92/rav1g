// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The switches of the parser of phase 2: ways of doing the same parse (docs/phase2-rav1d.md, "Where the
//! time goes"). The buffers of the GPU stages hold the same bytes with every one of them. Three are on
//! unless an application turns them off; a switch stays a switch so that a device can time the parse with
//! and without it.

use rav1d::parse::ParseSettings;

use crate::sink::SinkOptions;

/// The switches. [`ParseOptions::default`] is how the parser runs; [`ParseOptions::NONE`] is rav1d's
/// parse as upstream schedules it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseOptions {
    /// rav1d does not fill the masks and levels of its own loop filter (`ParseSettings::lf_masks` off).
    /// On by default.
    pub no_lf_masks: bool,
    /// `ParseSettings::tile_keeps_thread`: a tile is not handed from thread to thread between its
    /// superblock rows. On by default.
    pub tile_keeps_thread: bool,
    /// [`SinkOptions::time_calls`]: the calls of the sink are timed. Off by default.
    pub time_calls: bool,
    /// [`SinkOptions::test_panic`]: the parse of the third frame panics, for the test of what a bug in
    /// the parser does to a decode call. Off, and no way of doing a parse.
    pub test_panic: bool,
    /// For the test of a parse that waits for memory: behind its first piece of a pool (mode info
    /// records, coefficients, transform block records; docs/phase2-rav1d.md, step 5) a frame finds
    /// the pool of the D3D12 host empty for as long as an older frame holds anything of it, so every
    /// frame of every stream waits in the middle of its parse, as a frame does that finds the pool
    /// exhausted. Off, and no way of doing a parse.
    pub test_pool_wait: bool,
    /// The two lists CDEF reads (the strength index of every 64x64 block and the skip flag of every 8x8
    /// block) are written by a pass of the GPU in front of CDEF (`cdef_lists.hlsl`) and not by the
    /// thread that records the frame. Not a switch of the parse: the D3D12 host reads it when the decoder
    /// is created (the two buffers are then buffers a shader may write). On by default.
    pub gpu_cdef_lists: bool,
    /// [`SinkOptions::gpu_grid`]: the sink does not write the mode info grid; a pass of the GPU builds it
    /// from the frame's records, in front of every stage that reads it (`mi_grid_build.hlsl`), and what
    /// the parse threads read through the grid (the records above and to the left of a block for the
    /// block lists, the record a 64x64 area keeps its CDEF index in) the sink keeps at the edges of the
    /// tile. The D3D12 host reads it when the decoder is created: the grid of every slot is then a buffer
    /// a shader may write, the lists of CDEF are the GPU's whatever [`Self::gpu_cdef_lists`] says (the
    /// thread that records would read the grid for them). On by default.
    pub gpu_grid: bool,
}

impl Default for ParseOptions {
    /// `nolfmask`, `tilethread`, `gpucdef` and `gpugrid`.
    fn default() -> Self {
        Self::from_bits(Self::DEFAULT)
    }
}

impl ParseOptions {
    /// The switches as bits (`RAV1G_PARSER_OPTION_*` of `rav1g.h`, `AV1_PARSER_OPTION_*` of the C). The
    /// values 1 and 256 were switches that are gone (`stage`; `recordonce`, a mode info record written
    /// to its pool once, which was the slower way) and mean nothing. [`Self::RETIRED`] are the bits of
    /// the switches retired with interface version 3, which `rav1g.h` refuses.
    pub const NO_LF_MASKS: u32 = 2;
    pub const TILE_KEEPS_THREAD: u32 = 4;
    pub const TIME_CALLS: u32 = 8;
    pub const TEST_PANIC: u32 = 64;
    pub const TEST_POOL_WAIT: u32 = 128;
    pub const GPU_CDEF_LISTS: u32 = 512;
    pub const GPU_GRID: u32 = 2048;
    /// The bits of the switches retired with interface version 3: 16 `latelists` (the block lists of a
    /// tile counted at its end), 32 `cleargrid` (the mode info grid cleared for every frame), 1024
    /// `gridcheck` (a second grid by the GPU, held against the sink's) and 4096 `gpublists` (the inter,
    /// warp and reconstruction lists counted by the GPU).
    pub const RETIRED: u32 = 16 | 32 | 1024 | 4096;
    /// The bits of [`ParseOptions::default`].
    pub const DEFAULT: u32 = Self::NO_LF_MASKS | Self::TILE_KEEPS_THREAD | Self::GPU_CDEF_LISTS | Self::GPU_GRID;
    /// The switches that are on by default, which a list of words names to replace that default.
    const WAYS: u32 = Self::DEFAULT;

    /// No switch: the parse as upstream's rav1d does it.
    pub const NONE: ParseOptions = ParseOptions {
        no_lf_masks: false,
        tile_keeps_thread: false,
        time_calls: false,
        test_panic: false,
        test_pool_wait: false,
        gpu_cdef_lists: false,
        gpu_grid: false,
    };

    /// The words of the tools (`-parseopt <word>[,<word>...]`).
    pub const WORDS: [(&'static str, u32); 7] = [
        ("nolfmask", Self::NO_LF_MASKS),
        ("tilethread", Self::TILE_KEEPS_THREAD),
        ("time", Self::TIME_CALLS),
        ("testpanic", Self::TEST_PANIC),
        ("poolwait", Self::TEST_POOL_WAIT),
        ("gpucdef", Self::GPU_CDEF_LISTS),
        ("gpugrid", Self::GPU_GRID),
    ];

    pub const fn from_bits(bits: u32) -> Self {
        Self {
            no_lf_masks: bits & Self::NO_LF_MASKS != 0,
            tile_keeps_thread: bits & Self::TILE_KEEPS_THREAD != 0,
            time_calls: bits & Self::TIME_CALLS != 0,
            test_panic: bits & Self::TEST_PANIC != 0,
            test_pool_wait: bits & Self::TEST_POOL_WAIT != 0,
            gpu_cdef_lists: bits & Self::GPU_CDEF_LISTS != 0,
            gpu_grid: bits & Self::GPU_GRID != 0,
        }
    }

    pub fn bits(self) -> u32 {
        (u32::from(self.no_lf_masks) * Self::NO_LF_MASKS)
            | (u32::from(self.tile_keeps_thread) * Self::TILE_KEEPS_THREAD)
            | (u32::from(self.time_calls) * Self::TIME_CALLS)
            | (u32::from(self.test_panic) * Self::TEST_PANIC)
            | (u32::from(self.test_pool_wait) * Self::TEST_POOL_WAIT)
            | (u32::from(self.gpu_cdef_lists) * Self::GPU_CDEF_LISTS)
            | (u32::from(self.gpu_grid) * Self::GPU_GRID)
    }

    /// A list of words, separated by commas. Naming any default switch (`nolfmask`, `tilethread`,
    /// `gpucdef`, `gpugrid`), `none` or `default` selects the default switches explicitly.
    /// The other words add to the selection. For example, `time` times the default and
    /// `nolfmask,tilethread,gpucdef` keeps the sink-written grid.
    /// Returns `None` for an unknown word.
    pub fn from_words(list: &str) -> Option<Self> {
        let mut bits = 0;
        let mut ways_named = false;
        for word in list.split(',') {
            let bit = match word {
                "none" => 0,
                "default" => Self::DEFAULT,
                _ => Self::WORDS.iter().find(|(name, _)| *name == word)?.1,
            };
            ways_named |= bit & Self::WAYS != 0 || bit == 0;
            bits |= bit;
        }
        if !ways_named {
            bits |= Self::DEFAULT;
        }
        Some(Self::from_bits(bits))
    }

    /// The words of the switches that are on, as [`Self::from_words`] reads them: `none` in front when
    /// none of the default switches is on.
    pub fn words(self) -> String {
        let mut words: Vec<&str> =
            Self::WORDS.iter().filter(|(_, bit)| self.bits() & bit != 0).map(|(name, _)| *name).collect();
        if self.bits() & Self::WAYS == 0 {
            words.insert(0, "none");
        }
        words.join(",")
    }

    /// The part of the sink.
    pub fn sink(self) -> SinkOptions {
        SinkOptions {
            time_calls: self.time_calls,
            test_panic: self.test_panic,
            gpu_grid: self.gpu_grid,
            // Not a switch of these (they are bits of `rav1g.h`): rav1g-parse-check turns it off for its
            // -listslegacy.
            pipe_lists: true,
        }
    }

    /// The part of rav1d, into its settings.
    pub fn apply(self, settings: &mut ParseSettings) {
        settings.lf_masks = !self.no_lf_masks;
        settings.tile_keeps_thread = self.tile_keeps_thread;
    }
}

#[cfg(test)]
mod tests {
    use super::ParseOptions;

    #[test]
    fn words_and_bits() {
        let default = ParseOptions::default();
        assert!(
            default.no_lf_masks
                && default.tile_keeps_thread
                && default.gpu_cdef_lists
                && default.gpu_grid
                && !default.time_calls
        );
        assert_eq!(default.bits(), 2566);
        assert_eq!(default.words(), "nolfmask,tilethread,gpucdef,gpugrid");
        assert_eq!(ParseOptions::from_words("default"), Some(default));
        assert_eq!(ParseOptions::from_words("nolfmask,tilethread,gpucdef,gpugrid"), Some(default));
        assert_eq!(ParseOptions::from_words("none"), Some(ParseOptions::NONE));
        assert_eq!(ParseOptions::NONE.words(), "none");
        assert_eq!(ParseOptions::NONE.bits(), 0);
        // A way that is named replaces the default; the timing does not.
        let one = ParseOptions::from_words("tilethread").unwrap();
        assert!(one.tile_keeps_thread && !one.no_lf_masks && !one.gpu_cdef_lists && !one.time_calls);
        let timed = ParseOptions::from_words("time").unwrap();
        assert_eq!(timed, ParseOptions { time_calls: true, ..default });
        assert_eq!(ParseOptions::from_words("default,time"), Some(timed));
        // Keep the parser defaults and have the CPU write the grid and CDEF lists.
        let cpu_lists = ParseOptions::from_words("nolfmask,tilethread").unwrap();
        assert_eq!(cpu_lists, ParseOptions { gpu_cdef_lists: false, gpu_grid: false, ..default });
        assert_eq!(cpu_lists.bits(), 6);
        assert_eq!(cpu_lists.words(), "nolfmask,tilethread");
        let only_lists = ParseOptions::from_words("gpucdef,time").unwrap();
        assert_eq!(only_lists, ParseOptions { gpu_cdef_lists: true, time_calls: true, ..ParseOptions::NONE });
        assert_eq!(only_lists.bits(), 520);
        assert_eq!(only_lists.words(), "time,gpucdef");
        let some = ParseOptions::from_words("tilethread,time").unwrap();
        assert!(some.tile_keeps_thread && some.time_calls && !some.no_lf_masks);
        let none_timed = ParseOptions::from_words("none,time").unwrap();
        assert_eq!(none_timed, ParseOptions { time_calls: true, ..ParseOptions::NONE });
        assert_eq!(none_timed.words(), "none,time");
        let gpu_grid = ParseOptions::from_words("gpugrid").unwrap();
        assert_eq!(gpu_grid, ParseOptions { gpu_grid: true, ..ParseOptions::NONE });
        assert!(default.sink().gpu_grid && !cpu_lists.sink().gpu_grid);
        for options in [default, ParseOptions::NONE, one, timed, cpu_lists, only_lists, some, none_timed, gpu_grid] {
            assert_eq!(ParseOptions::from_bits(options.bits()), options);
            assert_eq!(ParseOptions::from_words(&options.words()), Some(options));
        }
        // The switches that are gone, and words that never were.
        assert_eq!(ParseOptions::from_words("stage"), None);
        assert_eq!(ParseOptions::from_words("recordonce"), None);
        for word in ["latelists", "cleargrid", "gridcheck", "gpublists"] {
            assert_eq!(ParseOptions::from_words(word), None);
        }
        // The bits of those retired with interface version 3 mean nothing here; rav1g.h refuses them.
        assert_eq!(ParseOptions::from_bits(ParseOptions::RETIRED), ParseOptions::NONE);
        assert_eq!(ParseOptions::RETIRED & (ParseOptions::DEFAULT | ParseOptions::TIME_CALLS), 0);
        assert_eq!(ParseOptions::from_words("all"), None);
        assert_eq!(ParseOptions::from_words("tilethread,other"), None);
        assert_eq!(ParseOptions::from_bits(1), ParseOptions::NONE);
        assert_eq!(ParseOptions::from_bits(256), ParseOptions::NONE);
        // The test switches: bits of their own, off in every default.
        let panics = ParseOptions::from_words("testpanic").unwrap();
        assert_eq!(panics, ParseOptions { test_panic: true, ..default });
        assert_eq!(panics.bits(), 2630);
        assert_eq!(ParseOptions::from_bits(2630), panics);
        let small = ParseOptions::from_words("poolwait").unwrap();
        assert_eq!(small, ParseOptions { test_pool_wait: true, ..default });
        assert_eq!(small.bits(), 2694);
        assert_eq!(small.words(), "nolfmask,tilethread,poolwait,gpucdef,gpugrid");
    }
}
