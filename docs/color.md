# Colour: what the decoder reports and what an application does with it

The decoder puts out the samples of the stream: a luma plane and two chroma planes, 8 or 10 bits,
4:2:0. It converts nothing, and nothing it does depends on what the samples mean. What they mean is
in the stream, and an application that shows the frames needs it: without it an HDR10 stream is
shown with the colours and the brightness of an SDR one. A stream in a container has the same
values in the container (`colr`, `mdcv` and `clli` boxes of MP4, `Colour` of Matroska); a raw `.obu`
or `.ivf` stream has them only where the decoder reads them. So the decoder passes them on:

- the **colour description** of the sequence header (`color_config` of the AV1 specification), in
  the stream info and with every frame;
- the **HDR metadata** OBUs (mastering display colour volume, content light level), with every
  frame they go with.

The output of the decoder is the same bytes as before: the values travel next to the frames, and no
stage reads them.

## The values

| | C (`rav1g.h`) | Rust (`rav1g`) |
| --- | --- | --- |
| Of a stream, before a decoder exists | `Rav1gStreamInfo::color` (`rav1g_peek_stream_info`) | `StreamInfo::color` (`peek_stream_info`) |
| Of a frame | `Rav1gFrame::color` | `Frame::color` |
| Colour description | `Rav1gColorInfo`: `description_present`, `primaries`, `transfer_characteristics`, `matrix_coefficients`, `full_range`, `chroma_sample_position` | `ColorInfo`: `description_present`, `primaries: ColorPrimaries`, `transfer_characteristics: TransferCharacteristics`, `matrix_coefficients: MatrixCoefficients`, `range: ColorRange`, `chroma_sample_position: ChromaSamplePosition` |
| Mastering display | `Rav1gFrame::has_mastering_display`, `::mastering_display` (`Rav1gMasteringDisplay`) | `Frame::mastering_display: Option<MasteringDisplay>` |
| Content light level | `Rav1gFrame::has_content_light`, `::content_light` (`Rav1gContentLightLevel`) | `Frame::content_light: Option<ContentLightLevel>` |
| As one line of text | `rav1g_color_text` | `color_text`, and `Display` of the types |

The three code points (`primaries`, `transfer_characteristics`, `matrix_coefficients`) are the
numbers of the stream: those of the AV1 specification (6.4.2), which are those of ITU-T H.273 and of
the `colr` box. In C they are `uint8_t` with `RAV1G_COLOR_PRIMARIES_*`, `RAV1G_TRANSFER_*` and
`RAV1G_MATRIX_*` constants for the values an application meets; in Rust each is a type around the
number (`ColorPrimaries(9)`), with a constant for every value the specification names
(`ColorPrimaries::BT2020`), a `name()` and a `Display` ("9 (BT.2020)"). A value the specification
reserves stays the number it is.

`Rav1gStreamInfo` and `Rav1gFrame` grew by these members, at their ends; an application built
against the header of before has to be built again (the test player is built with the library).

The Rust types follow the split of the colour types of
[rav1d-safe](https://github.com/imazen/rav1d-safe) (`ColorInfo` with a type per property,
`MasteringDisplay`, `ContentLightLevel`); what differs is that a code point here keeps its number,
where an enum has "unspecified" for every value it does not name.

## What to do with each

**`matrix_coefficients` and the range: from Y, U, V to R, G, B.** Every application that shows a
frame needs these two. With `b` bits a sample:

| Range | Luma to 0..1 | Chroma to -0.5..0.5 |
| --- | --- | --- |
| limited (`full_range` false) | `(Y - 16·2^(b-8)) / (219·2^(b-8))` | `(C - 128·2^(b-8)) / (224·2^(b-8))` |
| full | `Y / (2^b - 1)` | `(C - 2^(b-1)) / (2^b - 1)` |

Then `R = Y + 2(1 - Kr)·V`, `B = Y + 2(1 - Kb)·U`, `G = (Y - Kr·R - Kb·B) / (1 - Kr - Kb)`, with the
`Kr`, `Kb` of the matrix:

| `matrix_coefficients` | Kr | Kb |
| --- | --- | --- |
| 1 (BT.709) | 0.2126 | 0.0722 |
| 5 (BT.470 B/G), 6 (BT.601) | 0.299 | 0.114 |
| 9 (BT.2020 NCL) | 0.2627 | 0.0593 |

A full-range stream shown with the limited-range formula has grey blacks and dull whites; the other
way round, crushed blacks and clipped whites. The range is always coded, also in a stream without a
colour description.

**`transfer_characteristics`: what kind of output the frames need.**

- 16 (PQ, SMPTE ST 2084): HDR. With BT.2020 primaries (9) this is HDR10, and the R'G'B' values of
  the conversion above are what an HDR10 output takes: on D3D a swap chain of 10 bits a channel
  with `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`, and the HDR metadata below
  (`SetHDRMetaData`). Shown on an SDR output without a tone map it is dark and without colour.
- 18 (HLG, ARIB STD-B67): HDR of the other kind, which has no mastering metadata. For an HDR10
  output it is converted to PQ (BT.2100: the HLG inverse OETF and OOTF for the display's peak, then
  the PQ inverse EOTF); an output that takes HLG as it is can be given it.
- 1 (BT.709), 6 (BT.601), 14 and 15 (BT.2020): SDR, and the same curve; 13 (sRGB): SDR.
- 2 (unspecified): see below.

`ColorInfo::is_hdr()` (PQ or HLG) and `ColorInfo::is_hdr10()` (PQ with BT.2020 primaries) say it in
Rust.

**`primaries`: the gamut.** The colour space of the output has to have these primaries, or the
application converts: BT.2020 values (9) on an output of BT.709 (1) look washed out without the
conversion, and the other way round too saturated. An HDR10 output is BT.2020.

**Unspecified (2), or no description** (`description_present` false, where all three are 2): the
stream does not say. What a container says comes first. Without one the usual choice is BT.709 for
all three; players take BT.601 for the matrix of small frames (below 720 lines), which is a
convention and no rule of AV1.

**`chroma_sample_position`: where the chroma samples lie.** For the filter that brings chroma to
the size of luma: 1 (vertical; "left" elsewhere) has a chroma sample in the column of the left luma
samples and between the two rows, 2 (colocated; "top left") on the top left luma sample, which is
what BT.2020 video has. 0 is unknown; "left" is the usual choice then. A wrong choice shifts the
colour by half a luma sample, which shows at sharp coloured edges only.

**Mastering display** (`METADATA_TYPE_HDR_MDCV`, SMPTE ST 2086): the display the content was
graded on, for the output's tone map. An HDR10 output takes it as `DXGI_HDR_METADATA_HDR10`,
which has other units than the OBU:

| | AV1 (this interface) | `DXGI_HDR_METADATA_HDR10` | From one to the other |
| --- | --- | --- | --- |
| Chromaticities (red, green, blue, white point) | 0.16 fixed point: `value / 65536` | steps of 0.00002: `value / 50000` | `round(value · 50000 / 65536)` |
| Largest luminance | 24.8 fixed point: `value / 256` cd/m² | whole cd/m² | `value / 256` |
| Smallest luminance | 18.14 fixed point: `value / 16384` cd/m² | steps of 0.0001 cd/m² | `round(value · 10000 / 16384)` |

The primaries are red, green, blue in this order (the HEVC message has green, blue, red). In Rust
`MasteringDisplay::primary(i)`, `white()`, `max_luminance_nits()` and `min_luminance_nits()` give
the values in their units.

**Content light level** (`METADATA_TYPE_HDR_CLL`, CTA-861.3): MaxCLL and MaxFALL in cd/m², the
`MaxContentLightLevel` and `MaxFrameAverageLightLevel` of the same DXGI struct, as they are.

An HDR10 stream need not have either OBU. Without them an application gives the output no
metadata, or the defaults of its platform.

## Which frames the HDR metadata goes with

A metadata OBU is not part of a frame. The rule here is rav1d's (and dav1d's), which both parsers of
the decoder follow:

- An OBU of one of the two kinds holds from where it stands in the stream for every frame behind
  it in decoding order, until another OBU of its kind replaces it.
- A sequence header that differs from the one before starts a new sequence, and both are forgotten.
- A frame has what holds when its last tile group is read. A frame that a later temporal unit
  shows again (`show_existing_frame`) has what holds at that header, not what it had when it was
  coded.

So every frame carries the metadata that is current for it (encoders repeat the OBUs at their key
frames). An application has nothing to keep; one that wants "the latest" takes it from the last
frame it was given. The stream info has no HDR metadata: it is what a sequence header says, and the
OBUs are not in it.

The colour description can change as well, with a new sequence header. A frame has the description
of its own sequence, so a change arrives with the first frame of the new sequence, in output order.
(The decoder asks for a new decoder at a new sequence only when its frames are larger or have more
bits: `NeedsReconfigure`. A change of the colour alone goes through.)

Not passed on: metadata of the type ITU-T T.35 (HDR10+, Dolby Vision), the scalability and the
timecode metadata. Film grain is not a matter for the application: the decoder applies it.

## The two parsers

Both report the same, and CI holds them against each other on every stream.

- **rav1d** (the default) reads the colour description and the two OBUs as upstream does. Its
  sequence header keeps the three code points as enums in which a reserved value becomes
  "unspecified", so the copy in `third_party/rav1d` hands the sink the three numbers as they are
  coded, and a frame and a shown slot come with the HDR metadata upstream attaches to its picture
  (`ParseColorCodes`, `ParseHdrMetadata` of `src/parse.rs`; `PATCHES.md`). Nothing of the parse
  changes.
- **The C parser** (`-parser c`, until the last step of phase 2) had the colour description in its
  sequence header already; it now keeps `color_description_present_flag` and what the two OBUs say
  (it read them and dropped the values), and forgets the OBUs at a sequence header that differs
  from the one before. The same rule, from the same places in the stream.

The stream info comes from neither: `rav1g_peek_stream_info` reads the sequence header with the
small reader of `libav1/av1/common/seq_header_peek.h`, which now goes on to the end of the colour
description (`testing/seq_header_check` holds it against headers written bit by bit from the
syntax tables).

## In the logs

`rav1g-dec` prints, for every stream:

```text
COLOUR <stream> sequence: primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2 (colocated)
COLOUR <stream> frame 0: primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2 (colocated); MDCV R 0.6800,0.3200 G 0.2650,0.6900 B 0.1500,0.0600 W 0.3127,0.3290 L 0.0050..1000 cd/m2 [44564,20972 17367,45220 9830,3932 20493,21561 256000 82]; CLL 1000/400
```

The `sequence` line is the stream info of the sequence header a decoder is created for. The
`frame` line is what a frame carries: printed for the first frame of a decoder, and for every frame
that says something else than the frame before it. `MDCV` has the chromaticities of red, green,
blue and the white point, the smallest and the largest luminance, and in brackets the numbers of
the OBU; `CLL` is MaxCLL/MaxFALL. A stream without a colour description:

```text
COLOUR <stream> frame 0: primaries 2 (unspecified) transfer 2 (unspecified) matrix 2 (unspecified) range limited chroma pos 0 (unknown), no colour description; no MDCV; no CLL
```

The test player logs the `COLOUR frame <n>: ...` line the same way, once per task and at every
change. It does not use the values for its own picture, which is upstream's fixed conversion: it is
a test player (`docs/test-player.md`).

## The check in CI

Job `colour`, `testing/conformance/colour_check.py`: the `COLOUR` lines of three runs of the whole
conformance set (rav1d's parser through the Rust interface and through `rav1g.h`, in the job
`rust-conformance`, and `-parser c`, in `rust-parser-c`)
against two references that are not the decoder:

- FFmpeg's `trace_headers` filter, for the syntax elements of the first sequence header as numbers
  (`color_description_present_flag`, the three code points, `color_range`,
  `chroma_sample_position`): the `sequence` line.
- `ffprobe -show_frames` with libdav1d, for the colour properties and the side data (mastering
  display, content light level) of every output frame: the frames at which they change are the
  `frame` lines the decoder has to print, with those values. The numbers of the mastering display
  are compared exactly (ffprobe prints them as fractions of 65536, 256 and 16384).

What the set carries, which the script also knows by name, so that neither side can pass by saying
nothing:

| Stream | Colour description | HDR metadata |
| --- | --- | --- |
| `nf-dvbdash-d2-10b-2160p.ivf` (10-bit 2160p) | BT.2020, **HLG** (18), BT.2020 NCL, limited, chroma vertical | none: HLG has no mastering metadata |
| `nf-dvbdash-a1-8b-2160p.ivf` (8-bit 2160p) | BT.709 for all three, limited, chroma vertical | none |
| `syn-hdr10-10b-1080p.ivf` (encoded in CI for this) | BT.2020, PQ (16), BT.2020 NCL, limited, chroma colocated | mastering display P3 D65 at 0.005 to 1000 cd/m², MaxCLL 1000, MaxFALL 400 |
| The other 251: the AOM vectors, the other cuts, the other encodes | none (unspecified), limited range | none |

The 10-bit 4K stream of the set is HLG, not PQ, and no stream of Netflix Open Content in the set
has HDR metadata; `syn-hdr10-10b-1080p.ivf` (SVT-AV1 with `mastering-display` and `content-light`,
two key frames, frames shown with `show_existing_frame`) was added for it, as the 254th stream of
the set. The decoder code of the test player runs on it too (`player_smoke.py`), and has to log its
line.
