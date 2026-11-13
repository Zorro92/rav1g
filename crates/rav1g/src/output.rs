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
}

/// What the decoder needs an output image for: a frame that is going to be output.
#[derive(Clone, Copy, Debug)]
pub struct ImageRequest {
    /// Bytes of a buffer that takes the three planes of the frame.
    pub buffer_size: usize,
    /// Luma samples of a row, and rows, of the frame.
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
    pub(crate) offsets: [usize; 3],
    pub(crate) strides: [usize; 3],
    pub(crate) fence: Option<(ID3D12Fence, u64)>,
}

impl Frame {
    /// A plane (0 luma, 1 U, 2 V) in the mapped buffer of the image. `None` for an image without a
    /// mapped buffer, and for the chroma planes of a monochrome frame.
    pub fn plane(&self, index: usize) -> Option<Plane<'_>> {
        if index > 2 || self.image.memory.is_null() || (index > 0 && self.monochrome) {
            return None;
        }
        let (width, height) =
            if index == 0 { (self.width, self.height) } else { (self.width.div_ceil(2), self.height.div_ceil(2)) };
        let stride = self.strides[index];
        let size = stride.checked_mul(height as usize)?;
        if self.offsets[index].checked_add(size)? > self.image.buffer_size {
            return None;
        }
        // SAFETY: the contract of `OutputImage::buffer`: the mapping is readable for the size of the
        // buffer while the frame is alive, and the range was just held against that size.
        let data = unsafe { std::slice::from_raw_parts(self.image.memory.add(self.offsets[index]), size) };
        Some(Plane { data, stride, width, height })
    }

    /// The fence of the command list that wrote the frame into its image, and the value the fence has
    /// when that list has run on the device's queue.
    ///
    /// A frame is handed over when its command list has completed, so the fence has reached the value:
    /// the resources of the image can be read at once, on any queue. The pair is part of the frame so that
    /// an application that orders its own GPU work behind the decoder's by fence can do so with every
    /// frame alike.
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
    /// free: the decoder then waits with it, which is how an application holds the decoder back. `None`
    /// fails the temporal unit.
    fn image(&self, request: &ImageRequest) -> Option<OutputImage>;

    /// A frame is ready: the GPU has written it into its image. Frames arrive in output order.
    fn frame(&self, frame: Frame);

    /// An image the decoder took and did not write a frame into (a decode error behind the request).
    fn unused(&self, image: OutputImage) {
        drop(image);
    }

    /// Every frame that was submitted before a drain has been handed over.
    fn end_of_frames(&self) {}
}
