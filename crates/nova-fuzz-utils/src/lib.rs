//! Shared helpers for Nova fuzz targets.
//!
//! Keep this crate dependency-free (std-only): fuzz workspaces are intentionally tiny and often
//! compile in isolation.

use std::marker::PhantomData;
use std::panic::Location;
use std::str;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[track_caller]
fn lock<'a, T>(mutex: &'a Mutex<T>, context: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            eprintln!(
                "mutex poisoned; continuing with recovered guard: context={context} file={} line={} column={} error={err}",
                loc.file(),
                loc.line(),
                loc.column(),
            );
            err.into_inner()
        }
    }
}

/// Maximum size of a fuzz input accepted by per-crate fuzz harnesses.
///
/// This matches the repository's other per-crate harness caps and prevents `cargo fuzz ... -max_len ...`
/// from driving huge allocations or quadratic behavior via attacker-controlled lengths.
pub const MAX_INPUT_SIZE: usize = 256 * 1024; // 256 KiB

/// Wall-clock timeout per fuzz input.
pub const TIMEOUT: Duration = Duration::from_secs(1);

/// Returns a UTF-8 view of `data` truncated to `MAX_INPUT_SIZE`.
///
/// The fuzzer input is capped to avoid OOM and quadratic behavior on pathological inputs. If the
/// truncated data is not valid UTF-8, we only try trimming up to 3 bytes to recover from cutting a
/// multibyte codepoint.
#[inline]
pub fn truncate_utf8(data: &[u8]) -> Option<&str> {
    let cap = data.len().min(MAX_INPUT_SIZE);
    // If `cap` splits a multibyte codepoint we may need to trim a few bytes.
    for trim in 0..=3 {
        if cap < trim {
            break;
        }
        let slice = &data[..cap - trim];
        if let Ok(text) = str::from_utf8(slice) {
            return Some(text);
        }
    }
    None
}

pub struct FuzzRunner<State> {
    name: &'static str,
    max_input_size: usize,
    timeout: Duration,
    input_tx: mpsc::SyncSender<Vec<u8>>,
    output_rx: Mutex<mpsc::Receiver<()>>,
    _state: PhantomData<fn() -> State>,
}

impl<State: 'static> FuzzRunner<State> {
    pub fn new_default(
        name: &'static str,
        init: fn() -> State,
        run_one: fn(&mut State, &[u8]),
    ) -> Self {
        Self::new(name, MAX_INPUT_SIZE, TIMEOUT, init, run_one)
    }

    pub fn new(
        name: &'static str,
        max_input_size: usize,
        timeout: Duration,
        init: fn() -> State,
        run_one: fn(&mut State, &[u8]),
    ) -> Self {
        // Buffer a single input/output so the main fuzz thread can't deadlock with the worker
        // thread while trying to enforce timeouts.
        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (output_tx, output_rx) = mpsc::sync_channel::<()>(1);

        std::thread::spawn(move || {
            let mut state = init();
            for input in input_rx {
                run_one(&mut state, &input);
                let _ = output_tx.send(());
            }
        });

        Self {
            name,
            max_input_size,
            timeout,
            input_tx,
            output_rx: Mutex::new(output_rx),
            _state: PhantomData,
        }
    }

    pub fn run(&self, data: &[u8]) {
        let cap = data.len().min(self.max_input_size);
        let deadline = Instant::now() + self.timeout;

        let mut payload = data[..cap].to_vec();
        loop {
            match self.input_tx.try_send(payload) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(value)) => {
                    payload = value;
                    if Instant::now() >= deadline {
                        panic!("{} fuzz target timed out", self.name);
                    }
                    std::thread::yield_now();
                }
                Err(mpsc::TrySendError::Disconnected(_value)) => {
                    panic!("{} worker thread exited", self.name);
                }
            }
        }

        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);

        // Avoid poisoning the receiver lock on a fuzz failure panic.
        let recv = {
            let rx = lock(&self.output_rx, "FuzzRunner.output_rx");
            rx.recv_timeout(remaining)
        };

        match recv {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("{} fuzz target timed out", self.name),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("{} worker thread panicked", self.name)
            }
        }
    }
}
