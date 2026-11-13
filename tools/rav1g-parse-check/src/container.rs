// SPDX-License-Identifier: BSD-2-Clause

//! The temporal units of a stream file: IVF, and Matroska as far as the test vectors need it (one video
//! track, frames in SimpleBlocks or Blocks without lacing).

/// The frames of the file, each the bytes of a temporal unit.
pub fn read(data: &[u8]) -> Result<Vec<&[u8]>, String> {
    if data.starts_with(b"DKIF") {
        ivf(data)
    } else if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let mut frames = Vec::new();
        matroska(data, &mut frames)?;
        if frames.is_empty() {
            return Err(String::from("no frame found in the Matroska file"));
        }
        Ok(frames)
    } else {
        Err(String::from("neither an IVF nor a Matroska file"))
    }
}

fn ivf(data: &[u8]) -> Result<Vec<&[u8]>, String> {
    let header = data.get(6..8).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]]))).unwrap_or(0);
    if header < 32 || header > data.len() {
        return Err(String::from("not an IVF header"));
    }
    let mut frames = Vec::new();
    let mut at = header;
    while at + 12 <= data.len() {
        let size = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        at += 12;
        let Some(frame) = data.get(at..at + size) else {
            return Err(format!("the IVF file ends inside frame {}", frames.len()));
        };
        frames.push(frame);
        at += size;
    }
    Ok(frames)
}

/// An EBML number: the element ID with its length marker (`keep_marker`) or a size without it. Returns
/// the number and its length in bytes; `None` at the end of the data or for the unknown size.
fn ebml_number(data: &[u8], keep_marker: bool) -> Option<(u64, usize)> {
    let first = *data.first()?;
    if first == 0 {
        return None;
    }
    let length = first.leading_zeros() as usize + 1;
    let bytes = data.get(..length)?;
    let mut value = u64::from(if keep_marker { first } else { first & (0xFF_u32 >> length) as u8 });
    for byte in &bytes[1..] {
        value = (value << 8) | u64::from(*byte);
    }
    if !keep_marker && value == (1u64 << (7 * length)) - 1 {
        return None;
    }
    Some((value, length))
}

/// Walks the elements of `data`; goes into the Segment, its Clusters and their BlockGroups, and takes
/// the frame of every SimpleBlock and Block.
fn matroska<'a>(mut data: &'a [u8], frames: &mut Vec<&'a [u8]>) -> Result<(), String> {
    const SEGMENT: u64 = 0x1853_8067;
    const CLUSTER: u64 = 0x1F43_B675;
    const BLOCK_GROUP: u64 = 0xA0;
    const SIMPLE_BLOCK: u64 = 0xA3;
    const BLOCK: u64 = 0xA1;
    while !data.is_empty() {
        let (id, id_length) = ebml_number(data, true).ok_or("a broken element ID in the Matroska file")?;
        let rest = &data[id_length..];
        // An element of unknown size (a Segment or Cluster written live) runs to the end of its parent.
        let (size, size_length) = match ebml_number(rest, false) {
            Some((size, length)) => (size as usize, length),
            None => {
                let length = rest.first().map_or(1, |first| first.leading_zeros() as usize + 1);
                (rest.len().saturating_sub(length), length)
            }
        };
        let body = rest.get(size_length..size_length + size).ok_or("the Matroska file ends inside an element")?;
        match id {
            SEGMENT | CLUSTER | BLOCK_GROUP => matroska(body, frames)?,
            SIMPLE_BLOCK | BLOCK => {
                // Track number, 16 bits of time, flags; then the frame (no lacing in the test vectors).
                let (_, track_length) = ebml_number(body, false).ok_or("a broken block in the Matroska file")?;
                let flags = *body.get(track_length + 2).ok_or("a broken block in the Matroska file")?;
                if flags & 0x06 != 0 {
                    return Err(String::from("a Matroska block with lacing"));
                }
                frames.push(&body[track_length + 3..]);
            }
            _ => {}
        }
        data = &rest[size_length + size..];
    }
    Ok(())
}
