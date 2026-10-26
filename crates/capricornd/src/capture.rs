//! Hand-off from capture connections to the blocking OCR worker.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

use capricorn_core::proto::{ClipboardEntry, Frame};

/// Older frames of an output are dropped once this many wait, so the newest win.
pub const FRAMES_PER_OUTPUT: usize = 4;
const CLIPBOARD_BACKLOG: usize = 32;

#[derive(Debug)]
pub enum Work {
    Frame(Frame),
    Clipboard(ClipboardEntry),
}

#[derive(Default)]
struct Pending {
    frames: BTreeMap<String, VecDeque<Frame>>,
    clipboard: VecDeque<ClipboardEntry>,
    last_output: Option<String>,
    closed: bool,
}

impl Pending {
    fn pop(&mut self) -> Option<Work> {
        if let Some(entry) = self.clipboard.pop_front() {
            return Some(Work::Clipboard(entry));
        }
        let output = self.next_output()?;
        let frame = self.frames.get_mut(&output)?.pop_front()?;
        self.last_output = Some(output);
        Some(Work::Frame(frame))
    }

    /// Takes outputs in turn so a busy monitor cannot starve the others.
    fn next_output(&self) -> Option<String> {
        let waiting = |(_, queue): &(&String, &VecDeque<Frame>)| !queue.is_empty();
        let after_last = self
            .frames
            .iter()
            .filter(|(output, _)| Some(*output) > self.last_output.as_ref())
            .find(waiting);
        after_last
            .or_else(|| self.frames.iter().find(waiting))
            .map(|(output, _)| output.clone())
    }
}

#[derive(Default)]
pub struct CaptureQueue {
    pending: Mutex<Pending>,
    ready: Condvar,
}

impl CaptureQueue {
    pub fn push_frame(&self, frame: Frame) {
        self.with_pending(|pending| {
            let queue = pending.frames.entry(frame.output.clone()).or_default();
            if queue.len() >= FRAMES_PER_OUTPUT {
                queue.pop_front();
            }
            queue.push_back(frame);
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

    /// Wakes the worker; `next` returns `None` from then on.
    pub fn close(&self) {
        self.with_pending(|pending| pending.closed = true);
    }

    /// Blocks until work arrives or the queue is closed.
    pub fn next(&self) -> Option<Work> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if pending.closed {
                return None;
            }
            if let Some(work) = pending.pop() {
                return Some(work);
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
