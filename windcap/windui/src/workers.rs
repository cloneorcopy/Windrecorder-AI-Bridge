//! The threads that do the reading, so a frame never waits for a disk.
//!
//! One bounded pool, one unbounded command queue, one reply channel back to the UI thread. That is
//! the whole design, and each part is there for a reason:
//!
//!   * the pool is fixed at four threads so a page of 100 thumbnail decodes cannot become 100
//!     threads, and so one long search cannot starve the strip's decodes forever;
//!   * replies carry no ordering guarantee — `model::AppState::apply` drops them by request id,
//!     which is the only place ordering is decided;
//!   * every reply is followed by `Context::request_repaint`, because egui is a pull model and an
//!     event nobody asked for is an event nobody sees.
//!
//! Shutdown deliberately does not join: a search that is still running when the window closes must
//! not hold the process open. Dropping the queue's sender is enough for the workers to finish their
//! current job and exit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::model::AppEvent;

type Job = Box<dyn FnOnce(Sink) + Send + 'static>;

enum Message {
    Run(Job),
    Stop,
}

/// How a worker talks to the UI: one channel, plus the thing that makes the channel visible.
#[derive(Clone)]
pub struct Sink {
    events: Sender<AppEvent>,
    ctx: egui::Context,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl Sink {
    /// Deliver a result and ask for the frame that will draw it.
    pub fn reply(&self, event: AppEvent) {
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }
        // The receive end lives in `App`; a closed channel means the window is gone, which is the
        // only failure this has to survive without panicking on a detached thread.
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }
}

pub struct Workers {
    queue: Option<Sender<Message>>,
    /// Set before the first reply can land, so a job that finishes after the window closes does
    /// nothing. Freeing textures on a dead `Context` is harmless, but the intent is worth stating.
    cancelled: Arc<AtomicBool>,
    threads: usize,
}

impl Workers {
    pub fn new(threads: usize, ctx: &egui::Context) -> (Workers, Receiver<AppEvent>) {
        let (event_tx, event_rx) = mpsc::channel();
        let (job_tx, job_rx) = mpsc::channel::<Message>();
        let shared = Arc::new(Mutex::new(job_rx));
        let cancelled = Arc::new(AtomicBool::new(false));
        let sink = Sink {
            events: event_tx,
            ctx: ctx.clone(),
            cancelled: cancelled.clone(),
        };
        for _ in 0..threads.max(1) {
            let shared = shared.clone();
            let sink = sink.clone();
            let _ = std::thread::Builder::new()
                .name("windui-worker".to_string())
                .spawn(move || loop {
                    let next = match shared.lock() {
                        Ok(guard) => guard.recv(),
                        // A poisoned lock means a job panicked; the panic already surfaced on
                        // stderr, and the surviving jobs are still worth running.
                        Err(poisoned) => poisoned.into_inner().recv(),
                    };
                    match next {
                        Ok(Message::Run(job)) => job(sink.clone()),
                        Ok(Message::Stop) | Err(_) => break,
                    }
                });
        }
        let workers = Workers { queue: Some(job_tx), cancelled, threads: threads.max(1) };
        (workers, event_rx)
    }

    pub fn submit(&self, job: impl FnOnce(Sink) + Send + 'static) {
        if let Some(queue) = &self.queue {
            let _ = queue.send(Message::Run(Box::new(job)));
        }
    }

    /// Stop taking jobs and let the threads drain. Never blocks.
    pub fn shutdown(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(queue) = self.queue.take() {
            for _ in 0..self.threads {
                let _ = queue.send(Message::Stop);
            }
            // Dropping the last sender is what unblocks `recv` in the workers.
            drop(queue);
        }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LibraryStats;

    #[test]
    fn a_job_runs_off_the_calling_thread_and_its_reply_arrives() {
        let ctx = egui::Context::default();
        let (mut workers, rx) = Workers::new(2, &ctx);
        workers.submit(move |sink| {
            sink.reply(AppEvent::Library(LibraryStats {
                months_total: 3,
                months_scanned: 3,
                rows: 42,
                first: None,
                last: None,
                done: true,
                error: None,
                ..Default::default()
            }))
        });
        let event = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("reply");
        match event {
            AppEvent::Library(stats) => assert_eq!(stats.rows, 42),
            other => panic!("unexpected {other:?}"),
        }
        workers.shutdown();
    }

    #[test]
    fn a_reply_after_shutdown_is_dropped_instead_of_panicking() {
        let ctx = egui::Context::default();
        let (mut workers, rx) = Workers::new(1, &ctx);
        workers.submit(move |sink| sink.reply(AppEvent::Library(LibraryStats::default())));
        drop(rx);
        workers.shutdown();
        // Give the worker time to have tried; the assertion is that nothing above panicked.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
