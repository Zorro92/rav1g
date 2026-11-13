// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! rav1g-dec: the decoder as a desktop console program, for conformance runs.
//!
//! Decodes AV1 streams (IVF, WebM/Matroska, OBU, MP4) with rav1g on a D3D12 device, by default the WARP
//! software adapter, reads every output frame back and computes one md5 per output frame, the way the test
//! player does: the same decoder configuration and defaults (CDEF v2, a decoder sized for the stream, the
//! shared back buffer), the same output buffer callbacks, the same hash. The md5s are written in the
//! player's format and/or compared with a reference file. No window, no swap chain: it runs where there is
//! no GPU, such as a CI runner.
//!
//! It replaces `testing/warp_decoder` and takes its flags, prints its lines and returns its exit codes
//! (docs/warp-decoder.md), so the scripts of `testing/conformance` and `testing/stage_dump` run with
//! either.

mod args;
mod device;
mod library;
mod md5;
mod pool;
mod run;
mod shim;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use library::Library;

use args::{EXIT_ERROR, EXIT_MISMATCH, EXIT_PASS, EXIT_TIMEOUT, EXIT_USAGE, Globals, Task};

/// One line on stdout, written whole and flushed.
pub fn say_text(text: &str) {
    let mut line = String::with_capacity(text.len() + 1);
    line.push_str(text);
    line.push('\n');
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::say_text(&format!($($arg)*))
    };
}

static VERBOSE: AtomicBool = AtomicBool::new(false);

fn decoder_log_line(is_error: bool, line: &str) {
    if is_error || VERBOSE.load(Ordering::Relaxed) {
        say!("{}{}", if is_error { "DECODER ERROR: " } else { "decoder: " }, line);
    }
}

/// -timeout: a task that runs too long gets its result line from here, and the process ends. The frames
/// that came out up to then are read without the task's lock (the task may hold it); the lock on the
/// current task is kept to the end, so the task cannot go away under this thread.
fn watchdog(stop: mpsc::Receiver<()>) {
    use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    loop {
        match stop.recv_timeout(Duration::from_millis(500)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        let current = run::CURRENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(current) = current.as_ref() else {
            continue;
        };
        if current.timeout_s <= 0 {
            continue;
        }
        let elapsed = current.start.elapsed().as_secs_f64();
        if elapsed < current.timeout_s as f64 {
            continue;
        }
        current.run.timeout_line(&current.name, current.timeout_s, elapsed);
        // The decoder's threads may hang: no destructors, no exit handlers.
        // SAFETY: ends this process.
        unsafe {
            let _ = TerminateProcess(GetCurrentProcess(), EXIT_TIMEOUT as u32);
        }
        return;
    }
}

fn real_main() -> i32 {
    shim::process_init();

    let args: Vec<String> = std::env::args_os().skip(1).map(|arg| arg.to_string_lossy().into_owned()).collect();
    if args.is_empty() {
        args::usage();
        return EXIT_USAGE;
    }
    let mut base = Task::default();
    let mut globals = Globals::default();
    if let Err(error) = args::parse_args(&args, &mut base, Some(&mut globals)) {
        say!("ERROR {}", error);
        args::usage();
        return EXIT_USAGE;
    }
    VERBOSE.store(globals.verbose, Ordering::Relaxed);

    let mut tasks: Vec<Task> = Vec::new();
    if !globals.list.is_empty() {
        let Ok(bytes) = std::fs::read(&globals.list) else {
            say!("ERROR cannot read the list {}", globals.list);
            return EXIT_USAGE;
        };
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.split('\n').enumerate() {
            let parts = args::split_line(line);
            if parts.is_empty() || parts[0].starts_with('#') {
                continue;
            }
            let mut task = base.clone();
            if let Err(error) = args::parse_args(&parts, &mut task, None).and_then(|()| args::check_task(&mut task)) {
                say!("ERROR {}, line {}: {}", globals.list, index + 1, error);
                return EXIT_USAGE;
            }
            tasks.push(task);
        }
        if tasks.is_empty() {
            say!("ERROR no tasks in the list {}", globals.list);
            return EXIT_USAGE;
        }
    } else if globals.sm6test && base.input.is_empty() {
        // -sm6test alone: the test needs a device and no stream.
    } else {
        if let Err(error) = args::check_task(&mut base) {
            say!("ERROR {}", error);
            args::usage();
            return EXIT_USAGE;
        }
        tasks.push(base);
    }

    shim::set_log_verbose(globals.verbose);
    library::set_log(globals.capi, decoder_log_line);

    let Some(dev) = device::create(&globals) else {
        return EXIT_USAGE;
    };
    // What the device reports about itself ("CAPS ..."), at every launch, and with -sm6test the Shader
    // Model 6 test shaders ("SM6TEST ..."), before anything of the decoder exists (testing/sm6test). A
    // shader the device refuses is a result of the test; the exit code changes only when the test itself
    // broke.
    // SAFETY: the device and the queue are valid.
    unsafe { shim::sm6_log_caps(dev.device_ptr()) };
    let mut sm6_broken = false;
    if globals.sm6test {
        // SAFETY: as above.
        sm6_broken = unsafe { shim::sm6_run_tests(dev.device_ptr(), dev.queue_ptr()) };
        if globals.debug_layer {
            device::print_debug_messages(&dev.device);
        }
        if tasks.is_empty() {
            return if sm6_broken { EXIT_ERROR } else { EXIT_PASS };
        }
    }
    let pso_start = Instant::now();
    // The decoder's device: the program's D3D12 device and queue, and the pipeline states on it.
    let Some(library) = Library::open(&dev, globals.capi, args::PSO_THREADS as u32) else {
        say!("ERROR the compute pipeline states could not be created");
        return EXIT_USAGE;
    };
    say!("PIPELINE states created in {:.0} ms", pso_start.elapsed().as_secs_f64() * 1000.0);
    if globals.capi {
        say!("INTERFACE the decoder is used through the functions of rav1g.h (-capi)");
    }
    let variant = library.shader_variant();
    say!("SHADERS {}", if variant.is_empty() { "normal build" } else { &variant });
    if globals.debug_layer {
        device::print_debug_messages(&dev.device);
    }

    let (stop_watchdog, stop) = mpsc::channel::<()>();
    let watchdog_thread = std::thread::spawn(move || watchdog(stop));

    let mut exit_code = EXIT_PASS;
    for (index, task) in tasks.iter().enumerate() {
        let code = run::run_task(task, &dev, &library);
        if globals.debug_layer {
            device::print_debug_messages(&dev.device);
        }
        if code == EXIT_ERROR {
            exit_code = EXIT_ERROR;
            if globals.keep_going {
                // -keep_going: what the decoder left behind the error is what the next task starts on.
                continue;
            }
            // The decoder was destroyed, but a failed decode may leave the device or the process in a state
            // the next task should not inherit: the run ends here and says what was not run.
            if index + 1 < tasks.len() {
                say!("STOPPED after a decode error; {} tasks not run", tasks.len() - index - 1);
            }
            break;
        }
        if code == EXIT_MISMATCH && exit_code == EXIT_PASS {
            exit_code = EXIT_MISMATCH;
        }
    }

    drop(stop_watchdog);
    let _ = watchdog_thread.join();
    drop(library);
    drop(dev);
    if sm6_broken && exit_code == EXIT_PASS {
        exit_code = EXIT_ERROR;
    }
    exit_code
}

fn main() {
    std::process::exit(real_main());
}
