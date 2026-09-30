//! `wind-setup` — first-run onboarding, OCR engine discovery, and the re-entrant upgrade migration.
//!
//! This is the library half of the `windsetup` binary. It exists as a library because the two things it
//! does are the two things in this workspace that operate on data a user cannot recreate — a config file
//! that every process start reconciles, and years of monthly SQLite indexes that a half-finished upgrade
//! has to leave readable — and neither behaviour should have to be tested by spawning a process and
//! reading its stdout.
//!
//! # What is ported, and what deliberately is not
//!
//! `onboard_setting.py` is an interactive six-step wizard whose only durable outputs are `userdata/`, a
//! seeded `userdata/config_user.json`, a chosen `ocr_engine`/`ocr_lang`, and the onboarding markdown the
//! web UI shows until the index has rows in it. The wizard's prompts are a first-run experience and
//! belong to the interface; everything it *writes* is here: [`layout`] creates the tree,
//! [`configfile::seed`] seeds it without ever clobbering, [`engines`] probes what this machine can
//! actually run, and [`doctor`] reports the rest.
//!
//! `upgrade_migration_routine.py` is ported step for step in [`migrate`], with three classes of change
//! that are the point of the rewrite: nothing is deleted ([`backup::move_to_trash`] instead), every write
//! is preceded by a backup whose digest was re-read, and the run is re-entrant with the record of what it
//! did in [`marker`] rather than in the assumption that it ran.
//!
//! # Known gaps in the shared crates
//!
//! Two helpers belong upstream and are private here instead, because the alternative was editing a crate
//! another agent is working in:
//!
//!   * [`pathguard::inside`] duplicates `windcap/maint/src/layout.rs`'s `inside()`/`normalize()`. The
//!     rules are identical and the comment explaining why `canonicalize` is not usable should live in one
//!     place — this wants to be `wind_base::paths`.
//!   * [`migrate::inspect`] opens a month file read-only *without* staging a copy, which
//!     `wind_store::read::Month::open_read` cannot do: it goes through `_TEMP_READ.db`, and creating that
//!     copy is a write. A `--dry-run` and a `doctor` both need the read-only form.

pub mod backup;
pub mod configfile;
pub mod doctor;
pub mod engines;
pub mod hash;
pub mod layout;
pub mod marker;
pub mod migrate;
pub mod pathguard;
