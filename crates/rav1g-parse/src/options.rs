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
    /// [`SinkOptions::late_lists`]: the block lists of a tile are counted when the tile is parsed, as
    /// they were before they were counted superblock by superblock. Off by default.
    pub late_lists: bool,
    /// The host clears the mode info grid of a pipeline slot for every frame, as it did before it kept
    /// the grid between frames of one size. Not a switch of the parse: the D3D12 host reads it
    /// (`rav1g_setup_frame_begin`). Off by default.
    pub clear_grid: bool,
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
}

impl Default for ParseOptions {
    /// `nolfmask`, `tilethread` and `gpucdef`.
    fn default() -> Self {
        Self::from_bits(Self::DEFAULT)
    }
}

impl ParseOptions {
    /// The switches as bits (`RAV1G_PARSER_OPTION_*` of `rav1g.h`, `AV1_PARSER_OPTION_*` of the C). The
    /// values 1 and 256 were switches that are gone (`stage`; `recordonce`, a mode info record written
    /// to its pool once, which was the slower way) and mean nothing.
    pub const NO_LF_MASKS: u32 = 2;
    pub const TILE_KEEPS_THREAD: u32 = 4;
    pub const TIME_CALLS: u32 = 8;
    pub const LATE_LISTS: u32 = 16;
    pub const CLEAR_GRID: u32 = 32;
    pub const TEST_PANIC: u32 = 64;
    pub const TEST_POOL_WAIT: u32 = 128;
    pub const GPU_CDEF_LISTS: u32 = 512;
    /// The bits of [`ParseOptions::default`].
    pub const DEFAULT: u32 = Self::NO_LF_MASKS | Self::TILE_KEEPS_THREAD | Self::GPU_CDEF_LISTS;
    /// The three switches that are on by default, which a list of words names to replace that default.
    const WAYS: u32 = Self::DEFAULT;

    /// No switch: the parse as upstream's rav1d does it.
    pub const NONE: ParseOptions = ParseOptions {
        no_lf_masks: false,
        tile_keeps_thread: false,
        time_calls: false,
        late_lists: false,
        clear_grid: false,
        test_panic: false,
        test_pool_wait: false,
        gpu_cdef_lists: false,
    };

    /// The words of the tools (`-parseopt <word>[,<word>...]`).
    pub const WORDS: [(&'static str, u32); 8] = [
        ("nolfmask", Self::NO_LF_MASKS),
        ("tilethread", Self::TILE_KEEPS_THREAD),
        ("time", Self::TIME_CALLS),
        ("latelists", Self::LATE_LISTS),
        ("cleargrid", Self::CLEAR_GRID),
        ("testpanic", Self::TEST_PANIC),
        ("poolwait", Self::TEST_POOL_WAIT),
        ("gpucdef", Self::GPU_CDEF_LISTS),
    ];

    pub const fn from_bits(bits: u32) -> Self {
        Self {
            no_lf_masks: bits & Self::NO_LF_MASKS != 0,
            tile_keeps_thread: bits & Self::TILE_KEEPS_THREAD != 0,
            time_calls: bits & Self::TIME_CALLS != 0,
            late_lists: bits & Self::LATE_LISTS != 0,
            clear_grid: bits & Self::CLEAR_GRID != 0,
            test_panic: bits & Self::TEST_PANIC != 0,
            test_pool_wait: bits & Self::TEST_POOL_WAIT != 0,
            gpu_cdef_lists: bits & Self::GPU_CDEF_LISTS != 0,
        }
    }

    pub fn bits(self) -> u32 {
        u32::from(self.no_lf_masks) * Self::NO_LF_MASKS
            | u32::from(self.tile_keeps_thread) * Self::TILE_KEEPS_THREAD
            | u32::from(self.time_calls) * Self::TIME_CALLS
            | u32::from(self.late_lists) * Self::LATE_LISTS
            | u32::from(self.clear_grid) * Self::CLEAR_GRID
            | u32::from(self.test_panic) * Self::TEST_PANIC
            | u32::from(self.test_pool_wait) * Self::TEST_POOL_WAIT
            | u32::from(self.gpu_cdef_lists) * Self::GPU_CDEF_LISTS
    }

    /// A list of words, separated by commas. `nolfmask`, `tilethread` and `gpucdef` are the three
    /// switches that are on by default, and a list that has one of them, `none` or `default` replaces
    /// that default: of the three, the ones the list names are on and no other (`none`: none of them;
    /// `default`: all three; `nolfmask,tilethread`: the lists of CDEF written by the thread that records,
    /// as before the GPU wrote them). `time` (the timing of the sink's calls), `latelists` (the block
    /// lists at the end of a tile) and `cleargrid` (the mode info grid cleared for every frame) are off
    /// by default; each is on when the list has it and leaves the three as they are, so `time` alone
    /// times the default. `testpanic` is not a way of doing a parse: the parse of the third frame panics,
    /// for the test of what a bug does. Neither is `poolwait`: a frame finds the host's pools empty while
    /// an older frame holds a piece, for the test of a parse that waits for memory. `None` for a word
    /// that is none of these.
    ///
    /// | List | `nolfmask` | `tilethread` | `gpucdef` | `time` |
    /// | --- | --- | --- | --- | --- |
    /// | no list, `default` | on | on | on | |
    /// | `time`, `default,time` | on | on | on | on |
    /// | `none` | | | | |
    /// | `none,time` | | | | on |
    /// | `nolfmask` | on | | | |
    /// | `nolfmask,tilethread` | on | on | | |
    /// | `tilethread,time` | | on | | on |
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
    /// none of the three that are on by default is.
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
        SinkOptions { time_calls: self.time_calls, late_lists: self.late_lists, test_panic: self.test_panic }
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
        assert!(default.no_lf_masks && default.tile_keeps_thread && default.gpu_cdef_lists && !default.time_calls);
        assert_eq!(default.bits(), 518);
        assert_eq!(default.words(), "nolfmask,tilethread,gpucdef");
        assert_eq!(ParseOptions::from_words("default"), Some(default));
        assert_eq!(ParseOptions::from_words("nolfmask,tilethread,gpucdef"), Some(default));
        assert_eq!(ParseOptions::from_words("none"), Some(ParseOptions::NONE));
        assert_eq!(ParseOptions::NONE.words(), "none");
        assert_eq!(ParseOptions::NONE.bits(), 0);
        // A way that is named replaces the default; the timing does not.
        let one = ParseOptions::from_words("tilethread").unwrap();
        assert!(one.tile_keeps_thread && !one.no_lf_masks && !one.gpu_cdef_lists && !one.time_calls);
        let timed = ParseOptions::from_words("time").unwrap();
        assert_eq!(timed, ParseOptions { time_calls: true, ..default });
        assert_eq!(ParseOptions::from_words("default,time"), Some(timed));
        // The default without the lists of CDEF by the GPU: the two other ways, named.
        let cpu_lists = ParseOptions::from_words("nolfmask,tilethread").unwrap();
        assert_eq!(cpu_lists, ParseOptions { gpu_cdef_lists: false, ..default });
        assert_eq!(cpu_lists.bits(), 6);
        assert_eq!(cpu_lists.words(), "nolfmask,tilethread");
        let only_lists = ParseOptions::from_words("gpucdef,time").unwrap();
        assert_eq!(only_lists, ParseOptions { gpu_cdef_lists: true, time_calls: true, ..ParseOptions::NONE });
        assert_eq!(only_lists.bits(), 520);
        assert_eq!(only_lists.words(), "time,gpucdef");
        // The other switch that is off by default: it adds to the three, like the timing.
        let late = ParseOptions::from_words("latelists").unwrap();
        assert_eq!(late, ParseOptions { late_lists: true, ..default });
        assert_eq!(late.bits(), 534);
        assert_eq!(late.words(), "nolfmask,tilethread,latelists,gpucdef");
        let none_late = ParseOptions::from_words("none,latelists,time").unwrap();
        assert_eq!(none_late, ParseOptions { late_lists: true, time_calls: true, ..ParseOptions::NONE });
        assert_eq!(none_late.words(), "none,time,latelists");
        let old = ParseOptions::from_words("latelists,cleargrid").unwrap();
        assert_eq!(old, ParseOptions { late_lists: true, clear_grid: true, ..default });
        assert_eq!(old.bits(), 566);
        let some = ParseOptions::from_words("tilethread,time").unwrap();
        assert!(some.tile_keeps_thread && some.time_calls && !some.no_lf_masks);
        let none_timed = ParseOptions::from_words("none,time").unwrap();
        assert_eq!(none_timed, ParseOptions { time_calls: true, ..ParseOptions::NONE });
        assert_eq!(none_timed.words(), "none,time");
        for options in
            [default, ParseOptions::NONE, one, timed, cpu_lists, only_lists, some, none_timed, late, none_late, old]
        {
            assert_eq!(ParseOptions::from_bits(options.bits()), options);
            assert_eq!(ParseOptions::from_words(&options.words()), Some(options));
        }
        // The switches that are gone, and words that never were.
        assert_eq!(ParseOptions::from_words("stage"), None);
        assert_eq!(ParseOptions::from_words("recordonce"), None);
        assert_eq!(ParseOptions::from_words("all"), None);
        assert_eq!(ParseOptions::from_words("tilethread,other"), None);
        assert_eq!(ParseOptions::from_bits(1), ParseOptions::NONE);
        assert_eq!(ParseOptions::from_bits(256), ParseOptions::NONE);
        // The test switches: bits of their own, off in every default.
        let panics = ParseOptions::from_words("testpanic").unwrap();
        assert_eq!(panics, ParseOptions { test_panic: true, ..default });
        assert_eq!(panics.bits(), 582);
        assert_eq!(ParseOptions::from_bits(582), panics);
        let small = ParseOptions::from_words("poolwait").unwrap();
        assert_eq!(small, ParseOptions { test_pool_wait: true, ..default });
        assert_eq!(small.bits(), 646);
        assert_eq!(small.words(), "nolfmask,tilethread,poolwait,gpucdef");
    }
}
