// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What a stream says about the colour of its frames: the colour description of the sequence header
//! ([`ColorInfo`]) and the HDR metadata OBUs ([`MasteringDisplay`], [`ContentLightLevel`]).
//!
//! The decoder does nothing with any of it: the samples of a frame are the same whatever these say.
//! They are for the application that shows the frames (docs/color.md says what to do with each).
//!
//! The code points are the numbers of the AV1 specification (6.4.2), which are those of ITU-T H.273. A
//! code point is kept as the number the stream has, also one the specification reserves: the three
//! types are a number with names for the values that have one. The split into these types follows the
//! interface of rav1d-safe (`ColorInfo` and its parts).

use std::fmt;

use crate::sys;

macro_rules! code_points {
    ($(#[$doc:meta])* $type:ident { $($(#[$item:meta])* $name:ident = $value:literal, $text:literal;)* }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub struct $type(pub u8);

        impl $type {
            $($(#[$item])* pub const $name: $type = $type($value);)*

            /// A short name of the code point, "reserved" for one the specification has no meaning for.
            pub const fn name(self) -> &'static str {
                match self.0 {
                    $($value => $text,)*
                    _ => "reserved",
                }
            }
        }

        impl fmt::Display for $type {
            /// The number and its name: "9 (BT.2020)".
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{} ({})", self.0, self.name())
            }
        }
    };
}

code_points! {
    /// `color_primaries`: the chromaticities of red, green, blue and white the samples are in.
    ColorPrimaries {
        /// BT.709, also sRGB: HD video.
        BT709 = 1, "BT.709";
        /// The stream does not say. An application takes what the container says, or BT.709.
        UNSPECIFIED = 2, "unspecified";
        BT470M = 4, "BT.470 M";
        /// BT.470 B/G: PAL.
        BT470BG = 5, "BT.470 B/G";
        /// BT.601 (SMPTE 170M): NTSC.
        BT601 = 6, "BT.601";
        SMPTE240 = 7, "SMPTE 240M";
        FILM = 8, "generic film";
        /// BT.2020 and BT.2100: UHD and HDR video.
        BT2020 = 9, "BT.2020";
        /// SMPTE ST 428-1: CIE 1931 XYZ.
        XYZ = 10, "XYZ";
        /// SMPTE RP 431-2: DCI P3.
        SMPTE431 = 11, "DCI-P3";
        /// SMPTE EG 432-1: Display P3.
        SMPTE432 = 12, "Display P3";
        EBU3213 = 22, "EBU 3213-E";
    }
}

code_points! {
    /// `transfer_characteristics`: how a sample value relates to light.
    TransferCharacteristics {
        BT709 = 1, "BT.709";
        /// The stream does not say. An application takes what the container says, or BT.709.
        UNSPECIFIED = 2, "unspecified";
        /// BT.470 M: gamma 2.2.
        BT470M = 4, "gamma 2.2";
        /// BT.470 B/G: gamma 2.8.
        BT470BG = 5, "gamma 2.8";
        BT601 = 6, "BT.601";
        SMPTE240 = 7, "SMPTE 240M";
        LINEAR = 8, "linear";
        LOG100 = 9, "log 100:1";
        LOG100_SQRT10 = 10, "log 316:1";
        IEC61966 = 11, "IEC 61966-2-4";
        BT1361 = 12, "BT.1361";
        SRGB = 13, "sRGB";
        BT2020_10BIT = 14, "BT.2020 10-bit";
        BT2020_12BIT = 15, "BT.2020 12-bit";
        /// SMPTE ST 2084, the perceptual quantiser of BT.2100: HDR10, HDR10+, Dolby Vision.
        PQ = 16, "PQ";
        SMPTE428 = 17, "SMPTE 428";
        /// ARIB STD-B67, hybrid log-gamma of BT.2100: HLG.
        HLG = 18, "HLG";
    }
}

code_points! {
    /// `matrix_coefficients`: how luma and chroma are made from red, green and blue.
    MatrixCoefficients {
        /// The planes are green, blue and red (4:4:4 only, which this decoder does not decode).
        IDENTITY = 0, "identity";
        BT709 = 1, "BT.709";
        /// The stream does not say. An application takes what the container says, or BT.709.
        UNSPECIFIED = 2, "unspecified";
        FCC = 4, "FCC";
        BT470BG = 5, "BT.470 B/G";
        BT601 = 6, "BT.601";
        SMPTE240 = 7, "SMPTE 240M";
        YCGCO = 8, "YCgCo";
        /// BT.2020 with non-constant luminance: what HDR10 and HLG video has.
        BT2020_NCL = 9, "BT.2020 NCL";
        BT2020_CL = 10, "BT.2020 CL";
        SMPTE2085 = 11, "SMPTE 2085";
        CHROMA_DERIVED_NCL = 12, "chromaticity-derived NCL";
        CHROMA_DERIVED_CL = 13, "chromaticity-derived CL";
        ICTCP = 14, "ICtCp";
    }
}

/// `color_range`: the sample values black and white have.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ColorRange {
    /// Studio swing: luma from 16 to 235 and chroma from 16 to 240 in 8 bits, times 4 in 10 bits.
    #[default]
    Limited,
    /// Every value: 0 to 255 in 8 bits, 0 to 1023 in 10 bits.
    Full,
}

impl ColorRange {
    /// "limited" or "full".
    pub const fn name(self) -> &'static str {
        match self {
            ColorRange::Limited => "limited",
            ColorRange::Full => "full",
        }
    }
}

/// `chroma_sample_position`: where a chroma sample of a 4:2:0 frame lies among its four luma samples.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ChromaSamplePosition {
    /// The stream does not say (and a monochrome stream has none).
    #[default]
    Unknown = 0,
    /// In the column of the left luma samples, between the two rows: the siting of MPEG-2 and of most
    /// HD video ("left").
    Vertical = 1,
    /// On the top left luma sample: the siting of BT.2020 and BT.2100 video ("top left").
    Colocated = 2,
    /// The value the specification reserves.
    Reserved = 3,
}

impl ChromaSamplePosition {
    /// The value of the bitstream.
    pub const fn from_raw(value: u8) -> ChromaSamplePosition {
        match value {
            0 => ChromaSamplePosition::Unknown,
            1 => ChromaSamplePosition::Vertical,
            2 => ChromaSamplePosition::Colocated,
            _ => ChromaSamplePosition::Reserved,
        }
    }

    /// "unknown", "vertical", "colocated" or "reserved".
    pub const fn name(self) -> &'static str {
        match self {
            ChromaSamplePosition::Unknown => "unknown",
            ChromaSamplePosition::Vertical => "vertical",
            ChromaSamplePosition::Colocated => "colocated",
            ChromaSamplePosition::Reserved => "reserved",
        }
    }
}

/// The colour description of a sequence header (`color_config`): what the samples of its frames mean.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ColorInfo {
    /// The header has a description (`color_description_present_flag`): the three code points are
    /// coded. Without one they are "unspecified", and the application decides (the container, or
    /// BT.709 for video).
    pub description_present: bool,
    pub primaries: ColorPrimaries,
    pub transfer_characteristics: TransferCharacteristics,
    pub matrix_coefficients: MatrixCoefficients,
    /// Always coded, also without a description.
    pub range: ColorRange,
    pub chroma_sample_position: ChromaSamplePosition,
}

impl Default for ColorInfo {
    /// No description: the three code points unspecified, limited range, the chroma position unknown.
    fn default() -> ColorInfo {
        ColorInfo {
            description_present: false,
            primaries: ColorPrimaries::UNSPECIFIED,
            transfer_characteristics: TransferCharacteristics::UNSPECIFIED,
            matrix_coefficients: MatrixCoefficients::UNSPECIFIED,
            range: ColorRange::Limited,
            chroma_sample_position: ChromaSamplePosition::Unknown,
        }
    }
}

impl ColorInfo {
    /// The transfer is PQ or HLG: the frames are HDR and want an HDR output (or a tone map).
    pub const fn is_hdr(&self) -> bool {
        matches!(self.transfer_characteristics, TransferCharacteristics::PQ | TransferCharacteristics::HLG)
    }

    /// BT.2020 primaries with the PQ transfer: what an HDR10 output takes as it is.
    pub const fn is_hdr10(&self) -> bool {
        matches!(self.primaries, ColorPrimaries::BT2020)
            && matches!(self.transfer_characteristics, TransferCharacteristics::PQ)
    }
}

impl fmt::Display for ColorInfo {
    /// "primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2
    /// (colocated)", with ", no colour description" behind it when the header has none.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "primaries {} transfer {} matrix {} range {} chroma pos {} ({})",
            self.primaries,
            self.transfer_characteristics,
            self.matrix_coefficients,
            self.range.name(),
            self.chroma_sample_position as u8,
            self.chroma_sample_position.name()
        )?;
        if !self.description_present {
            f.write_str(", no colour description")?;
        }
        Ok(())
    }
}

/// The mastering display colour volume (`METADATA_TYPE_HDR_MDCV`, SMPTE ST 2086): the display the
/// content was graded on. The members are the numbers of the OBU; the methods give them in their
/// units.
///
/// The fixed point of AV1 is not that of the HEVC SEI message or of `DXGI_HDR_METADATA_HDR10`, which
/// have chromaticities in steps of 0.00002 and luminance in steps of 0.0001 cd/m² (docs/color.md).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct MasteringDisplay {
    /// The chromaticity (x, y) of red, green and blue, in 0.16 fixed point.
    pub primaries: [[u16; 2]; 3],
    /// The chromaticity (x, y) of the white point, in 0.16 fixed point.
    pub white_point: [u16; 2],
    /// The largest luminance in cd/m², in 24.8 fixed point.
    pub max_luminance: u32,
    /// The smallest luminance in cd/m², in 18.14 fixed point.
    pub min_luminance: u32,
}

impl MasteringDisplay {
    /// The chromaticity (x, y) of a primary: 0 red, 1 green, 2 blue.
    pub fn primary(&self, index: usize) -> [f64; 2] {
        self.primaries[index].map(|value| f64::from(value) / 65536.0)
    }

    /// The chromaticity (x, y) of the white point.
    pub fn white(&self) -> [f64; 2] {
        self.white_point.map(|value| f64::from(value) / 65536.0)
    }

    /// The largest luminance in cd/m².
    pub fn max_luminance_nits(&self) -> f64 {
        f64::from(self.max_luminance) / 256.0
    }

    /// The smallest luminance in cd/m².
    pub fn min_luminance_nits(&self) -> f64 {
        f64::from(self.min_luminance) / 16384.0
    }
}

impl fmt::Display for MasteringDisplay {
    /// "R 0.6800,0.3200 G 0.2650,0.6900 B 0.1500,0.0600 W 0.3127,0.3290 L 0.0050..1000 cd/m2 [44564,20972
    /// 17367,45220 9830,3932 20493,21561 256000 82]": the values in their units, then the numbers of the
    /// OBU (red, green, blue, white point, the largest and the smallest luminance).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b] = [self.primary(0), self.primary(1), self.primary(2)];
        let w = self.white();
        write!(
            f,
            "R {:.4},{:.4} G {:.4},{:.4} B {:.4},{:.4} W {:.4},{:.4} L {:.4}..{} cd/m2 [{},{} {},{} {},{} {},{} {} {}]",
            r[0],
            r[1],
            g[0],
            g[1],
            b[0],
            b[1],
            w[0],
            w[1],
            self.min_luminance_nits(),
            self.max_luminance_nits(),
            self.primaries[0][0],
            self.primaries[0][1],
            self.primaries[1][0],
            self.primaries[1][1],
            self.primaries[2][0],
            self.primaries[2][1],
            self.white_point[0],
            self.white_point[1],
            self.max_luminance,
            self.min_luminance
        )
    }
}

/// The content light level (`METADATA_TYPE_HDR_CLL`, CTA-861.3), in cd/m².
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ContentLightLevel {
    /// MaxCLL: the brightest sample of the content.
    pub max_content_light_level: u16,
    /// MaxFALL: the highest average of a frame.
    pub max_frame_average_light_level: u16,
}

impl fmt::Display for ContentLightLevel {
    /// "1000/400": MaxCLL and MaxFALL.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.max_content_light_level, self.max_frame_average_light_level)
    }
}

/// The colour of a frame as one line, for a log: the description, then the two kinds of HDR metadata.
///
/// "primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2
/// (colocated); MDCV R ... [...]; CLL 1000/400", with "no MDCV" and "no CLL" for what a frame does not
/// have.
pub fn color_text(
    color: &ColorInfo,
    mastering_display: Option<&MasteringDisplay>,
    content_light: Option<&ContentLightLevel>,
) -> String {
    let mdcv = mastering_display.map_or_else(|| "no MDCV".to_string(), |mdcv| format!("MDCV {mdcv}"));
    let cll = content_light.map_or_else(|| "no CLL".to_string(), |cll| format!("CLL {cll}"));
    format!("{color}; {mdcv}; {cll}")
}

/// The colour description an output image carries (`aom_image_t`, `crates/rav1g/src/host`).
pub(crate) fn color_of_image(image: &sys::aom_image_t) -> ColorInfo {
    ColorInfo {
        description_present: image.color_description_present != 0,
        primaries: ColorPrimaries(image.cp as u8),
        transfer_characteristics: TransferCharacteristics(image.tc as u8),
        matrix_coefficients: MatrixCoefficients(image.mc as u8),
        range: if image.range as u32 == 0 { ColorRange::Limited } else { ColorRange::Full },
        chroma_sample_position: ChromaSamplePosition::from_raw(image.csp as u8),
    }
}

/// The HDR metadata an output image carries.
pub(crate) fn hdr_of_image(image: &sys::aom_image_t) -> (Option<MasteringDisplay>, Option<ContentLightLevel>) {
    let hdr = &image.hdr;
    let mastering_display = (hdr.has_mdcv != 0).then_some(MasteringDisplay {
        primaries: hdr.mdcv_primaries,
        white_point: hdr.mdcv_white_point,
        max_luminance: hdr.mdcv_luminance_max,
        min_luminance: hdr.mdcv_luminance_min,
    });
    let content_light = (hdr.has_cll != 0).then_some(ContentLightLevel {
        max_content_light_level: hdr.max_cll,
        max_frame_average_light_level: hdr.max_fall,
    });
    (mastering_display, content_light)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_point_keeps_its_number_and_has_a_name() {
        assert_eq!(ColorPrimaries::BT2020.to_string(), "9 (BT.2020)");
        assert_eq!(TransferCharacteristics(16), TransferCharacteristics::PQ);
        assert_eq!(TransferCharacteristics(18).name(), "HLG");
        assert_eq!(MatrixCoefficients(9).to_string(), "9 (BT.2020 NCL)");
        // Values the specification reserves stay what the stream has.
        assert_eq!(ColorPrimaries(3).to_string(), "3 (reserved)");
        assert_eq!(MatrixCoefficients(200).0, 200);
    }

    #[test]
    fn the_line_of_an_hdr10_frame() {
        let color = ColorInfo {
            description_present: true,
            primaries: ColorPrimaries::BT2020,
            transfer_characteristics: TransferCharacteristics::PQ,
            matrix_coefficients: MatrixCoefficients::BT2020_NCL,
            range: ColorRange::Limited,
            chroma_sample_position: ChromaSamplePosition::Colocated,
        };
        assert!(color.is_hdr() && color.is_hdr10());
        let mdcv = MasteringDisplay {
            primaries: [[44564, 20972], [17367, 45220], [9830, 3932]],
            white_point: [20493, 21561],
            max_luminance: 256_000,
            min_luminance: 82,
        };
        let cll = ContentLightLevel { max_content_light_level: 1000, max_frame_average_light_level: 400 };
        assert_eq!(
            color_text(&color, Some(&mdcv), Some(&cll)),
            "primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2 (colocated); \
             MDCV R 0.6800,0.3200 G 0.2650,0.6900 B 0.1500,0.0600 W 0.3127,0.3290 L 0.0050..1000 cd/m2 \
             [44564,20972 17367,45220 9830,3932 20493,21561 256000 82]; CLL 1000/400"
        );
        assert_eq!(
            color_text(&ColorInfo::default(), None, None),
            "primaries 2 (unspecified) transfer 2 (unspecified) matrix 2 (unspecified) range limited chroma pos 0 \
             (unknown), no colour description; no MDCV; no CLL"
        );
        assert!(!ColorInfo::default().is_hdr());
    }
}
