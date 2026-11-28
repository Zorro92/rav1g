// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The frames a decoder puts out: the images of the application they are written into, and the frame as
//! the application gets it back.

use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, ID3D12Resource};

use crate::color::{ColorInfo, ContentLightLevel, MasteringDisplay};

/// The samples of an output image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum ImageFormat {
    /// One byte a sample.
    Bits8 = 0,
    /// Two bytes a sample, the 10 bits in the low bits.
    Bits10 = 1,
    /// Three samples of 10 bits packed into 32 bits ([`crate::Settings::packed_10x3`]).
    Packed10x3 = 2,
    /// The semi-planar output ([`OutputKind::SemiPlanar`]) of an 8-bit frame: a luma plane and one chroma
    /// plane with U and V side by side, one byte a sample, as an NV12 texture has them.
    Nv12 = 3,
    /// The same for a 10-bit frame, two bytes a sample with the 10 bits in the high bits (the sample
    /// times 64), as a P010 texture has them.
    P010 = 4,
}

impl ImageFormat {
    /// The image has a luma plane and one chroma plane of U and V side by side.
    pub fn is_semi_planar(self) -> bool {
        matches!(self, ImageFormat::Nv12 | ImageFormat::P010)
    }
}

/// What the output images of a decoder are ([`crate::Settings::output_kind`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u32)]
pub enum OutputKind {
    /// The planes one behind the other in a buffer, one texture a plane, or both.
    #[default]
    Planes = 0,
    /// One NV12 texture (8-bit frames) or P010 texture (10-bit frames), which the output pass writes
    /// (docs/output.md); a buffer gets the same two planes one behind the other.
    SemiPlanar = 1,
}

/// What the display pass does to a frame ([`Display::transform`], [`Frame::display_transform`]). The image
/// is BT.709 SDR in every case: BT.709 primaries, BT.1886 transfer, the BT.709 matrix, limited range,
/// chroma sited left and between rows (AV1 `CSP_VERTICAL`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum DisplayTransform {
    /// SDR with BT.709 primaries (or any but BT.2020): downscaled in display-linear light (BT.1886), no
    /// tone map, no gamut step, no clip before the encode.
    Scale = 1,
    /// SDR with BT.2020 primaries: downscaled in display-linear light, converted to BT.709 (BT.2087),
    /// clipped.
    Gamut = 2,
    /// PQ (SMPTE ST 2084): the BT.2390 EETF from the source peak ([`Frame::display_source_peak_nits`]: the
    /// HDR metadata's, else [`Display::source_peak_nits`]) to 2.03 times [`Display::target_white_nits`] on
    /// the largest channel, BT.2020 to BT.709, clipped.
    Pq = 3,
    /// HLG (ARIB STD-B67): the inverse OETF and the OOTF of a 1000-nit display (BT.2100), then as PQ.
    Hlg = 4,
}

impl DisplayTransform {
    /// The transform of a number (`RAV1G_DISPLAY_TRANSFORM_*`): 1 to 4, `None` for any other.
    pub fn from_number(value: u32) -> Option<DisplayTransform> {
        DisplayTransform::from_raw(value)
    }

    pub(crate) fn from_raw(value: u32) -> Option<DisplayTransform> {
        match value {
            1 => Some(DisplayTransform::Scale),
            2 => Some(DisplayTransform::Gamut),
            3 => Some(DisplayTransform::Pq),
            4 => Some(DisplayTransform::Hlg),
            _ => None,
        }
    }
}

/// The tone curve of the display pass ([`Display::tone_curve`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u32)]
pub enum ToneCurve {
    /// ITU-R BT.2390's EETF (the default).
    #[default]
    Bt2390 = 1,
    /// Hable's filmic curve: not in this version; a decoder with it is not created
    /// ([`crate::ErrorCode::Incapable`]), and an application keeps its own stage.
    Hable = 2,
}

/// The last quantisation of the display pass ([`Display::dither`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u32)]
pub enum Dither {
    /// The ordered dither for 8-bit codes (NV12), rounded 10-bit codes (P010).
    #[default]
    Auto = 0,
    /// Rounded at either depth.
    Off = 1,
    /// The ordered dither at either depth.
    Ordered = 2,
}

/// The display pass (docs/stage-fusion.md, docs/integration.md): every output image is the frame
/// downscaled to the display size (area weights in display-linear light), tone-mapped or gamut-converted
/// to BT.709 SDR, written by one pass at the end of the frame's command list into an NV12 or P010
/// texture of that size, in place of the frame itself. The decoded frames and the reference frames are the
/// same bytes with and without it. Width and height 0 (the default) mean no display pass: the images are
/// the frames, as without this field.
///
/// Needs [`OutputKind::SemiPlanar`] and images that are textures created with
/// `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` (no buffer); the device must let shaders write the format.
/// Zero in any other field is its default.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Display {
    /// The size of every image, both rounded up to even; at most 16384. Any ratio to the frame works
    /// (smaller or larger); exactly half (4K to 1080p), the same size (4K to 4K) and two thirds (4K to
    /// 1440p) of the frame in both are the fast paths.
    pub width: u32,
    pub height: u32,
    /// `None`: by the colour of each frame ([`DisplayTransform`]); `Some`: that transform for every frame,
    /// whatever its colour description says.
    pub transform: Option<DisplayTransform>,
    pub tone_curve: ToneCurve,
    /// The PQ and HLG light the EETF maps to the SDR range: 0 is 1000 nits. At most 10000. A PQ frame takes
    /// the peak of its HDR metadata instead when it has one (docs/integration.md, "The source peak"): this
    /// is the peak of HLG, and of PQ without metadata.
    pub source_peak_nits: f32,
    /// The SDR white: the EETF maps to 2.03 times it (203 nits for 100). 0 is 100 nits; up to 4926 (the
    /// application this is made for sets 80 to 400).
    pub target_white_nits: f32,
    /// The HDR metadata the application has for the stream (for example from its container: Matroska's
    /// MaxCLL and the mastering display's largest luminance), nits; 0 for none. The source peak of a PQ
    /// frame takes each where the stream's own metadata OBUs say nothing (or 0): the stream's values win,
    /// as for the assumed colour code points.
    pub max_cll_nits: f32,
    pub mastering_max_nits: f32,
    /// Not in this version: 0. Another value is [`crate::ErrorCode::Incapable`].
    pub desaturation: f32,
    /// The image's codes: 10 (P010), 8 (NV12), or 0: those of the decoder's bit depth
    /// ([`crate::Settings::bit_depth`]), for every frame whatever its sequence's depth.
    pub depth: u32,
    /// The last quantisation: an ordered dither (a threshold at each output sample from the R2 lattice,
    /// docs/integration.md "The dither") or rounding. [`Dither::Auto`]: dithered at 8 bits, rounded at 10.
    pub dither: Dither,
    /// The colour code points (H.273) the pass takes where the sequence header says "unspecified" (2),
    /// as an application merges what its container says: 0 for none (unspecified is BT.709 SDR then).
    pub assumed_primaries: u8,
    pub assumed_transfer_characteristics: u8,
    pub assumed_matrix_coefficients: u8,
    /// The AV1 `chroma_sample_position` taken where the header says 0 (unknown): 0 for none (unknown is
    /// between rows, as 1).
    pub assumed_chroma_sample_position: u8,
}

impl Display {
    /// Whether the settings ask for a display pass.
    pub fn is_on(&self) -> bool {
        self.width != 0 || self.height != 0
    }

    /// The settings with their defaults filled in, as the decoder takes them, for a decoder of
    /// `bit_depth`; an error for settings no decoder takes.
    pub(crate) fn resolve(&self, bit_depth: u32) -> Result<rav1g_sys::av1_display_cfg, crate::Error> {
        use crate::{Error, ErrorCode};
        if !self.is_on() {
            return Ok(rav1g_sys::av1_display_cfg::default());
        }
        let invalid = |what: &str| Err(Error::new(ErrorCode::InvalidParam, format!("display: {what}")));
        let incapable = |what: &str| Err(Error::new(ErrorCode::Incapable, format!("display: {what}")));
        if self.width == 0 || self.height == 0 || self.width > 16384 || self.height > 16384 {
            return invalid(&format!("a size of {}x{}: both 1 to 16384, or both 0", self.width, self.height));
        }
        let nits = |value: f32, default: f32, top: f32| {
            if value == 0.0 {
                Some(default)
            } else {
                (value.is_finite() && value > 0.0 && value <= top).then_some(value)
            }
        };
        let Some(peak) = nits(self.source_peak_nits, 1000.0, 10000.0) else {
            return invalid(&format!("a source peak of {} nits: 0 or 1 to 10000", self.source_peak_nits));
        };
        let Some(white) = nits(self.target_white_nits, 100.0, 10000.0 / 2.03) else {
            return invalid(&format!("a white of {} nits: 0 or up to 4926", self.target_white_nits));
        };
        for (name, value) in [("MaxCLL", self.max_cll_nits), ("mastering display maximum", self.mastering_max_nits)] {
            if !value.is_finite() || value < 0.0 {
                return invalid(&format!("a {name} of {value} nits: 0 (none) or above"));
            }
        }
        if self.tone_curve == ToneCurve::Hable {
            return incapable("the Hable curve is not in this version");
        }
        if !self.desaturation.is_finite() || self.desaturation < 0.0 {
            return invalid(&format!("a desaturation of {}", self.desaturation));
        }
        if self.desaturation != 0.0 {
            return incapable("desaturation is not in this version");
        }
        let depth = match self.depth {
            0 if bit_depth > 8 => 10,
            0 => 8,
            8 | 10 => self.depth,
            other => return invalid(&format!("a depth of {other}: 0, 8 or 10")),
        };
        Ok(rav1g_sys::av1_display_cfg {
            width: (self.width + 1) & !1,
            height: (self.height + 1) & !1,
            transform: self.transform.map_or(rav1g_sys::AV1_DISPLAY_TRANSFORM_AUTO, |t| t as u32),
            source_peak_nits: peak,
            target_white_nits: white,
            depth,
            assumed_primaries: self.assumed_primaries,
            assumed_transfer: self.assumed_transfer_characteristics,
            assumed_matrix: self.assumed_matrix_coefficients,
            assumed_chroma_sample_position: self.assumed_chroma_sample_position,
            dither: u8::from(match self.dither {
                Dither::Auto => depth == 8,
                Dither::Off => false,
                Dither::Ordered => true,
            }),
            max_cll_nits: self.max_cll_nits,
            mastering_max_nits: self.mastering_max_nits,
        })
    }
}

/// What the decoder needs an output image for: a frame that is going to be output.
#[derive(Clone, Copy, Debug)]
pub struct ImageRequest {
    /// Bytes of a buffer that takes the planes of the frame.
    pub buffer_size: usize,
    /// Luma samples of a row, and rows, of the frame. The texture of a semi-planar image
    /// ([`ImageFormat::Nv12`], [`ImageFormat::P010`]) has both rounded up to even.
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
}

/// An output image of the application: where the decoder's GPU writes a frame. A buffer that takes the
/// three planes one behind the other (to read the frame on the CPU), one texture per plane (to use it on
/// the GPU), or both. The decoder keeps the image, and with it the references of its resources, from
/// [`Output::image`] until it hands it back in [`Frame::image`] or through [`Output::unused`].
pub struct OutputImage {
    pub(crate) buffer: Option<ID3D12Resource>,
    pub(crate) memory: *mut u8,
    pub(crate) buffer_size: usize,
    pub(crate) textures: [Option<ID3D12Resource>; 3],
    /// The application's name for the image; the decoder only carries it.
    pub token: usize,
}

// SAFETY: D3D12 resources are free-threaded, and the mapped memory is read by one thread at a time: the
// one that holds the image or the frame.
unsafe impl Send for OutputImage {}

impl OutputImage {
    /// An image that is a buffer: the decoder copies the planes of the frame into it one behind the other
    /// ([`Frame::plane`] says where). `memory` is the mapping of the buffer, null for a buffer the
    /// application does not read on the CPU.
    ///
    /// # Safety
    /// `memory` is null or valid for reading `size` bytes for as long as the decoder has the image and for
    /// as long as the frame it comes back in is alive.
    pub unsafe fn buffer(buffer: ID3D12Resource, memory: *mut u8, size: usize, token: usize) -> OutputImage {
        OutputImage { buffer: Some(buffer), memory, buffer_size: size, textures: [None, None, None], token }
    }

    /// An image that is one texture per plane (luma, U, V) in the copy destination state, of the size and
    /// format of the request: R8 for 8-bit frames, R16 for 10-bit frames, R10G10B10A2 with a third of the
    /// width for [`ImageFormat::Packed10x3`]. A monochrome frame writes the first.
    pub fn textures(textures: [ID3D12Resource; 3], token: usize) -> OutputImage {
        OutputImage { buffer: None, memory: std::ptr::null_mut(), buffer_size: 0, textures: textures.map(Some), token }
    }

    /// Adds the textures to an image that is a buffer: the frame is written to both.
    pub fn with_textures(mut self, textures: [ID3D12Resource; 3]) -> OutputImage {
        self.textures = textures.map(Some);
        self
    }

    /// An image of the semi-planar output ([`OutputKind::SemiPlanar`]): one texture of the format the
    /// request names, `DXGI_FORMAT_NV12` or `DXGI_FORMAT_P010`, of the size of the request with both
    /// rounded up to even, in the state `D3D12_RESOURCE_STATE_COMMON`. It is in that state again when the
    /// frame is handed over. A texture created with `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` is
    /// written by the output pass itself where the device allows it; any other is written by a copy of
    /// each plane (docs/output.md). It may be a texture of another device or API that was opened on the
    /// decoder's device.
    pub fn semi_planar_texture(texture: ID3D12Resource, token: usize) -> OutputImage {
        OutputImage {
            buffer: None,
            memory: std::ptr::null_mut(),
            buffer_size: 0,
            textures: [Some(texture), None, None],
            token,
        }
    }

    /// Adds the semi-planar texture to an image that is a buffer: the frame is written to both, the
    /// texture by copies.
    pub fn with_semi_planar_texture(mut self, texture: ID3D12Resource) -> OutputImage {
        self.textures = [Some(texture), None, None];
        self
    }

    /// The buffer of the image.
    pub fn d3d12_buffer(&self) -> Option<&ID3D12Resource> {
        self.buffer.as_ref()
    }

    /// The texture of a plane.
    pub fn d3d12_texture(&self, plane: usize) -> Option<&ID3D12Resource> {
        self.textures.get(plane).and_then(Option::as_ref)
    }
}

/// A plane of a frame in the buffer of its image.
#[derive(Clone, Copy, Debug)]
pub struct Plane<'a> {
    /// The rows of the plane, `stride` bytes each; the samples of a row are at its start.
    pub data: &'a [u8],
    /// Bytes from one row to the next.
    pub stride: usize,
    /// Samples of a row, and rows.
    pub width: u32,
    pub height: u32,
}

/// A decoded frame, in the image the application gave for it.
pub struct Frame {
    /// The image, as the application made it.
    pub image: OutputImage,
    /// Luma samples of a row, and rows.
    pub width: u32,
    pub height: u32,
    /// Bits a sample: 8 or 10.
    pub bit_depth: u32,
    /// The frame has a luma plane only.
    pub monochrome: bool,
    /// The samples are packed three to 32 bits ([`ImageFormat::Packed10x3`]).
    pub packed_10x3: bool,
    /// The samples of the image, as the request for it said.
    pub format: ImageFormat,
    /// What was passed to [`crate::Decoder::decode`] with the temporal unit of the frame.
    pub user_data: u64,
    /// The colour description of the frame's sequence header. A stream can change it with a new
    /// sequence, so every frame has the one that holds for it.
    pub color: ColorInfo,
    /// The mastering display colour volume that goes with the frame, when the stream has sent one.
    ///
    /// Which frames a metadata OBU goes with is rav1d's rule: every frame from the OBU on, in decoding
    /// order, until another OBU of its kind replaces it or a sequence header that differs from the one
    /// before starts a new sequence; a frame that a later temporal unit shows again
    /// (`show_existing_frame`) has what holds when it is shown. So a frame carries the metadata that is
    /// current for it, and an application has nothing to keep (docs/color.md).
    pub mastering_display: Option<MasteringDisplay>,
    /// The content light level that goes with the frame, by the same rule.
    pub content_light: Option<ContentLightLevel>,
    /// What the display pass did ([`crate::Settings::display`]): `None` when the image is the frame
    /// itself. With a transform, `width`, `height`, `bit_depth` and `format` describe the image (the
    /// display size and depth), `color` is BT.709 SDR as [`DisplayTransform`] says, and what was decoded
    /// is in `source_width`, `source_height` and `source_color`; the HDR metadata stays the stream's.
    pub display_transform: Option<DisplayTransform>,
    /// The frame that was decoded (after super-resolution): its luma samples of a row and rows, and its
    /// sequence header's colour description. Without the display pass the same as `width`, `height`
    /// and `color`.
    pub source_width: u32,
    pub source_height: u32,
    pub source_color: ColorInfo,
    /// The source peak the display pass's tone map took for the frame, nits ([`Display::source_peak_nits`],
    /// docs/integration.md "The source peak"): PQ the HDR metadata's or the configured one, HLG the
    /// configured one; 0 without a tone map (no display pass, SDR).
    pub display_source_peak_nits: f32,
    pub(crate) offsets: [usize; 3],
    pub(crate) strides: [usize; 3],
    pub(crate) fence: Option<(ID3D12Fence, u64)>,
}

impl Frame {
    /// A plane (0 luma, 1 U, 2 V) in the mapped buffer of the image. `None` for an image without a
    /// mapped buffer, and for the chroma planes of a monochrome frame.
    ///
    /// A semi-planar frame ([`Frame::format`]) has two: 0 luma, 1 the plane of U and V side by side,
    /// whose `width` counts the pairs. Its luma plane has one more column behind an odd width and one
    /// more row behind an odd height (repeats of the last), which `width` and `height` do not count but
    /// `data` has. A monochrome frame has plane 1 too, of the middle value.
    pub fn plane(&self, index: usize) -> Option<Plane<'_>> {
        let semi_planar = self.format.is_semi_planar();
        if index > 2
            || self.image.memory.is_null()
            || (index > 0 && self.monochrome && !semi_planar)
            || (index > 1 && semi_planar)
        {
            return None;
        }
        let (width, height) =
            if index == 0 { (self.width, self.height) } else { (self.width.div_ceil(2), self.height.div_ceil(2)) };
        let stride = self.strides[index];
        // The luma plane of a semi-planar frame has its row behind an odd height.
        let rows = if semi_planar && index == 0 { height.next_multiple_of(2) } else { height };
        let size = stride.checked_mul(rows as usize)?;
        if self.offsets[index].checked_add(size)? > self.image.buffer_size {
            return None;
        }
        // SAFETY: the contract of `OutputImage::buffer`: the mapping is readable for the size of the
        // buffer while the frame is alive, and the range was just held against that size.
        let data = unsafe { std::slice::from_raw_parts(self.image.memory.add(self.offsets[index]), size) };
        Some(Plane { data, stride, width, height })
    }

    /// The output fence of the decoder ([`crate::Decoder::output_fence`]), and the value it has when the
    /// command list that wrote the frame into its image has run on the device's queue. The fence is the
    /// same for every frame of a decoder, and the value of a frame is larger than that of the frame
    /// before it.
    ///
    /// A frame is handed over when its command list has completed, so the fence has reached the value:
    /// the resources of the image can be read at once, on any queue. The pair is part of the frame so that
    /// an application that orders its own GPU work behind the decoder's by fence can do so with every
    /// frame alike; one that reads the image on another device or API opens the fence there
    /// ([`crate::Settings::shared_fence`]) and waits for the value on that device's queue.
    pub fn d3d12_fence(&self) -> Option<(&ID3D12Fence, u64)> {
        self.fence.as_ref().map(|(fence, value)| (fence, *value))
    }
}

/// Where a decoder gets its output images and puts its frames. The methods are called from the decoder's
/// threads: [`Output::image`] and [`Output::unused`] on the thread that calls
/// [`crate::Decoder::decode`], [`Output::frame`] and [`Output::end_of_frames`] on the decoder's completion
/// thread.
pub trait Output: Send + Sync {
    /// An image for a frame that is going to be output. The call may wait until the application has one
    /// free: the decoder then waits with it, which is how an application holds the decoder back. `None`:
    /// the frame is not output. It is decoded all the same (a reference of the frames behind it), and
    /// the decode call, or with more than one frame in parse a later call or the drain, returns
    /// [`crate::ErrorCode::Memory`] ("Failed to allocate output frame.") for it. Called on the thread of
    /// the decode calls, or with more than one frame in parse on a thread of the decoder.
    fn image(&self, request: &ImageRequest) -> Option<OutputImage>;

    /// A frame is ready: the GPU has written it into its image. Frames arrive in output order. On the
    /// decoder's completion thread.
    fn frame(&self, frame: Frame);

    /// An image the decoder took and did not write a frame into (a decode error behind the request), on
    /// the thread that asked for it; or one of a frame that a flush dropped ([`crate::Decoder::flush`]),
    /// on the completion thread, when the GPU is through with it.
    fn unused(&self, image: OutputImage) {
        drop(image);
    }

    /// Every frame that was submitted before a drain has been handed over. Not called by a flush.
    fn end_of_frames(&self) {}
}
