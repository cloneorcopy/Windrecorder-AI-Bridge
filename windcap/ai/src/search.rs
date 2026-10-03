//! Natural-language search: a sentence in, the index asked, rows back.
//!
//! The value of this feature is entirely in the *mapping*, so the mapping is the part that is
//! validated. `plan::build_plan` stands between the model and `wind_store::search`, and what reaches
//! the database is a `Query` built from fields that survived it. Two consequences worth stating,
//! because they look like omissions:
//!
//!   * **Model output never becomes SQL, a path, or a pattern.** `wind-store` binds every value, which
//!     kills injection; `plan` additionally strips the two `LIKE` metacharacters, which bind but still
//!     *mean* something; and the window-title filter is a literal substring test rather than a
//!     model-authored regex.
//!   * **The order is oldest-first or newest-first, decided locally.** `occurrence` comes back from the
//!     model as one of three words, and everything else about sort order is this module's choice.
//!
//! Upstream's `summarize_results` — the third leg of the extension, which pasted the top 30 hits' OCR
//! text into a prompt and asked for a paragraph — is deliberately not ported. It is the only code path
//! in the whole AI surface that ships the user's captured screen text off the machine, and the port's
//! rule is that OCR text does not leave except for the single request a feature needs. Search answers
//! the question by returning moments; the reading is the user's.

use wind_store::read::Row;

use crate::client::{ChatRequest, Client, Transport, Usage};
use crate::error::{AiError, Faults};
use crate::library::Index;
use crate::plan::{self, Occurrence, SearchPlan};
use crate::prompt;

/// Upstream's temperature for query parsing: the answer has to be parseable, not imaginative.
pub const PARSE_TEMPERATURE: f64 = 0.2;

/// The most rows one search will pull into memory.
///
/// `occurrence = last` needs the *newest* matching row, and `search_months` merges oldest-first and
/// trims from the head — so "the last time I did X" is answered correctly by taking the tail of a
/// bounded scan rather than by loading four years of OCR text. The true match count still comes back
/// from the `COUNT(*)`, which is what makes the cap visible instead of silently wrong.
pub const SCAN_CAP: usize = 20_000;

/// Everything a natural-language search produced, kept separate so the CLI can print the decision and
/// the rows as two different things.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub phrase: String,
    pub plan: SearchPlan,
    /// In the order the caller should print them.
    pub rows: Vec<Row>,
    /// Every row the query matched, before the display limit.
    pub total: i64,
    /// Rows the query matched that the window-title filter then removed.
    pub dropped_by_title_filter: usize,
    /// Months the query actually opened.
    pub months_searched: usize,
    /// `rows.len()` hit the scan cap, so `total` may be reachable but this list is not the whole set.
    pub capped: bool,
    pub usage: Option<Usage>,
}

impl Outcome {
    /// One line per thing this module overrode on the model's behalf. Empty means the search the model
    /// described is the search that ran.
    pub fn notes(&self) -> &[String] {
        &self.plan.notes
    }
}

/// Ask the model, validate, query, filter, order.
///
/// `display_limit` is how many rows to return for printing; the *query* still ran over the whole
/// validated window.
pub fn run<T: Transport>(
    index: &mut Index,
    client: &Client<T>,
    phrase: &str,
    display_limit: usize,
) -> Result<Outcome, AiError> {
    let (earliest, latest) = index.date_bounds()?;
    let bounds = index
        .bounds()?
        .ok_or_else(|| client.error(|f: &Faults| f.store("the index holds no recorded rows yet")))?;
    let system = prompt::search_system(&index.settings.prompts.search_system, &earliest, &latest, &wind_base::clock::now().date_stamp());
    let completion = client.ask(&ChatRequest {
        system: &system,
        user: phrase,
        temperature: PARSE_TEMPERATURE,
        json_mode: true,
    })?;

    let plan = plan::build_plan(&completion.text, bounds, client.faults())?;
    let months_searched = index.months_in(plan.from, plan.to).len();
    let query = index.query_for(&plan, SCAN_CAP);
    let result = index.run(&query)?;
    let scanned = result.rows.len();
    let total = result.total;

    // The title filter runs after the query, on rows already in hand, exactly as upstream filters its
    // DataFrame — and, unlike upstream, with no regex compiled from a model's output.
    let (mut rows, dropped) = if plan.applications.is_empty() {
        (result.rows, 0usize)
    } else {
        let kept: Vec<Row> = result
            .rows
            .into_iter()
            .filter(|row| plan::matches_application(row.title(), &plan.applications))
            .collect();
        let dropped = scanned - kept.len();
        (kept, dropped)
    };

    // `search_months` merges oldest-first. "First" is that order; everything else is the newest first,
    // because a person scanning their own history wants the most recent thing they saw, not the oldest.
    if plan.occurrence != Occurrence::First {
        rows.reverse();
    }
    // True only when the scan itself was cut short, which is the one way this list can under-represent
    // `total` without saying so.
    let capped = scanned >= SCAN_CAP && total as usize > scanned;
    rows.truncate(display_limit.max(1));

    Ok(Outcome {
        phrase: phrase.to_string(),
        plan,
        rows,
        total,
        dropped_by_title_filter: dropped,
        months_searched,
        capped,
        usage: completion.usage,
    })
}

/// The end-to-end tests live beside a fixture harness in `search_tests.rs` rather than inline, because
/// the harness (a synthetic install plus a loopback listener) is longer than any single case and the
/// pipeline they exercise is the one thing in this crate that touches a socket *and* the index.
#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;

