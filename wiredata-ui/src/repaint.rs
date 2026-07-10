//! Cross-thread repaint coalescing, shared by both apps (talker ADR-016 /
//! listener ADR-019 — extracted once both had grown the same pattern).
//!
//! Both GUIs are woken by background threads: talker's runners notify on each
//! queued status, listener's driver on each pushed update. egui already
//! coalesces repaint *requests* into one frame, but every
//! `Context::request_repaint()` call still pays a winit event-loop-proxy
//! wake — at kilohertz status rates that's pointless syscall churn. This
//! coalescer collapses all notifications between frames into a single wake:
//! `notify()` is one atomic swap unless a repaint isn't already pending.
//!
//! Usage: the UI thread calls [`RepaintCoalescer::frame_started`] at the top
//! of each frame — **before** draining its inbox — to re-arm; background
//! threads call [`RepaintCoalescer::notify`]. Re-arming before the drain
//! makes the wake un-losable: a notification during the drain either lands
//! in the drained batch or triggers a fresh wake for the next frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct RepaintCoalescer {
    wake: Box<dyn Fn() + Send + Sync>,
    pending: AtomicBool,
}

impl RepaintCoalescer {
    /// A coalescer over an arbitrary wake callback (unit-testable).
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            wake: Box::new(wake),
            pending: AtomicBool::new(false),
        })
    }

    /// The common case: wake `ctx` (an `egui::Context` clone is a cheap `Arc`
    /// and `request_repaint` is thread-safe).
    pub fn for_ctx(ctx: egui::Context) -> Arc<Self> {
        Self::new(move || ctx.request_repaint())
    }

    /// Request a repaint. Thread-safe; a no-op when one is already pending.
    pub fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    /// Re-arm at the top of a frame, before draining any inbox.
    pub fn frame_started(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn counting() -> (Arc<RepaintCoalescer>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&count);
        (
            RepaintCoalescer::new(move || {
                c2.fetch_add(1, Ordering::SeqCst);
            }),
            count,
        )
    }

    #[test]
    fn notifications_between_frames_coalesce_to_one_wake() {
        let (c, wakes) = counting();
        for _ in 0..100 {
            c.notify();
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn frame_start_rearms() {
        let (c, wakes) = counting();
        c.notify();
        c.notify();
        c.frame_started();
        c.notify();
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn quiet_frames_wake_nothing() {
        let (c, wakes) = counting();
        c.frame_started();
        c.frame_started();
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }
}
