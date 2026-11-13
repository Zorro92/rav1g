// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The switches of the parser of phase 2: ways of doing the same parse that are to be timed against each
//! other on the device (docs/phase2-rav1d.md, "Where the time goes"). The buffers of the GPU stages hold
//! the same bytes with every one of them.

use rav1d::parse::ParseSettings;

use crate::sink::SinkOptions;

/// The switches. All off is the parser as step 3 of the plan left it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParseOptions {
    /// [`SinkOptions::stage`]: the coefficients and the transform block records reach their buffers in
    /// runs.
    pub stage: bool,
    /// rav1d does not fill the masks and levels of its own loop filter (`ParseSettings::lf_masks` off).
    pub no_lf_masks: bool,
    /// `ParseSettings::tile_keeps_thread`: a tile is not handed from thread to thread between its
    /// superblock rows.
    pub tile_keeps_thread: bool,
    /// [`SinkOptions::time_calls`]: the calls of the sink are timed.
    pub time_calls: bool,
}

impl ParseOptions {
    /// The switches as bits (`RAV1G_PARSER_OPTION_*` of `rav1g.h`, `AV1_PARSER_OPTION_*` of the C).
    pub const STAGE: u32 = 1;
    pub const NO_LF_MASKS: u32 = 2;
    pub const TILE_KEEPS_THREAD: u32 = 4;
    pub const TIME_CALLS: u32 = 8;

    /// The words of the tools (`-parseopt <word>[,<word>...]`).
    pub const WORDS: [(&'static str, u32); 4] = [
        ("stage", Self::STAGE),
        ("nolfmask", Self::NO_LF_MASKS),
        ("tilethread", Self::TILE_KEEPS_THREAD),
        ("time", Self::TIME_CALLS),
    ];

    pub fn from_bits(bits: u32) -> Self {
        Self {
            stage: bits & Self::STAGE != 0,
            no_lf_masks: bits & Self::NO_LF_MASKS != 0,
            tile_keeps_thread: bits & Self::TILE_KEEPS_THREAD != 0,
            time_calls: bits & Self::TIME_CALLS != 0,
        }
    }

    pub fn bits(self) -> u32 {
        u32::from(self.stage) * Self::STAGE
            | u32::from(self.no_lf_masks) * Self::NO_LF_MASKS
            | u32::from(self.tile_keeps_thread) * Self::TILE_KEEPS_THREAD
            | u32::from(self.time_calls) * Self::TIME_CALLS
    }

    /// A list of words, separated by commas: `stage`, `nolfmask`, `tilethread`, `time`; `none` for no
    /// switch and `all` for the three that change how the parse is done (not `time`). `None` for a word
    /// that is none of these.
    pub fn from_words(list: &str) -> Option<Self> {
        let mut bits = 0;
        for word in list.split(',') {
            bits |= match word {
                "none" => 0,
                "all" => Self::STAGE | Self::NO_LF_MASKS | Self::TILE_KEEPS_THREAD,
                _ => Self::WORDS.iter().find(|(name, _)| *name == word)?.1,
            };
        }
        Some(Self::from_bits(bits))
    }

    /// The words of the switches that are on, as [`Self::from_words`] reads them.
    pub fn words(self) -> String {
        let words: Vec<&str> =
            Self::WORDS.iter().filter(|(_, bit)| self.bits() & bit != 0).map(|(name, _)| *name).collect();
        if words.is_empty() { String::from("none") } else { words.join(",") }
    }

    /// The part of the sink.
    pub fn sink(self) -> SinkOptions {
        SinkOptions { stage: self.stage, time_calls: self.time_calls }
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
        assert_eq!(ParseOptions::from_words("none"), Some(ParseOptions::default()));
        let all = ParseOptions::from_words("all").unwrap();
        assert_eq!(all.bits(), 7);
        assert_eq!(all.words(), "stage,nolfmask,tilethread");
        let some = ParseOptions::from_words("tilethread,time").unwrap();
        assert!(some.tile_keeps_thread && some.time_calls && !some.stage && !some.no_lf_masks);
        assert_eq!(ParseOptions::from_bits(some.bits()), some);
        assert_eq!(ParseOptions::from_words(&some.words()), Some(some));
        assert_eq!(ParseOptions::default().words(), "none");
        assert_eq!(ParseOptions::from_words("stage,other"), None);
    }
}
