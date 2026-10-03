//! The AI summary caches, seen from the side that deletes.
//!
//! `wind-summary` owns the format; this module owns one question about it: when the text a summary was
//! written from stops existing, what happens to the summary? The answer is the only one that is honest —
//! the paragraph goes, and the day built from it either goes with it or says out loud that it is no
//! longer whole.
//!
//! # Why this is not left to regeneration
//!
//! Every other AI artefact in the install can be rebuilt: `windai tags --month` will produce a month's
//! tags again from whatever titles remain. These cannot be. A paragraph written by an outside AI over
//! MCP is a record of text that a `windmaint forget` pass has just blanked, and nothing in this product
//! can produce it again — which is exactly why it survives an erase that does not name it. So the erase
//! has to reach it, and the reach is this module.
//!
//! # Partial erasure is treated as full
//!
//! A keyword `forget` blanks the rows that matched, which may be one frame of a nine-minute stretch. The
//! stretch's summary is dropped entirely rather than kept-and-annotated: a paragraph that quotes a
//! password manager's window is not made safe by a note saying part of its source was removed, and no
//! reader can tell which part.
//!
//! # Two passes, two answers about the day
//!
//! [`Aftermath::Erase`] is `forget`, which a human aimed at particular content: once no stretch of that
//! day is left standing, the day's own paragraph is deleted with them, because a `stale` flag on prose
//! that quotes what was just blanked is a marker on the thing the user asked to have gone.
//! [`Aftermath::Flag`] is `expire`, which aged footage out on a schedule: there the paragraph is kept and
//! flagged, because it may still describe stretches that stand and nothing here was chosen for its
//! content. Both pass through [`prune_segments`], and both can be previewed with [`plan`] without
//! writing a byte.

use std::collections::BTreeMap;

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_summary as summary;

/// What one pass did to the summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pruned {
    /// Stretch paragraphs removed, across every day touched.
    pub entries: usize,
    /// Product days whose own summary was marked no longer whole.
    pub days_marked: usize,
    /// Product days whose own summary was deleted with them, because nothing in the day was left
    /// standing. Only `forget` produces this; see [`Aftermath`].
    pub days_removed: usize,
    /// Days that held a summary file at all.
    pub days_touched: usize,
}

impl Pruned {
    /// The counts and the reason, in one line, in the tense the caller's pass deserves. `forget` and
    /// `expire` reach derived text through the same functions above; this keeps them saying what they
    /// did about it in the same words too.
    pub fn report(&self, planned: bool) -> String {
        let (drop, mark, remove) = if planned {
            ("would be dropped", "would be marked stale", "would be deleted")
        } else {
            ("dropped", "marked stale", "deleted")
        };
        format!(
            "{} stretch paragraph(s) {}, {} day(s) {}, {} day summary(ies) {} with the days that went \
             empty — unlike the month tags, none of this can be regenerated: an outside AI wrote some of \
             it, and no binary in this product can produce it again.",
            self.entries, drop, self.days_marked, mark, self.days_removed, remove
        )
    }
}

/// What happens to a day's own paragraph once the stretches it was written from are gone. The two
/// answers differ because the two passes differ in *why* they delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aftermath {
    /// `windmaint forget`: the user named this period because of what it held. A day left with no
    /// paragraph standing has nothing for its own summary to describe, and a `stale` flag on prose that
    /// quotes what was just blanked would be an erase in name only — so the day's summary goes too.
    Erase,
    /// `windmaint expire`: the footage aged out on a schedule nobody aimed at particular content. The
    /// day's paragraph may still describe stretches that stand, so it is flagged and kept for a reader
    /// to judge, never rewritten and never quietly deleted.
    Flag,
}

/// Whether this day's own paragraph is deleted or only flagged. One function for the question, so
/// [`prune_segments`] and [`plan`] cannot answer it differently.
fn erases_the_day(aftermath: Aftermath, emptied: bool) -> bool {
    matches!(aftermath, Aftermath::Erase) && emptied
}

/// Drop the summaries of segments that no longer stand, and settle the days built from them.
///
/// Takes `videofile_name`s — what both `forget` and `expire` have on hand — and resolves each to the
/// product day that files its summary, which is the day of the stretch's *start*, not of any row in it.
/// A segment with a name this build cannot read as a stamp has no summary to drop, so it is skipped
/// rather than guessed at.
pub fn prune_segments(config: &Config, filenames: &[String], aftermath: Aftermath) -> Pruned {
    let mut out = Pruned::default();
    for (day, keys) in days_of(config, filenames) {
        let held = summary::read_period(config, &day);
        // A file that will not parse is left exactly as it is. Erasing what cannot be read is the one
        // thing a maintenance pass must not do to somebody's text.
        if held.exists && !held.readable {
            println!("        {day}: summaries not touched — {}", held.note);
            continue;
        }
        out.days_touched += 1;
        let left = if held.absent() {
            0
        } else {
            match summary::prune_period(config, &day, &keys) {
                Ok(dropped) => {
                    out.entries += dropped;
                    held.len() - dropped
                }
                Err(why) => {
                    println!("        {day}: summaries not touched — {why}");
                    continue;
                }
            }
        };
        if erases_the_day(aftermath, left == 0) {
            if summary::prune_daily(config, &day).unwrap_or(false) {
                out.days_removed += 1;
            }
        } else if summary::mark_daily_stale(config, &day).unwrap_or(false) {
            out.days_marked += 1;
        }
    }
    out
}

/// What [`prune_segments`] would do, computed without writing a byte, so `--dry-run` can print a plan
/// rather than a zero. A dry run that reported "0 summaries dropped" would be the one number on this
/// pass that is not a prediction of the real run — and `forget` is the pass a user most wants to see
/// the cost of before typing it.
pub fn plan(config: &Config, filenames: &[String], aftermath: Aftermath) -> Pruned {
    let mut out = Pruned::default();
    for (day, keys) in days_of(config, filenames) {
        let held = summary::read_period(config, &day);
        if held.exists && !held.readable {
            continue;
        }
        out.days_touched += 1;
        let matching = keys.iter().filter(|key| held.get(key).is_some()).count();
        out.entries += matching;
        let emptied = held.len() - matching == 0;
        if erases_the_day(aftermath, emptied) {
            if summary::files::day_path(config, summary::Kind::Daily, &day).exists() {
                out.days_removed += 1;
            }
        } else if summary::read_daily(config, &day).summary.as_ref().is_some_and(|held| !held.stale) {
            out.days_marked += 1;
        }
    }
    out
}

/// Which product day file each of these segments' summaries lives in, and under which key.
///
/// One function for the planning pass and the erasing pass, so the two cannot disagree about which day
/// a name belongs to — the off-by-one at the 03:00 rollover is exactly the kind of thing a read-only
/// preview would get wrong and a real erase would not notice for a month.
fn days_of(config: &Config, filenames: &[String]) -> Vec<(String, Vec<String>)> {
    let shift = config.day_begin_minutes();
    let mut by_day: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in filenames {
        let Some(key) = summary::canonical_key(name) else { continue };
        let day = match LocalParts::from_stamp(&key) {
            Some(stamp) => summary::day_of(stamp.naive_epoch_seconds(), shift),
            None => continue,
        };
        if !by_day.values().any(|held| held.iter().any(|held_key| *held_key == key)) {
            by_day.entry(day).or_default().push(key);
        }
    }
    by_day.into_iter().collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use wind_summary::test_support as support;

    const FIRST: &str = "2026-09-27_09-00-00";
    const SECOND: &str = "2026-09-27_10-00-00";
    const DAY: &str = "2026-09-27";

    /// A day with two stretches and the paragraph written from them.
    fn seed(config: &Config, day: &str, keys: &[&str]) {
        support::seed_day(config, day, keys);
    }

    fn bytes(config: &Config, kind: summary::Kind, day: &str) -> Option<Vec<u8>> {
        std::fs::read(summary::files::day_path(config, kind, day)).ok()
    }

    #[test]
    fn dropping_one_stretch_of_two_flags_the_day_rather_than_deleting_it() {
        let root = support::install("maint-flag-one-of-two");
        let config = support::config_at(&root);
        seed(&config, DAY, &[FIRST, SECOND]);

        let done = prune_segments(&config, &[format!("{FIRST}.mp4")], Aftermath::Flag);
        assert_eq!((done.entries, done.days_marked, done.days_removed), (1, 1, 0), "{done:?}");
        let left = summary::read_period(&config, DAY);
        assert_eq!(left.len(), 1, "the neighbour's paragraph stands");
        assert!(left.get(SECOND).is_some());
        assert!(summary::read_daily(&config, DAY).summary.expect("kept").stale, "and says its premise moved");
        support::cleanup(&root);
    }

    #[test]
    fn a_day_left_with_nothing_standing_loses_its_own_summary_to_forget() {
        let root = support::install("maint-erase-whole-day");
        let config = support::config_at(&root);
        seed(&config, DAY, &[FIRST, SECOND]);

        let done = prune_segments(&config, &[format!("{FIRST}.mp4"), format!("{SECOND}.mp4")], Aftermath::Erase);
        assert_eq!((done.entries, done.days_marked, done.days_removed), (2, 0, 1), "{done:?}");
        assert!(summary::read_period(&config, DAY).absent(), "an emptied day is no file");
        assert!(bytes(&config, summary::Kind::Daily, DAY).is_none(), "and neither is a stale flag on prose the user asked to have gone");
        support::cleanup(&root);
    }

    /// The dry run is a promise, so it has to be the same promise the erase keeps — including which of
    /// the two fates the day meets.
    #[test]
    fn a_plan_reads_the_answer_and_writes_no_byte() {
        let root = support::install("maint-plan-matches");
        let config = support::config_at(&root);
        seed(&config, DAY, &[FIRST, SECOND]);
        let before = (bytes(&config, summary::Kind::Period, DAY), bytes(&config, summary::Kind::Daily, DAY));

        let planned = plan(&config, &[format!("{FIRST}.mp4"), format!("{SECOND}.mp4")], Aftermath::Erase);
        assert_eq!(before, (bytes(&config, summary::Kind::Period, DAY), bytes(&config, summary::Kind::Daily, DAY)), "a plan is read-only");
        assert_eq!(planned, prune_segments(&config, &[format!("{FIRST}.mp4"), format!("{SECOND}.mp4")], Aftermath::Erase), "and is right");

        let root_two = support::install("maint-plan-matches-flag");
        let config_two = support::config_at(&root_two);
        seed(&config_two, DAY, &[FIRST, SECOND]);
        let planned = plan(&config_two, &[format!("{FIRST}.mp4")], Aftermath::Flag);
        assert_eq!(planned, prune_segments(&config_two, &[format!("{FIRST}.mp4")], Aftermath::Flag));
        support::cleanup(&root);
        support::cleanup(&root_two);
    }

    #[test]
    fn nothing_to_reach_is_reported_as_nothing_reached() {
        let root = support::install("maint-nothing");
        let config = support::config_at(&root);
        assert_eq!(prune_segments(&config, &["nonsense.mp4".into(), "".into()], Aftermath::Erase), Pruned::default());
        assert_eq!(plan(&config, &["nonsense.mp4".into()], Aftermath::Erase), Pruned::default());
        // A real key, in an install where nobody ever wrote a summary.
        let done = prune_segments(&config, &[format!("{FIRST}.mp4")], Aftermath::Erase);
        assert_eq!((done.entries, done.days_marked, done.days_removed), (0, 0, 0), "{done:?}");
        support::cleanup(&root);
    }

    #[test]
    fn an_unreadable_day_file_is_left_for_a_human_and_says_so() {
        let root = support::install("maint-unreadable");
        let config = support::config_at(&root);
        seed(&config, DAY, &[FIRST, SECOND]);
        let torn = "{ not json".to_string();
        std::fs::write(summary::files::day_path(&config, summary::Kind::Period, DAY), &torn).expect("torn");

        let done = prune_segments(&config, &[format!("{FIRST}.mp4")], Aftermath::Erase);
        assert_eq!(done, Pruned::default(), "a file that will not parse is not this pass's to guess at");
        assert_eq!(std::fs::read_to_string(summary::files::day_path(&config, summary::Kind::Period, DAY)).expect("still there"), torn);
        assert!(!summary::read_daily(&config, DAY).summary.expect("untouched").stale);
        assert_eq!(plan(&config, &[format!("{FIRST}.mp4")], Aftermath::Erase), Pruned::default(), "and a plan says the same");
        support::cleanup(&root);
    }

    #[test]
    fn a_segment_before_the_rollover_is_dropped_from_the_day_that_owns_it() {
        let root = support::install("maint-rollover");
        let config = support::config_at(&root);
        let early = "2026-09-27_02-00-00";
        let later = "2026-09-28_10-00-00";
        seed(&config, "2026-09-26", &[early]);
        seed(&config, "2026-09-28", &[later]);

        let done = prune_segments(&config, &[format!("{early}.mp4")], Aftermath::Flag);
        assert_eq!((done.entries, done.days_marked), (1, 1), "{done:?}");
        assert!(summary::read_period(&config, "2026-09-26").absent(), "02:00 is the 26th's, whatever the calendar says");
        assert_eq!(summary::read_period(&config, "2026-09-28").len(), 1, "and no other day was reached");
        assert!(!summary::read_daily(&config, "2026-09-28").summary.expect("kept").stale);
        support::cleanup(&root);
    }

    #[test]
    fn the_report_carries_every_count_and_reads_the_same_whether_planned_or_done() {
        let planned = Pruned { entries: 3, days_marked: 1, days_removed: 2, days_touched: 6 }.report(true);
        let done = Pruned { entries: 3, days_marked: 1, days_removed: 2, days_touched: 6 }.report(false);
        assert!(planned.contains("3 stretch paragraph(s) would be dropped"), "{planned}");
        assert!(planned.contains("1 day(s) would be marked stale"), "{planned}");
        assert!(planned.contains("2 day summary(ies) would be deleted"), "{planned}");
        assert!(done.contains("3 stretch paragraph(s) dropped") && done.contains("1 day(s) marked stale") && done.contains("2 day summary(ies) deleted"), "{done}");
        assert!(done.contains("no binary in this product can produce it again"), "{done}");
    }
}
