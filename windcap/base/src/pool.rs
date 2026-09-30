//! A bounded pool for the work inside one maintenance step.
//!
//! The pass has always been one thread doing nine jobs in a row, and the machine it runs on has
//! twenty-odd of them. This is the smallest thing that changes that without changing what a step
//! *means*: the items a step would handle are enumerated first, handed to at most `lanes` workers, and
//! every result is put back in the slot its item came from. Nothing about a step's outcome counters, its
//! report lines, its `--limit` or its stop request is different because the work was done in parallel —
//! the only difference is that the wall clock moves while the encoder and the disk are busy, instead of
//! waiting for each of them in turn.
//!
//! # Why scoped threads and not a task crate
//!
//! [`std::thread::scope`] borrows the items and the result slots, so the borrow checker proves what a
//! pool of `'static` closures would have to promise by hand. It needs no dependency, and this
//! workspace's offline build is the gate every binary is proved by: adding a runtime here would make
//! `cargo build --offline` depend on somebody's crate cache.
//!
//! # Why the stop question is a parameter
//!
//! [`crate::maintain::may_continue`] is process-wide and latches once a stop has been seen, which is
//! exactly right for a pool: every worker asks the same question and the first to see the answer stops
//! claiming work. It stays a parameter anyway, so a caller whose loop must stop for some *other* reason
//! — a budget spent, a limit reached — can pass that instead, and so the rule is testable without a disk.
//!
//! # Ordering
//!
//! Results come back in item order, which is what keeps a report's lines stable between two runs of the
//! same install. A pass stopped early leaves a suffix of slots `None`: the caller sees how much of the
//! queue it did not reach instead of having to infer it from a counter. Indices are handed out from a
//! counter rather than from a queue of moved items, so that prefix property is the implementation, not a
//! hope: what was claimed is exactly what the counter reached.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// How many workers a step may use, given what its units cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duty {
    /// A unit that spawns a whole process — ffmpeg encoding a slice, the reindexer's own walk. Each lane
    /// is a separate OS process with its own encoder session and its own OCR child, so the count is
    /// deliberately small: a 24-thread machine gets four of these, not twenty-four.
    Subprocess,
    /// A unit that is image decode and resize inside this process: one thread of arithmetic per item,
    /// and no subprocess to bring its own threads along.
    Decode,
    /// A unit that is mostly disk — copy a month file, list a folder, probe existence. Past a few lanes
    /// the drive is the bottleneck and not the process.
    Disk,
}

/// The pool size for one duty on this machine.
///
/// `available_parallelism` is the logical CPU count, and the fractions below are the whole policy: a
/// subprocess lane is worth a quarter of the machine because it arrives with its own threads, a decode
/// lane is worth half, a disk lane a quarter. Always at least one lane, and never more than eight — a
/// pass that competes with a live recorder for the whole box is the failure this product already paid
/// for once (`psutil` at ten thousand calls a second, 2026-09-22).
pub fn lanes(duty: Duty) -> usize {
    let cores = std::thread::available_parallelism().map(|count| count.get()).unwrap_or(1);
    match duty {
        Duty::Subprocess => (cores / 4).clamp(1, 4),
        Duty::Decode => (cores / 2).clamp(1, 8),
        Duty::Disk => (cores / 4).clamp(1, 4),
    }
}

/// Run `work` over every item on at most `lanes` workers, returning results in item order.
///
/// A slot is `None` only when the item was never claimed, which happens when `may_work` says stop: the
/// pass was asked to end, or the caller's own budget ran out.
///
/// One lane, or one item, runs on the calling thread. That is not a micro-optimisation: a step asked to
/// do a single thing must not be able to behave differently from the same step on a machine with one
/// core, and the tests that run inside a busy `cargo test` process share this pool with the rest of the
/// suite.
pub fn run<In, Out, F, S>(items: &[In], lanes: usize, may_work: S, work: F) -> Vec<Option<Out>>
where
    In: Sync,
    Out: Send,
    F: Fn(&In) -> Out + Send + Sync,
    S: Fn() -> bool + Send + Sync,
{
    let mut slots: Vec<Option<Out>> = items.iter().map(|_| None).collect();
    if items.is_empty() {
        return slots;
    }
    if lanes <= 1 || items.len() == 1 {
        for (index, item) in items.iter().enumerate() {
            if !may_work() {
                break;
            }
            slots[index] = Some(work(item));
        }
        return slots;
    }

    let cursor = AtomicUsize::new(0);
    let held = Mutex::new(&mut slots);
    std::thread::scope(|scope| {
        for _ in 0..lanes.min(items.len()) {
            scope.spawn(|| loop {
                // Asked before claiming, so a stop request costs at most the one item a worker is
                // already inside rather than the whole queue.
                if !may_work() {
                    return;
                }
                let index = cursor.fetch_add(1, Ordering::SeqCst);
                if index >= items.len() {
                    return;
                }
                let out = work(&items[index]);
                let mut guard = held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                guard[index] = Some(out);
            });
        }
    });
    slots
}

/// How many items were never claimed.
pub fn unfinished<Out>(slots: &[Option<Out>]) -> usize {
    slots.iter().filter(|slot| slot.is_none()).count()
}

/// The items that were answered, in the order they came.
pub fn answered<Out>(slots: Vec<Option<Out>>) -> Vec<Out> {
    slots.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn every_item_is_answered_in_the_order_it_came() {
        let items: Vec<usize> = (0..40).collect();
        let done: Vec<usize> = run(&items, 4, || true, |n| n * 2).into_iter().flatten().collect();
        assert_eq!(done, (0..40).map(|n| n * 2).collect::<Vec<_>>(), "parallel work, sequential report");
    }

    #[test]
    fn a_stop_request_leaves_the_rest_of_the_queue_untouched() {
        let items: Vec<usize> = (0..400).collect();
        let claims = AtomicUsize::new(0);
        let slots = run(
            &items,
            4,
            move || claims.fetch_add(1, Ordering::SeqCst) < 20,
            |n| {
                std::thread::yield_now();
                *n
            },
        );
        let done = slots.iter().filter(|slot| slot.is_some()).count();
        assert!(done < items.len(), "the queue was cut short: {done} of {}", items.len());
        assert_eq!(unfinished(&slots), items.len() - done);
        // Nothing is answered past a gap: the counter hands out indices, so claimed work is a prefix.
        let first_gap = slots.iter().position(|slot| slot.is_none()).expect("a gap exists");
        assert!(slots[..first_gap].iter().all(|slot| slot.is_some()), "claims fill from the front");
    }

    #[test]
    fn one_lane_and_one_item_answer_everything_in_order() {
        // The two shapes that must not go through a worker at all. Their results are the same as the
        // pooled path's, which is what lets a single-slice install behave like every other install.
        let single = vec![7usize];
        assert_eq!(answered(run(&single, 8, || true, |n| *n)), vec![7]);
        let many = vec![1usize, 2, 3];
        assert_eq!(answered(run(&many, 1, || true, |n| *n)), vec![1, 2, 3]);
        // A stop on the serial path is a break, not a panic: the remaining items keep their empty slot.
        let stopped = run(&many, 1, || false, |n| *n);
        assert_eq!(answered(stopped), Vec::<usize>::new(), "nothing was allowed to start");
    }

    #[test]
    fn the_lane_policy_never_hands_out_zero() {
        for duty in [Duty::Subprocess, Duty::Decode, Duty::Disk] {
            assert!(lanes(duty) >= 1, "{duty:?}");
            assert!(lanes(duty) <= 8, "{duty:?}");
        }
        assert!(lanes(Duty::Subprocess) <= lanes(Duty::Decode), "a subprocess is the expensive lane");
    }

    #[test]
    fn an_empty_queue_calls_nothing() {
        let items: Vec<usize> = Vec::new();
        let slots = run(&items, 4, || panic!("no work, no ask"), |n| *n);
        assert!(slots.is_empty());
    }

    #[test]
    fn work_actually_overlaps() {
        // The reason this file exists: four items that each sleep 200 ms must not take 800 ms. A floor of
        // 600 ms rather than the theoretical 200 keeps a loaded CI machine from failing a correct pool,
        // and still fails the serial one.
        let items = vec![1usize; 4];
        let started = std::time::Instant::now();
        let done = run(&items, 4, || true, |_| std::thread::sleep(std::time::Duration::from_millis(200)));
        let elapsed = started.elapsed();
        assert_eq!(done.len(), 4);
        assert!(elapsed < std::time::Duration::from_millis(600), "four lanes took {elapsed:?}");
    }
}
