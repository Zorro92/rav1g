// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The switches of the parser of phase 2: ways of doing the same parse (docs/phase2-rav1d.md, "Where the
//! time goes"). The buffers of the GPU stages hold the same bytes with every one of them. Two are on
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
}

impl Default for ParseOptions {
    /// `nolfmask` and `tilethread`.
    fn default() -> Self {
        Self::from_bits(Self::DEFAULT)
    }
}

impl ParseOptions {
    /// The switches as bits (`RAV1G_PARSER_OPTION_*` of `rav1g.h`, `AV1_PARSER_OPTION_*` of the C). The
    /// value 1 was a switch that is gone (`stage`) and means nothing.
    pub const NO_LF_MASKS: u32 = 2;
    pub const TILE_KEEPS_THREAD: u32 = 4;
    pub const TIME_CALLS: u32 = 8;
    /// The bits of [`ParseOptions::default`].
    pub const DEFAULT: u32 = Self::NO_LF_MASKS | Self::TILE_KEEPS_THREAD;
    /// The switches that say how the parse is done: all but the timing.
    const WAYS: u32 = Self::NO_LF_MASKS | Self::TILE_KEEPS_THREAD;

    /// No switch: the parse as upstream's rav1d does it.
    pub const NONE: ParseOptions = ParseOptions { no_lf_masks: false, tile_keeps_thread: false, time_calls: false };

    /// The words of the tools (`-parseopt <word>[,<word>...]`).
    pub const WORDS: [(&'static str, u32); 3] =
        [("nolfmask", Self::NO_LF_MASKS), ("tilethread", Self::TILE_KEEPS_THREAD), ("time", Self::TIME_CALLS)];

    pub const fn from_bits(bits: u32) -> Self {
        Self {
            no_lf_masks: bits & Self::NO_LF_MASKS != 0,
            tile_keeps_thread: bits & Self::TILE_KEEPS_THREAD != 0,
            time_calls: bits & Self::TIME_CALLS != 0,
        }
    }

    pub fn bits(self) -> u32 {
        u32::from(self.no_lf_masks) * Self::NO_LF_MASKS
            | u32::from(self.tile_keeps_thread) * Self::TILE_KEEPS_THREAD
            | u32::from(self.time_calls) * Self::TIME_CALLS
    }

    /// A list of words, separated by commas. `nolfmask` and `tilethread` say how the parse is done, and
    /// a list that has one of them, `none` or `default` replaces the default: the parse is done in the
    /// ways the list names and in no other (`none`: in neither; `default`: in the two that are the
    /// default). `time` adds the timing of the sink's calls and leaves the ways as they are, so `time`
    /// alone times the default. `None` for a word that is none of these.
    ///
    /// | List | `nolfmask` | `tilethread` | `time` |
    /// | --- | --- | --- | --- |
    /// | no list, `default` | on | on | |
    /// | `time`, `default,time` | on | on | on |
    /// | `none` | | | |
    /// | `none,time` | | | on |
    /// | `nolfmask` | on | | |
    /// | `tilethread` | | on | |
    /// | `tilethread,time` | | on | on |
    pub fn from_words(list: &str) -> Option<Self> {
        let mut bits = 0;
        let mut ways_named = false;
        for word in list.split(',') {
            let bit = match word {
                "none" => 0,
                "default" => Self::DEFAULT,
                _ => Self::WORDS.iter().find(|(name, _)| *name == word)?.1,
            };
            ways_named |= bit != Self::TIME_CALLS;
            bits |= bit;
        }
        if !ways_named {
            bits |= Self::DEFAULT;
        }
        Some(Self::from_bits(bits))
    }

    /// The words of the switches that are on, as [`Self::from_words`] reads them: `none` in front when
    /// the parse is done in neither of the two ways.
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
        SinkOptions { time_calls: self.time_calls }
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
        assert!(default.no_lf_masks && default.tile_keeps_thread && !default.time_calls);
        assert_eq!(default.bits(), 6);
        assert_eq!(default.words(), "nolfmask,tilethread");
        assert_eq!(ParseOptions::from_words("default"), Some(default));
        assert_eq!(ParseOptions::from_words("nolfmask,tilethread"), Some(default));
        assert_eq!(ParseOptions::from_words("none"), Some(ParseOptions::NONE));
        assert_eq!(ParseOptions::NONE.words(), "none");
        assert_eq!(ParseOptions::NONE.bits(), 0);
        // A way that is named replaces the default; the timing does not.
        let one = ParseOptions::from_words("tilethread").unwrap();
        assert!(one.tile_keeps_thread && !one.no_lf_masks && !one.time_calls);
        let timed = ParseOptions::from_words("time").unwrap();
        assert_eq!(timed, ParseOptions { time_calls: true, ..default });
        assert_eq!(ParseOptions::from_words("default,time"), Some(timed));
        let some = ParseOptions::from_words("tilethread,time").unwrap();
        assert!(some.tile_keeps_thread && some.time_calls && !some.no_lf_masks);
        let none_timed = ParseOptions::from_words("none,time").unwrap();
        assert_eq!(none_timed, ParseOptions { time_calls: true, ..ParseOptions::NONE });
        assert_eq!(none_timed.words(), "none,time");
        for options in [default, ParseOptions::NONE, one, timed, some, none_timed] {
            assert_eq!(ParseOptions::from_bits(options.bits()), options);
            assert_eq!(ParseOptions::from_words(&options.words()), Some(options));
        }
        // The switch that is gone, and words that never were.
        assert_eq!(ParseOptions::from_words("stage"), None);
        assert_eq!(ParseOptions::from_words("all"), None);
        assert_eq!(ParseOptions::from_words("tilethread,other"), None);
        assert_eq!(ParseOptions::from_bits(1), ParseOptions::NONE);
    }
}
