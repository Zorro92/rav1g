// SPDX-License-Identifier: BSD-2-Clause

//! Upstream rav1d on the conformance streams (docs/phase2-rav1d.md, step 1): the base the parser of
//! phase 2 is cut from decodes every stream to its reference.
//!
//! The task list is the one of the conformance run (`-name <name> -i <stream> -check <md5 file>
//! [-aom_md5]` per line). The md5 of a frame is over its visible samples as `rav1g-dec` hashes them:
//! the rows of luma, then of U, then of V; one byte a sample for 8-bit, two bytes, low byte first, above
//! that; for a monochrome stream luma only, or with `-aom_md5` luma and two planes of mid grey, as
//! aomdec writes them. The md5 files are AOM's (one line per frame, the md5 first) or the test player's
//! (the same); the md5 values are compared in order.
//!
//! One line per stream (`RAV1D <name>: PASS frames <n>` or `FAIL ...`), a summary, exit code 1 unless all
//! pass.

#[path = "../../../tools/rav1g-parse-check/src/container.rs"]
mod container;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use md5::{Digest, Md5};
use rav1d::{Decoder, PixelLayout, PlanarImageComponent, Rav1dError, Settings};

struct Task {
    name: String,
    input: PathBuf,
    check: PathBuf,
    aom_md5: bool,
    all_layers: bool,
    operating_point: u8,
}

fn usage() -> ! {
    eprintln!("usage: rav1d-md5 --root <directory of the streams> --manifest <task list> [--threads <n>]");
    std::process::exit(2);
}

fn tasks(manifest: &Path, root: &Path) -> Vec<Task> {
    let text = std::fs::read_to_string(manifest).unwrap_or_else(|error| {
        eprintln!("rav1d-md5: cannot read {}: {error}", manifest.display());
        std::process::exit(2);
    });
    let mut tasks = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let value = |option: &str| words.iter().position(|word| *word == option).and_then(|at| words.get(at + 1));
        let (Some(input), Some(check)) = (value("-i"), value("-check")) else { continue };
        tasks.push(Task {
            name: String::from(*value("-name").unwrap_or(input)),
            input: root.join(input),
            check: root.join(check),
            aom_md5: words.contains(&"-aom_md5"),
            all_layers: words.contains(&"-alllayers"),
            operating_point: value("-oppoint").and_then(|point| point.parse().ok()).unwrap_or(0),
        });
    }
    tasks
}

/// The md5 values of a reference file: the first word of every line that starts with 32 hex digits.
fn reference(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| word.len() == 32 && word.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .collect())
}

fn frame_md5(picture: &rav1d::Picture, aom_md5: bool) -> String {
    let bytes = if picture.bit_depth() == 8 { 1 } else { 2 };
    let (width, height) = (picture.width() as usize, picture.height() as usize);
    let layout = picture.pixel_layout();
    let (chroma_width, chroma_height) = match layout {
        PixelLayout::I444 => (width, height),
        PixelLayout::I422 => (width.div_ceil(2), height),
        _ => (width.div_ceil(2), height.div_ceil(2)),
    };
    let mut md5 = Md5::new();
    let mut rows = |component: PlanarImageComponent, width: usize, height: usize| {
        let plane = picture.plane(component);
        let stride = picture.stride(component) as usize;
        for row in 0..height {
            md5.update(&plane[row * stride..][..width * bytes]);
        }
    };
    rows(PlanarImageComponent::Y, width, height);
    if layout != PixelLayout::I400 {
        rows(PlanarImageComponent::U, chroma_width, chroma_height);
        rows(PlanarImageComponent::V, chroma_width, chroma_height);
    } else if aom_md5 {
        let value = 1u16 << (picture.bit_depth() - 1);
        let grey: Vec<u8> =
            if bytes == 1 { vec![value as u8; chroma_width] } else { value.to_le_bytes().repeat(chroma_width) };
        for _ in 0..2 * chroma_height {
            md5.update(&grey);
        }
    }
    md5.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The md5 of every frame rav1d outputs for the stream.
fn decode(task: &Task, threads: u32) -> Result<Vec<String>, String> {
    let data = std::fs::read(&task.input).map_err(|error| format!("cannot read the stream: {error}"))?;
    let units = container::read(&data)?;
    let mut settings = Settings::new();
    settings.set_n_threads(threads);
    settings.set_max_frame_delay(1);
    settings.set_all_layers(task.all_layers);
    settings.set_operating_point(task.operating_point);
    let mut decoder = Decoder::with_settings(&settings).map_err(|error| format!("rav1d was not created: {error:?}"))?;
    let mut frames = Vec::new();
    let mut drain = |decoder: &mut Decoder, frames: &mut Vec<String>| -> Result<(), String> {
        loop {
            match decoder.get_picture() {
                Ok(picture) => frames.push(frame_md5(&picture, task.aom_md5)),
                Err(Rav1dError::TryAgain) => return Ok(()),
                Err(error) => return Err(format!("frame {}: {error:?}", frames.len())),
            }
        }
    };
    for (index, unit) in units.iter().enumerate() {
        let mut result = decoder.send_data(Box::from(*unit), None, None, None);
        loop {
            match result {
                Ok(()) => break,
                Err(Rav1dError::TryAgain) => {
                    drain(&mut decoder, &mut frames)?;
                    result = decoder.send_pending_data();
                }
                Err(error) => return Err(format!("temporal unit {index}: {error:?}")),
            }
        }
        drain(&mut decoder, &mut frames)?;
    }
    drain(&mut decoder, &mut frames)?;
    Ok(frames)
}

fn main() -> ExitCode {
    let mut root = PathBuf::from(".");
    let mut manifest = None;
    let mut threads = 4;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--root" => root = PathBuf::from(value()),
            "--manifest" => manifest = Some(PathBuf::from(value())),
            "--threads" => threads = value().parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    let tasks = tasks(&manifest.unwrap_or_else(|| usage()), &root);
    if tasks.is_empty() {
        eprintln!("rav1d-md5: no task in the list");
        return ExitCode::from(2);
    }
    let mut passed = 0;
    let mut frames = 0;
    for task in &tasks {
        let result = reference(&task.check).and_then(|reference| {
            let decoded = decode(task, threads)?;
            if let Some(index) = decoded.iter().zip(&reference).position(|(a, b)| a != b) {
                return Err(format!("frame {index}: md5 {}, the reference has {}", decoded[index], reference[index]));
            }
            if decoded.len() != reference.len() || decoded.is_empty() {
                return Err(format!("{} frames, the reference has {}", decoded.len(), reference.len()));
            }
            Ok(decoded.len())
        });
        match result {
            Ok(count) => {
                passed += 1;
                frames += count;
                println!("RAV1D {}: PASS frames {count}", task.name);
            }
            Err(error) => println!("RAV1D {}: FAIL {error}", task.name),
        }
    }
    println!("RAV1D SUMMARY: {passed} of {} streams decode to their reference md5, {frames} frames", tasks.len());
    if passed == tasks.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
