//! Hand-off from capture connections to the blocking OCR worker.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

use capricorn_core::proto::{ClipboardEntry, Frame};
use tracing::warn;

/// Older frames of an output are dropped once this many wait, so the newest win.
pub const FRAMES_PER_OUTPUT: usize = 4;
/// Output names come from the client, so they are capped like everything else it sends.
pub const MAX_OUTPUTS: usize = 16;
pub const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;
const CLIPBOARD_BACKLOG: usize = 32;

#[derive(Debug)]
pub enum Work {
    Frame(Frame),
    Clipboard(ClipboardEntry),
}

struct Queued {
    arrival: u64,
    frame: Frame,
}

struct Pending {
    /// Only outputs with frames waiting have an entry.
    frames: BTreeMap<String, VecDeque<Queued>>,
    clipboard: VecDeque<ClipboardEntry>,
    last_output: Option<String>,
    closed: bool,
    bytes: usize,
    arrivals: u64,
    max_outputs: usize,
    max_bytes: usize,
    warned: bool,
}

impl Pending {
    fn new(max_outputs: usize, max_bytes: usize) -> Self {
        Self {
            frames: BTreeMap::new(),
            clipboard: VecDeque::new(),
            last_output: None,
            closed: false,
            bytes: 0,
            arrivals: 0,
            max_outputs,
            max_bytes,
            warned: false,
        }
    }

    fn pop(&mut self) -> Option<Work> {
        if let Some(entry) = self.clipboard.pop_front() {
            return Some(Work::Clipboard(entry));
        }
        let output = self.next_output()?;
        let frame = self.take_front(&output)?;
        self.last_output = Some(output);
        Some(Work::Frame(frame))
    }

    /// Takes outputs in turn so a busy monitor cannot starve the others.
    fn next_output(&self) -> Option<String> {
        let after_last = self
            .frames
            .keys()
            .find(|output| Some(*output) > self.last_output.as_ref());
        after_last.or_else(|| self.frames.keys().next()).cloned()
    }

    /// Returns true when older frames had to be dropped to stay within the limits.
    fn push_frame(&mut self, frame: Frame) -> bool {
        self.arrivals += 1;
        self.bytes += frame.rgba.len();
        let output = frame.output.clone();
        let queue = self.frames.entry(output.clone()).or_default();
        queue.push_back(Queued {
            arrival: self.arrivals,
            frame,
        });
        let mut dropped = false;
        if queue.len() > FRAMES_PER_OUTPUT {
            self.take_front(&output);
        }
        while self.bytes > self.max_bytes || self.frames.len() > self.max_outputs {
            if !self.drop_oldest() {
                break;
            }
            dropped = true;
        }
        dropped
    }

    fn drop_oldest(&mut self) -> bool {
        let oldest = self
            .frames
            .iter()
            .filter_map(|(output, queue)| queue.front().map(|queued| (queued.arrival, output)))
            .min()
            .map(|(_, output)| output.clone());
        oldest.is_some_and(|output| self.take_front(&output).is_some())
    }

    fn take_front(&mut self, output: &str) -> Option<Frame> {
        let queue = self.frames.get_mut(output)?;
        let queued = queue.pop_front()?;
        if queue.is_empty() {
            self.frames.remove(output);
        }
        self.bytes -= queued.frame.rgba.len();
        Some(queued.frame)
    }
}

pub struct CaptureQueue {
    pending: Mutex<Pending>,
    ready: Condvar,
}

impl Default for CaptureQueue {
    fn default() -> Self {
        Self::with_limits(MAX_OUTPUTS, MAX_QUEUED_BYTES)
    }
}

impl CaptureQueue {
    pub fn with_limits(max_outputs: usize, max_bytes: usize) -> Self {
        Self {
            pending: Mutex::new(Pending::new(max_outputs, max_bytes)),
            ready: Condvar::new(),
        }
    }

    pub fn push_frame(&self, frame: Frame) {
        self.with_pending(|pending| {
            if pending.push_frame(frame) && !pending.warned {
                pending.warned = true;
                warn!("capture queue full; dropping the oldest frames");
            }
        });
    }

    pub fn push_clipboard(&self, entry: ClipboardEntry) {
        self.with_pending(|pending| {
            if pending.clipboard.len() >= CLIPBOARD_BACKLOG {
                pending.clipboard.pop_front();
            }
            pending.clipboard.push_back(entry);
        });
    }

    /// Wakes the worker; `next` returns what is still queued, then `None`.
    pub fn close(&self) {
        self.with_pending(|pending| pending.closed = true);
    }

    /// Blocks until work arrives or the queue is closed and drained.
    pub fn next(&self) -> Option<Work> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(work) = pending.pop() {
                return Some(work);
            }
            if pending.closed {
                return None;
            }
            pending = self
                .ready
                .wait(pending)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn with_pending(&self, change: impl FnOnce(&mut Pending)) {
        change(&mut self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        self.ready.notify_one();
    }
}

/// Token bucket holding at most one second's worth of messages.
pub struct RateLimit {
    per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimit {
    pub fn new(per_sec: u32) -> Self {
        Self {
            per_sec: f64::from(per_sec),
            tokens: f64::from(per_sec),
            last: Instant::now(),
        }
    }

    pub fn allow(&mut self) -> bool {
        self.allow_at(Instant::now())
    }

    fn allow_at(&mut self, now: Instant) -> bool {
        let refill = now.saturating_duration_since(self.last).as_secs_f64() * self.per_sec;
        self.tokens = (self.tokens + refill).min(self.per_sec);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use capricorn_core::proto::WindowContext;

    use super::*;

    fn frame(output: &str, ts: i64) -> Frame {
        Frame {
            ts,
            output: output.into(),
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            window: WindowContext::default(),
        }
    }

    fn next_frame(queue: &CaptureQueue) -> (String, i64) {
        match queue.next() {
            Some(Work::Frame(frame)) => (frame.output, frame.ts),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn newest_frames_win_per_output() {
        let queue = CaptureQueue::default();
        for ts in 0..6 {
            queue.push_frame(frame("DP-1", ts));
        }
        let kept: Vec<i64> = (0..FRAMES_PER_OUTPUT)
            .map(|_| next_frame(&queue).1)
            .collect();
        assert_eq!(kept, vec![2, 3, 4, 5]);
    }

    #[test]
    fn outputs_take_turns_and_clipboard_goes_first() {
        let queue = CaptureQueue::default();
        queue.push_frame(frame("A", 1));
        queue.push_frame(frame("A", 2));
        queue.push_frame(frame("B", 3));
        queue.push_clipboard(ClipboardEntry {
            ts: 9,
            text: "x".into(),
            source: WindowContext::default(),
        });
        assert!(matches!(queue.next(), Some(Work::Clipboard(_))));
        let order: Vec<String> = (0..3).map(|_| next_frame(&queue).0).collect();
        assert_eq!(order, vec!["A", "B", "A"]);
    }

    fn sized_frame(output: &str, ts: i64, bytes: usize) -> Frame {
        Frame {
            rgba: vec![0; bytes],
            ..frame(output, ts)
        }
    }

    #[test]
    fn output_count_is_capped_dropping_the_oldest() {
        let queue = CaptureQueue::with_limits(2, usize::MAX);
        queue.push_frame(frame("A", 1));
        queue.push_frame(frame("B", 2));
        queue.push_frame(frame("C", 3));
        let kept: Vec<(String, i64)> = (0..2).map(|_| next_frame(&queue)).collect();
        assert_eq!(kept, vec![("B".into(), 2), ("C".into(), 3)]);
    }

    #[test]
    fn queued_bytes_are_capped_dropping_the_oldest() {
        let queue = CaptureQueue::with_limits(usize::MAX, 100);
        queue.push_frame(sized_frame("A", 1, 60));
        queue.push_frame(sized_frame("B", 2, 60));
        queue.push_frame(sized_frame("A", 3, 30));
        let kept: Vec<i64> = (0..2).map(|_| next_frame(&queue).1).collect();
        assert_eq!(kept, vec![3, 2]);
        queue.close();
        assert!(queue.next().is_none());
    }

    #[test]
    fn close_drains_queued_work_first() {
        let queue = CaptureQueue::default();
        queue.push_frame(frame("A", 1));
        queue.close();
        assert_eq!(next_frame(&queue), ("A".into(), 1));
        assert!(queue.next().is_none());
    }

    #[test]
    fn close_releases_a_waiting_worker() {
        let queue = std::sync::Arc::new(CaptureQueue::default());
        let waiter = {
            let queue = queue.clone();
            std::thread::spawn(move || queue.next().is_none())
        };
        std::thread::sleep(Duration::from_millis(50));
        queue.close();
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn rate_limit_drops_excess_and_refills() {
        let mut limit = RateLimit::new(2);
        let start = limit.last;
        assert!(limit.allow_at(start));
        assert!(limit.allow_at(start));
        assert!(!limit.allow_at(start));
        assert!(limit.allow_at(start + Duration::from_millis(500)));
        assert!(!limit.allow_at(start + Duration::from_millis(600)));
    }
}
