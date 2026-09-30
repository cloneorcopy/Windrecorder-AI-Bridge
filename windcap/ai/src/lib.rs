//! `wind-ai` — the AI features of Windrecorder, ported.
//!
//! Two things live here, and they share one client. **Natural-language search** (`search`) turns a
//! phrase like "那封关于续约的邮件，上周下午" into a structured query and hands it to
//! `wind_store::search` unchanged. **Monthly activity tags** (`tags`) batch a month's window titles,
//! ask the model what they add up to, and cache the answer by content so a re-run costs nothing.
//!
//! Three properties hold across both, and each has a module dedicated to it:
//!
//!   * **The API key is inert everywhere except one header.** Read from `userdata/config_user.json`
//!     by `settings`, never accepted from argv or the environment, and stripped from every error by
//!     `error::Faults` at construction rather than at print time. See `error` for why that order is
//!     the one that cannot be bypassed.
//!   * **A model answer is data, never an instruction.** `plan` validates and clamps everything that
//!     comes back as JSON; the only thing that reaches the database is a `wind_store::search::Query`
//!     whose fields went through that filter, so no string the model produced can become SQL, a path,
//!     or a wildcard.
//!   * **Dates cross the epoch axis in exactly one place.** `wind_base::clock` stores `videofile_time`
//!     as naive-local seconds, not POSIX seconds; `dates` is the only module allowed to translate
//!     between that and the `%Y-%m-%d` text a model thinks in, because getting it wrong is eight hours
//!     wrong and the user blames the search.
//!
//! The transport is raw Win32 WinHTTP (`http`, `ffi`): no HTTP crate exists in `Cargo.lock`, and
//! adding one would make every future offline build depend on somebody's cargo cache. TLS trust is
//! delegated to Schannel and the machine's certificate store rather than implemented.

pub mod args;
pub mod client;
pub mod dates;
pub mod error;
pub mod hashing;
pub mod http;
#[cfg(windows)]
mod ffi;
pub mod library;
pub mod plan;
pub mod prompt;
pub mod search;
pub mod settings;
pub mod summarize;
pub mod tags;

#[cfg(test)]
mod test_support;

pub use client::{ChatRequest, Client, Completion, Transport, Usage, WinHttp};
pub use error::{AiError, ErrorKind, Faults};
pub use library::Index;
pub use plan::{Occurrence, SearchPlan};
pub use search::Outcome as SearchOutcome;
pub use settings::{SecretKey, Settings, KEY_PLACEHOLDER};
pub use tags::{MonthTags, TagRun, Tags, TitleTable};

/// The version stamped into the `User-Agent`. Kept next to the crate it describes so it cannot drift
/// into a string that promises something the binary does not do.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
