// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The pipeline cache on disk (docs/integration.md, "The pipeline cache"): the compiled pipeline states of
//! a device in a file of a folder of the application, so that the next device on the same adapter and
//! driver loads them instead of compiling them again.
//!
//! One file, `rav1g-pipelines-<key>.bin`: `<key>` is 16 hex digits of a fingerprint of everything the
//! pipelines are made of (the bytecode of every shader of the set in use, the serialized root signatures,
//! the names of the pipelines, the version of the library). The file is a header and one of two payloads:
//!
//! - a D3D12 pipeline library (`ID3D12PipelineLibrary`), where the device has them
//!   (`D3D12_SHADER_CACHE_SUPPORT_LIBRARY`);
//! - otherwise, the cached blob of every pipeline state (`GetCachedBlob`) by its name, where the device
//!   has those (`D3D12_SHADER_CACHE_SUPPORT_SINGLE_PSO`).
//!
//! A missing file is a miss, a file that is damaged, of another kind or that the device refuses (another
//! driver or adapter) is rebuilt: the pipelines are compiled and the file written again, as a temporary
//! file renamed over the final name, and the other `rav1g-pipelines-*` files of the folder are deleted.
//! Nothing of the cache fails a device creation. One log line per device says what happened
//! (`PIPELINES ...`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Instant;

use super::log::log;
use crate::gpu::{self, Device, Layout, Pipeline, PipelineLibrary};

/// The first bytes of a file.
const MAGIC: [u8; 8] = *b"RAV1GPC\0";
/// The layout of the file; a file of another one is rebuilt.
const FORMAT: u32 = 1;
/// The bytes in front of the payload: magic, format, kind, key, payload size, payload checksum.
const HEADER_SIZE: usize = 8 + 4 + 4 + 8 + 8 + 8;

/// What the payload of a file is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// A serialized pipeline library.
    Library = 1,
    /// The cached blobs of the pipeline states by name.
    Blobs = 2,
}

impl Kind {
    fn from_raw(value: u32) -> Option<Kind> {
        match value {
            1 => Some(Kind::Library),
            2 => Some(Kind::Blobs),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Library => "pipeline library",
            Kind::Blobs => "cached pipeline states",
        }
    }
}

/// FNV-1a, 64 bits, continued from `hash`.
fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// The key of the pipelines of a device: a fingerprint of the version of the library and of the name, the
/// bytecode and the serialized layout of every pipeline, in their order.
pub(crate) fn key<'a>(pipelines: impl Iterator<Item = (&'a str, &'a [u8], &'a [u8])>) -> u64 {
    let mut hash = fnv(FNV_START, b"rav1g pipeline cache\0");
    hash = fnv(hash, &FORMAT.to_le_bytes());
    for part in [env!("CARGO_PKG_VERSION"), crate::sys::SHADER_SET] {
        hash = fnv(hash, part.as_bytes());
        hash = fnv(hash, b"\0");
    }
    for (name, bytecode, layout) in pipelines {
        hash = fnv(hash, name.as_bytes());
        hash = fnv(hash, b"\0");
        hash = fnv(hash, &(bytecode.len() as u64).to_le_bytes());
        hash = fnv(hash, bytecode);
        hash = fnv(hash, &(layout.len() as u64).to_le_bytes());
        hash = fnv(hash, layout);
    }
    hash
}

/// The name of the file of a key.
pub(crate) fn file_name(key: u64) -> String {
    format!("rav1g-pipelines-{key:016x}.bin")
}

/// A file of the cache: its header and its payload.
pub(crate) fn encode(kind: Kind, key: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_SIZE + payload.len());
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&FORMAT.to_le_bytes());
    bytes.extend_from_slice(&(kind as u32).to_le_bytes());
    bytes.extend_from_slice(&key.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&fnv(FNV_START, payload).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// The kind of a file of the cache, with the file cut down to its payload; or why the file cannot be used.
pub(crate) fn decode(mut bytes: Vec<u8>, key: u64) -> Result<(Kind, Vec<u8>), String> {
    let field = |at: usize, size: usize| -> u64 {
        let mut value = [0u8; 8];
        value[..size].copy_from_slice(&bytes[at..at + size]);
        u64::from_le_bytes(value)
    };
    if bytes.len() < HEADER_SIZE || bytes[..8] != MAGIC {
        return Err(format!("the file is damaged: no header ({} bytes)", bytes.len()));
    }
    let format = field(8, 4);
    if format != u64::from(FORMAT) {
        return Err(format!("the file is of format {format}, not {FORMAT}"));
    }
    let kind = field(12, 4);
    let Some(kind) = Kind::from_raw(kind as u32) else {
        return Err(format!("the file is damaged: kind {kind}"));
    };
    if field(16, 8) != key {
        return Err(format!("the file has the key {:016x}", field(16, 8)));
    }
    let size = field(24, 8);
    if (bytes.len() - HEADER_SIZE) as u64 != size {
        return Err(format!(
            "the file is damaged: {} bytes of payload, the header says {size}",
            bytes.len() - HEADER_SIZE
        ));
    }
    let checksum = field(32, 8);
    bytes.drain(..HEADER_SIZE);
    if fnv(FNV_START, &bytes) != checksum {
        return Err("the file is damaged: the checksum differs".to_string());
    }
    Ok((kind, bytes))
}

/// The payload of [`Kind::Blobs`]: the number of pipelines, then the name and the blob of each, every
/// size as 4 bytes.
pub(crate) fn encode_blobs(blobs: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
    for (name, blob) in blobs {
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        bytes.extend_from_slice(blob);
    }
    bytes
}

pub(crate) fn decode_blobs(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    fn take<'a>(rest: &mut &'a [u8], size: usize) -> Option<&'a [u8]> {
        let (head, tail) = rest.split_at_checked(size)?;
        *rest = tail;
        Some(head)
    }
    fn size(rest: &mut &[u8]) -> Option<usize> {
        Some(u32::from_le_bytes(take(rest, 4)?.try_into().ok()?) as usize)
    }
    let mut rest = bytes;
    let blobs = (|| {
        let count = size(&mut rest)?;
        let mut blobs = BTreeMap::new();
        for _ in 0..count {
            let length = size(&mut rest)?;
            let name = String::from_utf8(take(&mut rest, length)?.to_vec()).ok()?;
            let length = size(&mut rest)?;
            blobs.insert(name, take(&mut rest, length)?.to_vec());
        }
        rest.is_empty().then_some(blobs)
    })();
    blobs.ok_or_else(|| "the file is damaged: the list of cached pipeline states".to_string())
}

/// The bytes of a file. Not `std::fs::read`: the standard library reads a file with `NtReadFile` of
/// `ntdll.dll`, which a program of an app container should not import (testing/uwp_imports); `ReadFile`
/// is a function of the API sets of `WindowsApp.lib`.
fn read_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, GetFileSizeEx, OPEN_EXISTING, ReadFile,
    };
    let error = |error: windows::core::Error| {
        let code = error.code().0 as u32;
        if code >> 16 == 0x8007 {
            std::io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            std::io::Error::other(format!("hr 0x{code:08X}"))
        }
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: a C string of UTF-16; the handle is closed below.
    let file = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(error)?;
    // SAFETY: an open file; every read writes into the part of `bytes` it is given.
    let read = unsafe {
        (|| {
            let mut size = 0i64;
            GetFileSizeEx(file, &mut size).map_err(error)?;
            let size = usize::try_from(size).map_err(std::io::Error::other)?;
            let mut bytes = vec![0u8; size];
            let mut at = 0;
            while at < size {
                let chunk = (size - at).min(1 << 30);
                let mut count = 0u32;
                ReadFile(file, Some(&mut bytes[at..at + chunk]), Some(&mut count), None).map_err(error)?;
                if count == 0 {
                    bytes.truncate(at); // shorter than it was: what is there
                    break;
                }
                at += count as usize;
            }
            Ok(bytes)
        })()
    };
    // SAFETY: the handle of this call.
    let _ = unsafe { CloseHandle(file) };
    read
}

/// The file of `key` in `dir`: its payload, `Ok(None)` when there is none, or why it cannot be used.
pub(crate) fn read_file(dir: &Path, key: u64) -> Result<Option<(Kind, Vec<u8>)>, String> {
    match read_bytes(&dir.join(file_name(key))) {
        Ok(bytes) => decode(bytes, key).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("the file could not be read: {error}")),
    }
}

/// Writes the file of `key` into `dir` (created when it is not there): a temporary file in the folder,
/// renamed over the final name. Then deletes the other files of the cache in the folder. Returns the size
/// of the file.
pub(crate) fn write_file(dir: &Path, key: u64, kind: Kind, payload: &[u8]) -> Result<usize, String> {
    use std::io::Write;
    std::fs::create_dir_all(dir).map_err(|error| format!("the folder could not be created: {error}"))?;
    let name = file_name(key);
    let temporary = format!("rav1g-pipelines-{key:016x}.{}.tmp", std::process::id());
    let bytes = encode(kind, key, payload);
    let written = (|| {
        let mut file = std::fs::File::create(dir.join(&temporary))?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written.and_then(|()| std::fs::rename(dir.join(&temporary), dir.join(&name))) {
        let _ = std::fs::remove_file(dir.join(&temporary));
        return Err(format!("the file could not be written: {error}"));
    }
    // The files of other keys (another version of the library, another shader pick) and temporary files
    // that were left behind.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let other = entry.file_name();
            let Some(other) = other.to_str() else { continue };
            if other != name
                && other.starts_with("rav1g-pipelines-")
                && (other.ends_with(".bin") || other.ends_with(".tmp"))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(bytes.len())
}

/// What a code of a refused blob means.
fn refusal(code: i32) -> String {
    let what = match code as u32 {
        0x8007_0057 => "damaged or unknown",
        0x887E_0001 => "of another adapter",
        0x887E_0002 => "of another driver or runtime",
        _ => "refused",
    };
    format!("hr 0x{:08X}, {what}", code as u32)
}

/// What the device keeps the pipelines in.
enum Store {
    /// No cache was asked for.
    Off,
    /// The device keeps no pipelines.
    Unsupported,
    /// A pipeline library: the one of the file, or none (then everything is compiled).
    Library(Option<PipelineLibrary>),
    /// The cached blobs of the file by name (empty without a file).
    Blobs(BTreeMap<String, Vec<u8>>),
}

/// The pipeline cache of one device creation, from the start of the creation of its pipelines to the
/// log line behind the last one. Shared by the threads that create the pipelines.
pub(crate) struct PipelineCache {
    start: Instant,
    dir: Option<PathBuf>,
    key: u64,
    store: Store,
    /// Why the file of the key was not used; `None` when it was, or when there was none.
    rejected: Option<String>,
    /// There was a file and the device took it.
    found: bool,
    loaded: AtomicUsize,
    refused: AtomicUsize,
    first_refusal: AtomicI32,
}

impl PipelineCache {
    /// Reads the file of `key` in `dir` (`None`: no cache) for the pipelines of `device`. `start` is when
    /// the creation of the pipelines started.
    pub(crate) fn open(device: Device<'_>, dir: Option<&Path>, key: u64, start: Instant) -> PipelineCache {
        let mut cache = PipelineCache {
            start,
            dir: dir.map(Path::to_path_buf),
            key,
            store: Store::Off,
            rejected: None,
            found: false,
            loaded: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            first_refusal: AtomicI32::new(0),
        };
        let Some(dir) = dir else {
            return cache;
        };
        let support = device.shader_cache_support();
        let kind = if support.library {
            Kind::Library
        } else if support.single_pipeline {
            Kind::Blobs
        } else {
            cache.store = Store::Unsupported;
            return cache;
        };
        let file = match read_file(dir, key) {
            Ok(file) => file,
            Err(reason) => {
                cache.rejected = Some(reason);
                None
            }
        };
        cache.store = match kind {
            Kind::Library => Store::Library(None),
            Kind::Blobs => Store::Blobs(BTreeMap::new()),
        };
        let Some((file_kind, payload)) = file else {
            return cache;
        };
        if file_kind != kind {
            cache.rejected = Some(format!("the file has {}, the device takes {}", file_kind.label(), kind.label()));
            return cache;
        }
        match kind {
            Kind::Library => match device.create_pipeline_library(payload) {
                Ok(library) => {
                    cache.store = Store::Library(Some(library));
                    cache.found = true;
                }
                Err(error) => {
                    cache.rejected = Some(format!("the device refused the pipeline library: {}", refusal(error.code)));
                }
            },
            Kind::Blobs => match decode_blobs(&payload) {
                Ok(blobs) => {
                    cache.store = Store::Blobs(blobs);
                    cache.found = true;
                }
                Err(reason) => cache.rejected = Some(reason),
            },
        }
        cache
    }

    fn refuse(&self, error: gpu::Error) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        let _ = self.first_refusal.compare_exchange(0, error.code, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// The pipeline `name`: from the cache, or compiled when the cache does not have it. A pipeline is
    /// asked for once (several threads ask for different ones at the same time).
    pub(crate) fn create(
        &self,
        device: Device<'_>,
        name: &str,
        layout: &Layout,
        bytecode: &[u8],
    ) -> Result<Pipeline, gpu::Error> {
        let cached = match &self.store {
            Store::Library(Some(library)) => match library.load(name, layout, bytecode) {
                Ok(pipeline) => Some(pipeline),
                Err(error) => {
                    self.refuse(error);
                    None
                }
            },
            Store::Blobs(blobs) => match blobs.get(name) {
                Some(blob) => match device.create_pipeline_from_cache(layout, bytecode, blob) {
                    Ok(pipeline) => Some(pipeline),
                    Err(error) => {
                        self.refuse(error);
                        None
                    }
                },
                None => {
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    None
                }
            },
            _ => None,
        };
        match cached {
            Some(pipeline) => {
                self.loaded.fetch_add(1, Ordering::Relaxed);
                Ok(pipeline)
            }
            None => device.create_pipeline(layout, bytecode),
        }
    }

    /// Behind the last pipeline: writes the file when the cache did not have every pipeline, and the log
    /// line. `pipelines` are all of the device by name, `None` for one that was not created (the file is
    /// then not written).
    pub(crate) fn finish(&self, device: Device<'_>, pipelines: &[(&str, Option<Pipeline>)]) {
        let elapsed = self.start.elapsed().as_secs_f64() * 1000.0;
        let total = pipelines.len();
        let created = pipelines.iter().filter(|(_, pipeline)| pipeline.is_some()).count();
        let count =
            if created == total { format!("{total} pipelines") } else { format!("{created} of {total} pipelines") };
        let kind = match &self.store {
            Store::Off => {
                log(false, &format!("PIPELINES no cache: {count} in {elapsed:.0} ms"));
                return;
            }
            Store::Unsupported => {
                log(
                    false,
                    &format!(
                        "PIPELINES cache unsupported (the device keeps neither pipeline libraries nor cached \
                         pipeline states): {count} in {elapsed:.0} ms"
                    ),
                );
                return;
            }
            Store::Library(_) => Kind::Library,
            Store::Blobs(_) => Kind::Blobs,
        };
        let loaded = self.loaded.load(Ordering::Relaxed);
        let refused = self.refused.load(Ordering::Relaxed);
        let outcome = if self.found && loaded == total {
            "hit".to_string()
        } else if let Some(reason) = &self.rejected {
            format!("rebuilt ({reason})")
        } else if self.found {
            let code = self.first_refusal.load(Ordering::Relaxed);
            let first = if code != 0 { format!(", first {}", refusal(code)) } else { String::new() };
            format!("rebuilt ({refused} of {total} pipelines not taken from the file{first})")
        } else {
            "miss".to_string()
        };
        let mut detail = format!("{}, key {:016x}", kind.label(), self.key);
        if self.found && loaded != total {
            detail += &format!(", {loaded} loaded");
        }
        if loaded != total {
            detail += "; ";
            detail += &self.write(device, kind, pipelines);
        }
        log(false, &format!("PIPELINES cache {outcome}: {count} in {elapsed:.0} ms ({detail})"));
    }

    /// Writes the file of all pipelines; what was done, for the log line.
    fn write(&self, device: Device<'_>, kind: Kind, pipelines: &[(&str, Option<Pipeline>)]) -> String {
        let Some(dir) = &self.dir else {
            return String::new();
        };
        let start = Instant::now();
        let all: Option<Vec<(&str, &Pipeline)>> =
            pipelines.iter().map(|(name, pipeline)| Some((*name, pipeline.as_ref()?))).collect();
        let Some(all) = all else {
            return "not written: a pipeline could not be created".to_string();
        };
        let payload = match kind {
            // A new library with every pipeline: the one of the file (if any) may have pipelines of the
            // same names that the device did not take.
            Kind::Library => device.create_pipeline_library(Vec::new()).and_then(|library| {
                for (name, pipeline) in &all {
                    library.store(name, pipeline)?;
                }
                library.serialize()
            }),
            Kind::Blobs => all
                .iter()
                .map(|(name, pipeline)| Ok((*name, pipeline.cached_blob()?)))
                .collect::<Result<Vec<_>, gpu::Error>>()
                .map(|blobs| encode_blobs(&blobs)),
        };
        let payload = match payload {
            Ok(payload) => payload,
            Err(error) => return format!("not written: {} hr 0x{:08X}", error.step, error.code as u32),
        };
        match write_file(dir, self.key, kind, &payload) {
            Ok(size) => {
                format!("written: {} KiB in {:.0} ms", size.div_ceil(1024), start.elapsed().as_secs_f64() * 1000.0)
            }
            Err(reason) => format!("not written: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rav1g-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn keys_change_with_every_part() {
        let base = key([("a", &b"code"[..], &b"layout"[..]), ("b", &b"x"[..], &b"y"[..])].into_iter());
        assert_eq!(base, key([("a", &b"code"[..], &b"layout"[..]), ("b", &b"x"[..], &b"y"[..])].into_iter()));
        for other in [
            key([("a", &b"code"[..], &b"layout"[..])].into_iter()),
            key([("a", &b"cod3"[..], &b"layout"[..]), ("b", &b"x"[..], &b"y"[..])].into_iter()),
            key([("a", &b"code"[..], &b"layouT"[..]), ("b", &b"x"[..], &b"y"[..])].into_iter()),
            key([("A", &b"code"[..], &b"layout"[..]), ("b", &b"x"[..], &b"y"[..])].into_iter()),
            key([("b", &b"x"[..], &b"y"[..]), ("a", &b"code"[..], &b"layout"[..])].into_iter()),
            // The bytes move from one part to the next.
            key([("a", &b"cod"[..], &b"elayout"[..]), ("b", &b"x"[..], &b"y"[..])].into_iter()),
        ] {
            assert_ne!(base, other);
        }
        assert_eq!(file_name(0x0123_4567_89ab_cdef), "rav1g-pipelines-0123456789abcdef.bin");
    }

    #[test]
    fn files_are_checked() {
        let file = encode(Kind::Library, 7, b"payload");
        assert_eq!(decode(file.clone(), 7), Ok((Kind::Library, b"payload".to_vec())));
        assert!(decode(file.clone(), 8).unwrap_err().contains("key"));
        // Cut short anywhere: rebuilt, never taken.
        for length in 0..file.len() {
            assert!(decode(file[..length].to_vec(), 7).is_err(), "cut to {length} bytes");
        }
        let mut flipped = file.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert_eq!(decode(flipped, 7).unwrap_err(), "the file is damaged: the checksum differs");
        let mut format = file.clone();
        format[8] = 9;
        assert!(decode(format, 7).unwrap_err().contains("format 9"));
        assert!(decode(b"something else entirely, and long enough for a header".to_vec(), 7).is_err());
    }

    #[test]
    fn blob_lists() {
        let blobs = vec![("idct4x4", vec![1u8, 2, 3]), ("inter_main", Vec::new())];
        let payload = encode_blobs(&blobs);
        let read = decode_blobs(&payload).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read["idct4x4"], [1, 2, 3]);
        assert!(read["inter_main"].is_empty());
        for length in 0..payload.len() {
            assert!(decode_blobs(&payload[..length]).is_err(), "cut to {length} bytes");
        }
        let mut longer = payload.clone();
        longer.push(0);
        assert!(decode_blobs(&longer).is_err());
    }

    #[test]
    fn files_are_written_and_the_others_deleted() {
        let dir = temporary_dir("cache").join("rav1g");
        assert_eq!(read_file(&dir, 1), Ok(None));
        // The folder is created.
        write_file(&dir, 1, Kind::Blobs, b"one").unwrap();
        assert_eq!(read_file(&dir, 1), Ok(Some((Kind::Blobs, b"one".to_vec()))));
        std::fs::write(dir.join("rav1g-pipelines-00000000000000aa.123.tmp"), b"left behind").unwrap();
        std::fs::write(dir.join("something-else.bin"), b"not ours").unwrap();
        write_file(&dir, 2, Kind::Library, b"two").unwrap();
        let mut names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["rav1g-pipelines-0000000000000002.bin", "something-else.bin"]);
        // Written again over the same name.
        write_file(&dir, 2, Kind::Library, b"three").unwrap();
        assert_eq!(read_file(&dir, 2), Ok(Some((Kind::Library, b"three".to_vec()))));
        // Truncated: rebuilt.
        let path = dir.join(file_name(2));
        let length = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(length / 2).unwrap();
        assert!(read_file(&dir, 2).unwrap_err().starts_with("the file is damaged"));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
