//! The scratch-directory counter both test targets need, and nothing else.
//!
//! This used to live in `fixtures.rs`, which is declared by *both* `lib.rs` and `main.rs` because the
//! egui window's tests and the shared query layer's tests each build their own temp roots. That gave
//! the library target a copy of every widget fixture it never calls - `Library`, `Canned`, `at`,
//! `date`, `clock`, `records` - and rustc reported all ten as never used while compiling `windui`'s
//! tests. The counter is the one thing the library's own tests genuinely want, so it has its own
//! module now and each target declares only what it uses.

/// A scratch directory number unique to *this test*, not merely to this process.
///
/// cargo runs the suite in parallel threads inside one process, so a temp path built from the pid
/// alone gives two same-tag fixtures the same directory — and the `remove_dir_all` that clears a
/// fixture wipes a sibling's tree mid-run. That is the intermittent failure that passed on re-run.
static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn next_scratch_id() -> u64 {
    SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The number is the whole contract: two calls in one process must differ, or two tests share a
/// directory and one's cleanup deletes the other's library mid-run.
#[cfg(test)]
mod tests {
    use super::next_scratch_id;

    #[test]
    fn two_calls_in_one_process_never_hand_back_the_same_number() {
        assert_ne!(next_scratch_id(), next_scratch_id());
    }
}
