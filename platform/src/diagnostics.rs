//! Best-effort console diagnostics must not become kernel backpressure.
//!
//! Producers never wait for a queue lock or perform sink I/O. One detached
//! writer owns stderr output; its lifetime is deliberately not joined during
//! shutdown because a pipe or filesystem write can remain blocked indefinitely.
//! These bounded, lossy messages are not a durable audit or cleanup receipt.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};

pub const ORDINARY_CAPACITY: usize = 128;
pub const TERMINAL_CAPACITY: usize = 16;
pub const MAX_LINE_BYTES: usize = 8192;
const TRUNCATED: &str = " [truncated]\n";

/// Process-local counters and queue gauges; no peer text, paths, handles or identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Snapshot {
    pub accepted: u64,
    pub written: u64,
    pub dropped_ordinary: u64,
    pub dropped_terminal: u64,
    pub write_failures: u64,
    pub truncated: u64,
    pub queued_ordinary: u64,
    pub queued_terminal: u64,
}

#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    written: AtomicU64,
    dropped_ordinary: AtomicU64,
    dropped_terminal: AtomicU64,
    write_failures: AtomicU64,
    truncated: AtomicU64,
    queued_ordinary: AtomicU64,
    queued_terminal: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped_ordinary: self.dropped_ordinary.load(Ordering::Relaxed),
            dropped_terminal: self.dropped_terminal.load(Ordering::Relaxed),
            write_failures: self.write_failures.load(Ordering::Relaxed),
            truncated: self.truncated.load(Ordering::Relaxed),
            queued_ordinary: self.queued_ordinary.load(Ordering::Relaxed),
            queued_terminal: self.queued_terminal.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
struct Shared {
    ordinary: Mutex<VecDeque<String>>,
    terminal: Mutex<VecDeque<String>>,
    worker: OnceLock<std::thread::Thread>,
    counters: Counters,
    stopped: AtomicBool,
}

struct Diagnostics(Arc<Shared>);

impl Diagnostics {
    fn start(writer: impl Write + Send + 'static) -> Self {
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        if std::thread::Builder::new()
            .name("nucleus-diagnostics".into())
            .spawn(move || {
                let _ = worker.worker.set(std::thread::current());
                consume(&worker, writer);
            })
            .is_err()
        {
            // Diagnostics cannot make startup fail or leave an undrainable queue.
            shared.stopped.store(true, Ordering::Release);
        }
        Self(shared)
    }

    fn emit(&self, args: fmt::Arguments<'_>, terminal: bool) -> bool {
        let dropped =
            if terminal { &self.0.counters.dropped_terminal } else { &self.0.counters.dropped_ordinary };
        if self.0.stopped.load(Ordering::Acquire) {
            dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // Formatting happens outside the queue lock and stops at the byte limit.
        let (line, truncated) = limited_line(args);
        if truncated {
            self.0.counters.truncated.fetch_add(1, Ordering::Relaxed);
        }
        let (queue, capacity, queued) = if terminal {
            (&self.0.terminal, TERMINAL_CAPACITY, &self.0.counters.queued_terminal)
        } else {
            (&self.0.ordinary, ORDINARY_CAPACITY, &self.0.counters.queued_ordinary)
        };
        let mut queue = match queue.try_lock() {
            Ok(queues) => queues,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        if queue.len() == capacity {
            dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        queue.push_back(line);
        queued.fetch_add(1, Ordering::Relaxed);
        self.0.counters.accepted.fetch_add(1, Ordering::Relaxed);
        drop(queue);
        if let Some(worker) = self.0.worker.get() {
            worker.unpark();
        }
        true
    }
}

fn limited_line(args: fmt::Arguments<'_>) -> (String, bool) {
    struct Line(String, bool);
    impl fmt::Write for Line {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            let remaining = (MAX_LINE_BYTES - TRUNCATED.len()).saturating_sub(self.0.len());
            if value.len() <= remaining {
                self.0.push_str(value);
                return Ok(());
            }
            let mut end = remaining;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            self.0.push_str(&value[..end]);
            self.1 = true;
            Err(fmt::Error)
        }
    }
    let mut line = Line(String::with_capacity(MAX_LINE_BYTES), false);
    let _ = line.write_fmt(args);
    if line.1 {
        line.0.push_str(TRUNCATED);
    } else {
        line.0.push('\n');
    }
    (line.0, line.1)
}

fn consume(shared: &Shared, mut writer: impl Write) {
    let mut reported = (0, 0, 0, 0);
    loop {
        let Some(line) = take(&shared.terminal, &shared.counters.queued_terminal)
            .or_else(|| take(&shared.ordinary, &shared.counters.queued_ordinary))
        else {
            if shared.stopped.load(Ordering::Acquire) {
                return;
            }
            // unpark retains a token across the empty-check/park race. The
            // worker registers itself before its first queue check, so startup
            // notifications also cannot strand an already queued diagnostic.
            std::thread::park();
            continue;
        };
        // No queue or kernel state lock is held across either write. Ordinary
        // event overflow cannot consume the independent terminal queue slots.
        let snapshot = shared.counters.snapshot();
        let current = (
            snapshot.dropped_ordinary,
            snapshot.dropped_terminal,
            snapshot.write_failures,
            snapshot.truncated,
        );
        if current != reported {
            let summary = format!(
                "nucleus: diagnostic_output: {{\"v\":1,\"dropped_ordinary\":{},\"dropped_terminal\":{},\"write_failures\":{},\"truncated\":{}}}\n",
                current.0, current.1, current.2, current.3,
            );
            if writer.write_all(summary.as_bytes()).is_ok() {
                reported = current;
            } else {
                shared.counters.write_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        if writer.write_all(line.as_bytes()).is_ok() {
            shared.counters.written.fetch_add(1, Ordering::Relaxed);
        } else {
            shared.counters.write_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn take(queue: &Mutex<VecDeque<String>>, queued: &AtomicU64) -> Option<String> {
    let mut queue = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let line = queue.pop_front();
    if line.is_some() {
        queued.fetch_sub(1, Ordering::Relaxed);
    }
    line
}

static CONSOLE: OnceLock<Diagnostics> = OnceLock::new();

/// Initialize outside the kernel state lock before supervising processes.
pub fn initialize() {
    CONSOLE.get_or_init(|| Diagnostics::start(std::io::stderr()));
}

/// Enqueue existing console-event text without waiting for its output sink.
pub fn emit(args: fmt::Arguments<'_>) -> bool {
    initialize();
    CONSOLE.get().is_some_and(|console| console.emit(args, false))
}

/// Reserved capacity for first runtime-loss and cleanup-failure diagnostics.
/// Callers must retain their existing finite, credential-free diagnostic schema.
pub fn emit_terminal(args: fmt::Arguments<'_>) -> bool {
    initialize();
    CONSOLE.get().is_some_and(|console| console.emit(args, true))
}

/// Observable even while the writer is blocked; does not initialize or wait.
pub fn snapshot() -> Snapshot {
    CONSOLE.get().map_or_else(Snapshot::default, |console| console.0.counters.snapshot())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::{Condvar, mpsc};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct SinkState {
        released: Mutex<bool>,
        wake: Condvar,
        bytes: Mutex<Vec<u8>>,
        fail: AtomicBool,
    }

    struct ControlledSink {
        state: Arc<SinkState>,
        entered: Option<mpsc::Sender<()>>,
        finished: mpsc::Sender<()>,
    }

    impl Write for ControlledSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            let mut released = self.state.released.lock().unwrap();
            while !*released {
                released = self.state.wake.wait(released).unwrap();
            }
            drop(released);
            if self.state.fail.load(Ordering::Acquire) {
                return Err(std::io::Error::other("synthetic sink failure"));
            }
            self.state.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for ControlledSink {
        fn drop(&mut self) {
            let _ = self.finished.send(());
        }
    }

    struct Fixture {
        logger: Diagnostics,
        sink: Arc<SinkState>,
        finished: mpsc::Receiver<()>,
    }

    impl Fixture {
        fn blocked() -> Self {
            let sink = Arc::new(SinkState::default());
            let (entered_tx, entered_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            let logger = Diagnostics::start(ControlledSink {
                state: Arc::clone(&sink),
                entered: Some(entered_tx),
                finished: finished_tx,
            });
            let fixture = Self { logger, sink, finished: finished_rx };
            // Startup's empty-queue check may briefly own this mutex. Admission
            // is intentionally lossy under contention, so retry only this setup.
            wait_for(|| fixture.logger.emit(format_args!("first"), false));
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            fixture
        }

        fn release(&self) {
            *self.sink.released.lock().unwrap() = true;
            self.sink.wake.notify_all();
        }

        fn stop(&self) {
            self.logger.0.stopped.store(true, Ordering::Release);
            self.release();
            if let Some(worker) = self.logger.0.worker.get() {
                worker.unpark();
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.sink.bytes.lock().unwrap().clone()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Release test-owned I/O even if an assertion panics; never leave a
            // blocked test thread alive after the test harness exits this scope.
            self.stop();
            let _ = self.finished.recv_timeout(Duration::from_secs(5));
        }
    }

    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "diagnostic worker did not make progress");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn blocked_sink_has_bounded_queues_and_reserved_terminal_capacity() {
        let fixture = Fixture::blocked();
        let before = fixture.logger.0.counters.snapshot();
        let started = Instant::now();
        for n in 0..ORDINARY_CAPACITY {
            assert!(fixture.logger.emit(format_args!("ordinary {n}"), false));
        }
        assert!(!fixture.logger.emit(format_args!("ordinary overflow"), false));
        for n in 0..TERMINAL_CAPACITY {
            assert!(fixture.logger.emit(format_args!("terminal {n}"), true));
        }
        assert!(!fixture.logger.emit(format_args!("terminal overflow"), true));
        assert!(started.elapsed() < Duration::from_secs(1));
        let snapshot = fixture.logger.0.counters.snapshot();
        assert_eq!(snapshot.queued_ordinary, ORDINARY_CAPACITY as u64);
        assert_eq!(snapshot.queued_terminal, TERMINAL_CAPACITY as u64);
        assert_eq!(snapshot.dropped_ordinary, before.dropped_ordinary + 1);
        assert_eq!(snapshot.dropped_terminal, before.dropped_terminal + 1);
        assert_eq!(snapshot.written, 0);

        fixture.release();
        wait_for(|| fixture.logger.0.counters.snapshot().written == snapshot.accepted);
        let text = fixture.text();
        assert!(text.find("terminal 0\n").unwrap() < text.find("ordinary 0\n").unwrap());
        assert!(!text.contains("overflow"));
        let summary = text
            .lines()
            .filter_map(|line| line.strip_prefix("nucleus: diagnostic_output: "))
            .next_back()
            .unwrap();
        let summary: serde_json::Value = serde_json::from_str(summary).unwrap();
        assert_eq!(summary["dropped_ordinary"], snapshot.dropped_ordinary);
        assert_eq!(summary["dropped_terminal"], 1);
        assert_eq!(summary.as_object().unwrap().len(), 5);
    }

    #[test]
    fn contended_ordinary_queue_cannot_block_producers_or_terminal_admission() {
        let fixture = Fixture::blocked();
        let _held = fixture.logger.0.ordinary.lock().unwrap();
        let before = fixture.logger.0.counters.snapshot();
        let started = Instant::now();
        assert!(!fixture.logger.emit(format_args!("contended"), false));
        assert!(fixture.logger.emit(format_args!("terminal"), true));
        assert!(started.elapsed() < Duration::from_secs(1));
        let after = fixture.logger.0.counters.snapshot();
        assert_eq!(after.dropped_ordinary, before.dropped_ordinary + 1);
        assert_eq!(after.queued_terminal, 1);
    }

    #[test]
    fn sink_errors_are_counted_and_do_not_stop_later_output() {
        let fixture = Fixture::blocked();
        fixture.sink.fail.store(true, Ordering::Release);
        fixture.release();
        wait_for(|| fixture.logger.0.counters.snapshot().write_failures > 0);
        fixture.sink.fail.store(false, Ordering::Release);
        wait_for(|| fixture.logger.emit(format_args!("after failure"), true));
        wait_for(|| fixture.text().contains("after failure\n"));
        let text = fixture.text();
        assert!(text.contains("after failure\n"));
        assert!(
            text.lines()
                .filter_map(|line| line.strip_prefix("nucleus: diagnostic_output: "))
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .any(|summary| summary["write_failures"].as_u64().unwrap() > 0)
        );
        assert!(!text.contains("synthetic sink failure"));
    }

    #[test]
    fn output_is_byte_bounded_and_truncates_at_utf8_boundaries() {
        let (small, truncated) = limited_line(format_args!("event: {}", 7));
        assert_eq!(small, "event: 7\n");
        assert!(!truncated);
        let large = "€".repeat(MAX_LINE_BYTES);
        let (line, truncated) = limited_line(format_args!("{large}"));
        assert!(truncated);
        assert!(line.len() <= MAX_LINE_BYTES);
        assert!(line.ends_with(TRUNCATED));
        assert!(line.is_char_boundary(line.len() - TRUNCATED.len()));
        let fixture = Fixture::blocked();
        assert!(fixture.logger.emit(format_args!("{large}"), false));
        assert_eq!(fixture.logger.0.counters.snapshot().truncated, 1);
    }

    #[test]
    fn stopped_logger_drops_without_waiting_for_its_blocked_writer() {
        let fixture = Fixture::blocked();
        fixture.logger.0.stopped.store(true, Ordering::Release);
        let before = fixture.logger.0.counters.snapshot();
        let started = Instant::now();
        assert!(!fixture.logger.emit(format_args!("late ordinary"), false));
        assert!(!fixture.logger.emit(format_args!("late terminal"), true));
        assert!(started.elapsed() < Duration::from_secs(1));
        let after = fixture.logger.0.counters.snapshot();
        assert_eq!(after.queued_ordinary, 0);
        assert_eq!(after.queued_terminal, 0);
        assert_eq!(after.dropped_ordinary, before.dropped_ordinary + 1);
        assert_eq!(after.dropped_terminal, before.dropped_terminal + 1);
    }
}
