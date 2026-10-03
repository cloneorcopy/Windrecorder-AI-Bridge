//! `windcapctl` — the terminal front door to the recorded history.
//!
//! Subcommands come in two kinds. The probes (`status`, `bench`, `grab`, `snap`) measure the capture
//! side; the readers (`query`, `day`, `stats`, `inspect`, `bench-search`) walk the monthly index, and
//! `index` is the one command that writes. Both kinds print wall-clock numbers next to their answer,
//! because the whole claim of the rewrite is "the same data, faster" — and a claim nobody can measure
//! in thirty seconds is not a claim.
//!
//! # Console encoding
//!
//! The bytes this binary writes are UTF-8 and correct. A cp936 console — the default for a zh-CN
//! Windows session — renders Chinese as `?`, which looks exactly like a data bug and is not one.
//! Nothing here papers over that: report text is never transcoded, and `query --json` exists so the
//! real bytes can be redirected (`windcapctl query 文件 --json > hits.json`) and read by anything
//! that speaks UTF-8.
//!
//! # Structure
//!
//! Parsing and formatting live in `args`, `range`, `render` and `json`; every file and database
//! access lives in `library`. What is left below is one reporter per subcommand, each returning its
//! whole report as a `String`, so `main` is the only place that prints and every command is
//! assertable from a test.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use wind_base::clock::{self, LocalParts};
use wind_base::image;
use wind_store::aggregate::{self, Timeline};
use wind_store::maintain::{ensure_time_index, text_similarity};
use wind_store::read::Row;
use wind_store::search::Query;
use wind_store::{Month, SimilarChars};

mod args;
mod json;
mod library;
mod render;

use args::{Command, QueryArgs};
use library::{file_name, Library, MonthFacts};
use wind_base::range::{self, Origin, Span};
use render::{bar, clip, clip_chars, column_chart, flatten, millis, offset, quoted_or, resample, ruler, stride_labels, Align, Table};

/// 6 minutes, the resolution the shipped OneDay area chart uses.
const BUCKET_SECS: i64 = 360;
/// Cells the day sparkline is drawn into, so it survives an 80-column console.
const CHART_COLUMNS: usize = 72;
const CHART_HEIGHT: usize = 6;
/// Two rows of the same title further apart than this are two sessions, not one. Upstream's number.
const TITLE_MAX_GAP: i64 = 100;
/// Body preview length, in characters: the trailing column is never padded, so cells are not what
/// is being bounded here.
const BODY_CHARS: usize = 60;
/// Title column width, in terminal cells, because it sits between padded columns.
const TITLE_CELLS: usize = 40;
/// JPEG quality for `snap`: high enough that OCR text inside the shot stays readable.
const SNAP_QUALITY: u8 = 90;
/// `wind_base::paths::STAMP_LEN`, repeated because that crate is not in this binary's dependency
/// list and a name's length does not warrant pulling it in.
const STAMP_LEN: usize = 19;
/// A segment is bounded by `record_seconds`; this generous window is what routes `inspect` to the
/// right month file instead of scanning the whole library.
const SEGMENT_SCAN_SECS: i64 = 6 * 3600;
/// The index `wind_store::maintain::TIME_INDEX` creates. Its name is repeated here because timing
/// both states means being able to take it away again.
const TIME_INDEX: &str = "video_text_time";
/// Queries per state in `index`: enough to beat the timer's resolution on a small file and still
/// finish in a second on a large one.
const INDEX_ITERATIONS: usize = 50;

/// Everything a reporter returns instead of printing.
type Report = Result<String, String>;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = match args::parse(&argv) {
        Ok(command) => command,
        Err(args::ParseError::Help) => {
            println!("{}", args::usage());
            return;
        }
        // Before `open_library`, which is where every other command starts by resolving a root and
        // reading the config: the version is the one answer that must survive having neither.
        Err(args::ParseError::Version) => {
            println!("{}", args::version_line());
            return;
        }
        Err(args::ParseError::Unknown(name)) => {
            eprintln!("{}", args::usage());
            if name.is_empty() {
                eprintln!("error: no command given");
            } else {
                eprintln!("error: unknown command '{name}'");
            }
            std::process::exit(2);
        }
        Err(args::ParseError::Bad(message)) => {
            eprintln!("{}", args::usage());
            eprintln!("error: {message}");
            std::process::exit(2);
        }
    };

    let report = match command {
        Command::Status => Ok(status_report()),
        Command::Bench { iters } => Ok(bench_report(iters)),
        Command::Grab { iters, width, source } => grab_report(iters, width, source),
        Command::Query(query) => query_report(&query),
        Command::Day(day) => day_report(&day),
        Command::Stats(stats) => stats_report(&stats),
        Command::Index(index) => index_report(&index),
        Command::Inspect(inspect) => inspect_report(&inspect),
        Command::Snap(snap) => snap_report(&snap),
        Command::BenchSearch(bench) => bench_search_report(&bench),
    };

    match report {
        Ok(text) => {
            print!("{text}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    }
}

/// Any error becomes the message the user sees; none of them carry a stack worth printing.
fn reason(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// A `key  value` line. Keys are ASCII literals, so byte padding is cell-accurate.
fn labelled(key: &str, value: &str) -> String {
    format!("{key:<11} {value}\n")
}

/// A library, or a clear message saying the index is not where the root claims it is.
fn open_library(root: Option<PathBuf>) -> Result<Library, String> {
    let library = Library::open(library::resolve_root(root))?;
    if !library.db_dir().exists() {
        return Err(format!(
            "{} has no index directory at {}",
            library.root.display(),
            library.db_dir().display()
        ));
    }
    Ok(library)
}

/// The month files a report should name, or the honest placeholder.
fn month_names(months: &[Month]) -> String {
    if months.is_empty() {
        "<none>".to_string()
    } else {
        months.iter().map(file_name).collect::<Vec<_>>().join(", ")
    }
}

fn endpoint(seconds: Option<i64>) -> String {
    seconds.map(|t| LocalParts::from_naive_epoch(t).display()).unwrap_or_else(|| "<none>".to_string())
}

/// The one place a `Query` is assembled, so `query` and the benchmark battery cannot drift apart on
/// whether an empty exclude string means "exclude nothing".
fn build_query(
    span: &Span,
    keywords: &str,
    exclude: &str,
    similar: Option<SimilarChars>,
    size: usize,
    page: usize,
) -> Query {
    let mut query = Query::new(span.from, span.to).with_keywords(keywords);
    if !exclude.is_empty() {
        query = query.with_exclude(exclude);
    }
    if let Some(table) = similar {
        query = query.with_similar(table);
    }
    // Page numbers are 1-based because that is how the WebUI counts; a zero would make `Query::page`
    // skip the whole first page.
    query.page(size.max(1), page.max(1))
}

// ---------------------------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------------------------

/// The search a user came here for.
///
/// `--json` replaces the whole report with one JSON object per line and nothing else on stdout: the
/// point of that mode is being piped into `jq` or redirected to a file, where a header line is a
/// parse error and a redirected file is the only reliable way to read Chinese out of this tool.
fn query_report(options: &QueryArgs) -> Report {
    let library = open_library(options.root.clone())?;
    let span = range::resolve(&options.window, library.day_begin_minutes(), clock::now())?;
    let months = library.months_covering(span.from, span.to);
    let size = options.size.unwrap_or_else(|| library.config.i64_or("max_page_result", 20).max(1) as usize);
    let page = options.page.unwrap_or(1);
    // `--exact` is the only way to see what the glyph table contributes. Without that switch a
    // widened hit count looks like a bug in the search rather than the deliberate recall trade it is.
    let similar = if options.exact { None } else { library.similar() };
    let query = build_query(&span, &options.keywords, options.exclude.as_deref().unwrap_or(""), similar, size, page);

    let (found, ms) = library.rows_in(&months, &query)?;
    if options.json {
        return Ok(json_records(&found.rows));
    }

    let mut out = String::new();
    out.push_str(&query_context(options, &span, &months, size, page, found.total, found.page_count()));
    let mut table = Table::new(
        &["time", "offset", "video", "title", "body"],
        &[Align::Left, Align::Right, Align::Left, Align::Left, Align::Left],
    );
    for row in &found.rows {
        table.push(row_cells(row));
    }
    if found.rows.is_empty() {
        out.push_str("  (no rows on this page)\n");
    } else {
        out.push_str(&table.render());
        out.push('\n');
    }
    out.push_str(&query_footer(found.rows.len(), found.total, months.len(), ms));
    Ok(out)
}

/// The lines that make a result set explainable after the fact.
fn query_context(
    options: &QueryArgs,
    span: &Span,
    months: &[Month],
    size: usize,
    page: usize,
    total: i64,
    pages: usize,
) -> String {
    format!(
        "keywords {}  |  exclude {}  |  similar {}  |  window {} ({}, day_begin {})\n\
         {}  |  page {page}/{} of {total} hits, {size} per page\n",
        quoted_or(&options.keywords, "<everything in range>"),
        quoted_or(options.exclude.as_deref().unwrap_or(""), "<none>"),
        if options.exact { "off (--exact)" } else { "on" },
        span.label(),
        span.origin.label(),
        span.day_begin_label(),
        months_report(months),
        pages.max(1),
    )
}

/// Months are the unit of cost in this design, so every report names the ones it routed to.
fn months_report(months: &[Month]) -> String {
    format!("{} month file(s): {}", months.len(), month_names(months))
}

/// One result row as table cells, in the column order the header declares.
fn row_cells(row: &Row) -> Vec<String> {
    vec![
        row.when().display()[11..].to_string(),
        row.offset_in_segment().map(offset).unwrap_or_else(|| "?".to_string()),
        if row.video_exists { "yes" } else { "no" }.to_string(),
        clip(&flatten(row.title().unwrap_or("")), TITLE_CELLS),
        clip_chars(&flatten(row.body()), BODY_CHARS),
    ]
}

/// `N of M hits across K month files in T ms (X ms/query)`.
///
/// X is the cost per month file opened — the number that grows as the user's history grows, and
/// therefore the one worth watching between releases.
fn query_footer(shown: usize, total: i64, months: usize, ms: f64) -> String {
    format!(
        "{shown} of {total} hits across {months} month files in {ms:.3} ms ({:.3} ms/query)\n",
        ms / months.max(1) as f64
    )
}

/// One JSON object per line, carrying what a caller needs to open the video itself.
fn json_records(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        let month = row.month_path.as_deref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let line = json::object(&[
            ("videofile_name", json::Field::Text(&row.videofile_name)),
            ("time", json::Field::Int(row.time)),
            ("time_display", json::Field::Text(&row.when().display())),
            ("offset_in_segment", json::Field::OptionalInt(row.offset_in_segment())),
            ("title", json::Field::Text(row.title().unwrap_or(""))),
            ("body", json::Field::Text(row.body())),
            ("thumbnail_base64", json::Field::Text(row.thumbnail.as_deref().unwrap_or(""))),
            // Past the minimal set, but a caller that skips `video_exists` will try to open a file
            // that is not there and blame this tool.
            ("video_exists", json::Field::Bool(row.video_exists)),
            ("month_path", json::Field::Text(&month)),
        ]);
        out.push_str(&line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------------------------
// day
// ---------------------------------------------------------------------------------------------

/// The OneDay view: how much there is, when it happened, where the time went, and a strip.
fn day_report(options: &args::DayArgs) -> Report {
    let library = open_library(options.root.clone())?;
    let (year, month, day) = range::parse_date(&options.day).ok_or_else(|| format!("invalid day '{}'", options.day))?;
    let span = range::product_day(year, month, day, library.day_begin_minutes(), Origin::Day);
    let months = library.months_covering(span.from, span.to);
    let (found, ms) = library.rows_in(&months, &Query::new(span.from, span.to))?;

    let mut out = String::new();
    out.push_str(&labelled(
        "day",
        &format!("{} ({}, day_begin {})", options.day, span.origin.label(), span.day_begin_label()),
    ));
    out.push_str(&labelled("window", &span.label()));
    out.push_str(&labelled("months", &months_report(&months)));
    out.push_str(&labelled("read", &format!("{} hits in {:.3} ms", found.total, ms)));

    let overview = aggregate::overview(&found.rows, span.from, span.to, BUCKET_SECS, library.config.presence_gap_secs());
    out.push_str(&labelled("rows", &overview.rows.to_string()));
    out.push_str(&labelled("first", &endpoint(overview.first)));
    out.push_str(&labelled("last", &endpoint(overview.last)));
    out.push_str(&labelled(
        "active",
        &format!("{:.3} h across {} buckets of {} min", overview.hours(), overview.buckets.len(), BUCKET_SECS / 60),
    ));
    // Say which ruler produced that, because it is the user's own recording settings answering, not
    // a constant buried in this binary: `windui`'s day header and this line must be the same number.
    out.push_str(&labelled(
        "presence gap",
        &format!("{} min — a stretch of nothing captured longer than this is counted as this", library.config.presence_gap_secs() / 60),
    ));
    let labels = stride_labels(&overview.buckets.iter().map(|b| b.label.clone()).collect::<Vec<_>>(), 8);
    out.push_str(&sparkline(&overview.buckets.iter().map(|b| b.count).collect::<Vec<_>>(), &labels));

    out.push_str(&format!("\nwhere the time went, top 10 (titles merged across gaps under {TITLE_MAX_GAP} s)\n"));
    let totals = aggregate::title_totals(&found.rows, TITLE_MAX_GAP);
    if totals.is_empty() {
        out.push_str("  no window title was recorded for this day\n");
    } else {
        let peak = totals.iter().map(|(_, seconds)| *seconds).max().unwrap_or(0);
        let mut table = Table::new(&["duration", "share", "title"], &[Align::Right, Align::Left, Align::Left]);
        for (title, seconds) in totals.iter().take(10) {
            table.push(vec![clock::seconds_to_hhmmss(*seconds), bar(*seconds, peak, 20), clip(&flatten(title), TITLE_CELLS)]);
        }
        out.push_str(&table.render());
        out.push('\n');
    }

    out.push_str(&timeline_strip(&found.rows, &span, options.detail));
    Ok(out)
}

/// A 6-minute-bucket ASCII chart of one day, with an hour ruler under it.
fn sparkline(counts: &[usize], labels: &[String]) -> String {
    let columns = resample(counts, CHART_COLUMNS);
    let width = columns.len().max(1);
    let mut out = format!("\nactivity, {} buckets of {} min drawn in {width} columns\n", counts.len(), BUCKET_SECS / 60);
    for line in column_chart(&columns, CHART_HEIGHT) {
        out.push_str(&line);
        out.push('\n');
    }
    let (ticks, text) = ruler(width, labels);
    out.push_str(&ticks);
    out.push('\n');
    out.push_str(&text);
    out.push('\n');
    out
}

/// The scrubber, drawn with text.
///
/// A terminal cannot show the stored base64 thumbnails, so each sample prints the body text captured
/// at that instant instead — which is the part a user actually scans for. The thumbnail bytes are not
/// lost: `query --json` carries `thumbnail_base64` for every row.
fn timeline_strip(rows: &[Row], span: &Span, detail: usize) -> String {
    let strip = Timeline::sample(rows, span.from, span.to, detail);
    let mut out = format!(
        "\ntimeline ({} of {detail} samples, placed by time so a slot means a slice of the day)\n",
        strip.points.len()
    );
    if strip.points.is_empty() {
        out.push_str("  nothing recorded in this window\n");
        return out;
    }
    for (index, row) in strip.points.iter().enumerate() {
        let (from, to) = strip.spans.get(index).copied().unwrap_or((row.time, row.time));
        out.push_str(&format!(
            "  {:>2}  {} .. {}  {:<8} {}\n",
            index + 1,
            &LocalParts::from_naive_epoch(from).display()[11..],
            &LocalParts::from_naive_epoch(to).display()[11..],
            clip(&flatten(row.title().unwrap_or("-")), 8),
            clip_chars(&flatten(row.body()), BODY_CHARS),
        ));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------------------------

/// Per-day rows for one month, plus the whole library's totals.
fn stats_report(options: &args::StatsArgs) -> Report {
    let library = open_library(options.root.clone())?;
    let facts = library.facts()?;
    let mut out = String::new();
    out.push_str(&labelled("root", &library.root.display().to_string()));
    out.push_str(&labelled("index dir", &library.db_dir().display().to_string()));
    out.push_str(&labelled("months", &facts.len().to_string()));
    if facts.is_empty() {
        out.push_str("\nno month files under the index directory\n");
        return Ok(out);
    }
    out.push_str(&labelled("rows", &facts.iter().map(|f: &MonthFacts| f.rows).sum::<i64>().to_string()));
    // Summed per file: a segment that straddles a month boundary is indexed in both files and so
    // counted twice. Naming the approximation beats quietly calling an estimate exact.
    let segments = facts.iter().map(|f| f.segments).sum::<i64>();
    out.push_str(&labelled("segments", &format!("{segments} (distinct names, summed per month file)")));
    out.push_str(&labelled("earliest", &endpoint(facts.iter().filter_map(|f| f.bounds.map(|(from, _)| from)).min())));
    out.push_str(&labelled("latest", &endpoint(facts.iter().filter_map(|f| f.bounds.map(|(_, to)| to)).max())));
    out.push_str(&labelled("span", &format!("{} calendar day(s) covered", span_days(&facts))));

    let requested = options.month.clone();
    let wanted = match &requested {
        Some(text) => Some(range::parse_month(text)?),
        None => None,
    };
    let month = match &requested {
        Some(text) => library.months.iter().find(|m| &format!("{:04}-{:02}", m.year, m.month) == text),
        // No month named means the newest file, which is the one a user asking "how much have I
        // recorded" means. "Today" would be empty on a machine whose recorder has been off.
        None => library.months.last(),
    };
    let Some(month) = month else {
        return Ok(format!(
            "{}no month file for {}; the library holds {}\n",
            out,
            requested.unwrap_or_else(|| "?".into()),
            library.months.iter().map(|m| format!("{:04}-{:02}", m.year, m.month)).collect::<Vec<_>>().join(", ")
        ));
    };

    // Which physical file the histogram comes from, with its own totals: a `--month` search that
    // turns out to be filtered down to half a file is otherwise invisible.
    if let Some(facts) = facts.iter().find(|f| &f.month == month) {
        out.push_str(&labelled(
            "month file",
            &format!("{} ({} rows, {} segments in the file)", file_name(&facts.month), facts.rows, facts.segments),
        ));
    }

    let all = library.rows_of(month)?;
    let in_month: Vec<Row> = match &wanted {
        Some(span) => all.into_iter().filter(|r| r.time >= span.from && r.time <= span.to).collect(),
        None => all,
    };
    let stats = aggregate::histogram(&in_month, library.day_begin_minutes(), library.config.presence_gap_secs());
    let shift = Span { from: 0, to: 0, origin: Origin::Month, day_begin_minutes: library.day_begin_minutes() };
    out.push_str(&format!(
        "\nday by day for {:04}-{:02} ({} rows, product days beginning at {})\n",
        month.year,
        month.month,
        in_month.len(),
        shift.day_begin_label(),
    ));
    if stats.is_empty() {
        out.push_str("  nothing recorded\n");
        return Ok(out);
    }
    let peak = stats.iter().map(|s| s.rows).max().unwrap_or(0);
    let mut table = Table::new(&["date", "rows", "hours", "share"], &[Align::Left, Align::Right, Align::Right, Align::Left]);
    for day in &stats {
        table.push(vec![
            format!("{:04}-{:02}-{:02}", day.year, day.month, day.day),
            day.rows.to_string(),
            format!("{:.2}", day.hours),
            bar(day.rows as i64, peak as i64, 30),
        ]);
    }
    out.push_str(&table.render());
    out.push('\n');
    let busiest = stats.iter().max_by_key(|s| s.rows).map(|s| format!("{:04}-{:02}-{:02}", s.year, s.month, s.day)).unwrap_or_default();
    out.push_str(&labelled("busiest", &busiest));
    Ok(out)
}

/// How many calendar days the library actually covers, from its own bounds.
fn span_days(facts: &[MonthFacts]) -> i64 {
    let from = facts.iter().filter_map(|f| f.bounds.map(|(a, _)| a)).min();
    let to = facts.iter().filter_map(|f| f.bounds.map(|(_, b)| b)).max();
    match (from, to) {
        (Some(a), Some(b)) if b >= a => (b - a) / 86_400 + 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// index
// ---------------------------------------------------------------------------------------------

/// The evidence for adding an index the Python app never had.
///
/// This is the only subcommand that modifies a user's index. It drops `TIME_INDEX`, times the same
/// query fifty times without it, then creates it and times again — so the net state of a database
/// that already had the index is unchanged, and one that did not now has it. Timing runs on the live
/// file rather than the `_TEMP_READ.db` copy on purpose: both measurements must describe one set of
/// bytes, and a copy taken before the create would not see the index at all.
fn index_report(options: &args::IndexArgs) -> Report {
    let library = open_library(options.root.clone())?;
    if library.months.is_empty() {
        return Err(format!("no month files under {}", library.db_dir().display()));
    }
    let mut out = String::new();
    out.push_str(&labelled("index dir", &library.db_dir().display().to_string()));
    out.push_str(&labelled("mode", if options.status_only { "status only, nothing is created" } else { "ensure the index, timing both states" }));
    let mut created = 0usize;
    let mut present = 0usize;

    for month in &library.months {
        let name = clip(&file_name(month), 30);
        if options.status_only {
            // Zero staleness forces the read copy to be refreshed first: a stale copy would answer
            // "ABSENT" about a file that has been indexed since.
            let conn = month.open_read(Duration::ZERO, library.maintaining()).map_err(reason)?;
            let (rows, _) = file_totals(&conn)?;
            let has = index_present(&conn)?;
            out.push_str(&labelled(&name, &format!("index {} ({rows} rows)", if has { "present" } else { "ABSENT" })));
            if has {
                present += 1;
            }
            continue;
        }

        let conn = month.open_write().map_err(reason)?;
        let (rows, newest) = file_totals(&conn)?;
        let had = index_present(&conn)?;
        out.push_str(&labelled(&name, &format!("{rows} rows, index {}", if had { "present" } else { "absent" })));
        // Put the index in place before anything can fail, so an early return still leaves the file
        // better off than it found it rather than half-measured.
        ensure_time_index(&conn).map_err(reason)?;
        if had {
            present += 1;
        } else {
            created += 1;
        }
        let Some(newest) = newest else {
            // Nothing to time in an empty file, but the index still belongs there: the recorder's
            // next segment will be queried the moment it exists.
            out.push_str("  (no rows: index ensured, nothing to time)\n");
            continue;
        };

        // The last indexed hour is the shape of a day view, which is what a plain index on the
        // timestamp column serves. A query returning every row of the month is a full scan either way
        // and would prove nothing.
        let from = newest - 3600;
        conn.execute_batch("DROP INDEX IF EXISTS video_text_time").map_err(reason)?;
        let absent = time_month(month, from, newest)?;
        ensure_time_index(&conn).map_err(reason)?;
        let with = time_month(month, from, newest)?;

        out.push_str(&format!(
            "  {:<11} {} .. {}  (last hour of {rows} rows, {INDEX_ITERATIONS} queries per state)\n",
            "window",
            LocalParts::from_naive_epoch(from).display(),
            LocalParts::from_naive_epoch(newest).display(),
        ));
        out.push_str(&format!("  {:<11} {}\n", "no index", timing_line(&absent)));
        out.push_str(&format!("  {:<11} {}\n", "with index", timing_line(&with)));
        out.push_str(&format!("  {:<11} {}\n", "ratio", ratio_line(&absent, &with)));
    }
    let total = if options.status_only {
        format!("{present}/{} month files carry the index", library.months.len())
    } else {
        format!("{created} created, {present} already present")
    };
    out.push_str(&labelled("total", &total));
    if options.status_only {
        out.push_str("no index file was modified\n");
    }
    Ok(out)
}

/// `(row count, newest timestamp)` for one open file.
///
/// Both are aggregate statements rather than a materialised row set: this runs either side of a
/// schema change on the live file, and pulling a month of OCR text into memory to count it would
/// dominate the very timing being measured.
fn file_totals(conn: &Connection) -> Result<(i64, Option<i64>), String> {
    let rows = wind_store::read::count_rows(conn).map_err(reason)?;
    let newest = conn
        .query_row("SELECT MAX(videofile_time) FROM video_text", [], |r| r.get::<_, Option<i64>>(0))
        .map_err(reason)?;
    Ok((rows, newest))
}

/// Does this file already carry the timestamp index?
fn index_present(conn: &Connection) -> Result<bool, String> {
    // The name is a constant this module owns, so interpolating it is not the injection the search
    // module refuses to commit with user text.
    let sql = format!("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='{TIME_INDEX}'");
    Ok(conn.query_row(&sql, [], |r| r.get::<_, i64>(0)).map_err(reason)? > 0)
}

/// One state's measurement of the same query, on a freshly opened handle.
fn time_month(month: &Month, from: i64, to: i64) -> Result<Timing, String> {
    let conn = month.open_write().map_err(reason)?;
    let span = Span { from, to, origin: Origin::Explicit, day_begin_minutes: 0 };
    let query = build_query(&span, "", "", None, 20, 1);
    let mut timing = Timing::new(INDEX_ITERATIONS);
    for _ in 0..INDEX_ITERATIONS {
        let started = Instant::now();
        let hits = wind_store::search::search(&conn, &query).map_err(reason)?;
        timing.observe(started.elapsed().as_secs_f64() * 1000.0, hits.len());
    }
    Ok(timing)
}

/// Measured milliseconds for one state of one month file.
#[derive(Debug, Clone, Copy)]
struct Timing {
    iterations: usize,
    best: f64,
    total: f64,
    rows: usize,
}

impl Timing {
    fn new(iterations: usize) -> Timing {
        Timing { iterations, best: f64::INFINITY, total: 0.0, rows: 0 }
    }

    fn observe(&mut self, ms: f64, rows: usize) {
        self.best = self.best.min(ms);
        self.total += ms;
        self.rows = rows;
    }

    fn mean(&self) -> f64 {
        if self.iterations == 0 {
            0.0
        } else {
            self.total / self.iterations as f64
        }
    }
}

fn timing_line(timing: &Timing) -> String {
    format!("best {}  mean {}  ({} rows per query)", millis(timing.best), millis(timing.mean()), timing.rows)
}

/// `4.10x faster`. Greater than one means the index won — including the honest case where it is
/// below one, because a file too small to need a seek pays for the extra B-tree walk instead.
fn ratio_line(absent: &Timing, with: &Timing) -> String {
    let (before, after) = (absent.mean(), with.mean());
    if before <= 0.0 || after <= 0.0 {
        return "not measurable at this size".to_string();
    }
    let ratio = before / after;
    if ratio >= 1.0 {
        format!("{ratio:.2}x faster with the index")
    } else {
        format!("{:.2}x slower with the index at {} rows", 1.0 / ratio, absent.rows)
    }
}

// ---------------------------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------------------------

/// Every row of one segment, oldest first, with the similarity the recorder's dedup line would have
/// scored against the row before it.
///
/// The metric is `wind_store::maintain::text_similarity` — character-set Jaccard, weak but
/// load-bearing, because the history on a user's disk was already cut by exactly it.
fn inspect_report(options: &args::InspectArgs) -> Report {
    let library = open_library(options.root.clone())?;
    // A segment name starts with its own timestamp, so the file it lives in is known without a scan.
    let (from, to) = match LocalParts::from_stamp(&options.segment) {
        Some(stamp) => {
            let at = stamp.naive_epoch_seconds();
            (at, at + SEGMENT_SCAN_SECS)
        }
        // A name that is not stamped (a screenshot row, or a typo) has to be looked for everywhere.
        None => (i64::MIN / 4, i64::MAX / 4),
    };
    let months = library.months_covering(from, to);
    if months.is_empty() {
        return Err(format!("no month file covers '{}'", options.segment));
    }
    let mut rows: Vec<Row> = Vec::new();
    for month in &months {
        let (found, _) = library.rows_in(std::slice::from_ref(month), &Query::new(from, to))?;
        rows.extend(found.rows.into_iter().filter(|r| matches_segment(&options.segment, &r.videofile_name)));
    }
    if rows.is_empty() {
        return Err(format!("no rows for segment '{}' in {}", options.segment, month_names(&months)));
    }
    rows.sort_by_key(|r| (r.time, r.rowid));

    let threshold = library.config.f64_or("ocr_compare_similarity_in_table", 0.94);
    let mut out = String::new();
    out.push_str(&labelled("segment", &options.segment));
    out.push_str(&labelled("months", &months_report(&months)));
    out.push_str(&labelled("rows", &rows.len().to_string()));
    out.push_str(&labelled(
        "window",
        &format!(
            "{} .. {}",
            endpoint(rows.first().map(|r| r.time)),
            endpoint(rows.last().map(|r| r.time))
        ),
    ));
    out.push_str(&labelled("dedup", &format!("similarity >= {threshold:.2} against the previous row drops the row")));

    let mut table = Table::new(
        &["time", "offset", "vs prev", "keep?", "video", "title", "body"],
        &[Align::Left, Align::Right, Align::Right, Align::Left, Align::Left, Align::Left, Align::Left],
    );
    let mut dropped = 0usize;
    let mut previous: Option<&Row> = None;
    for row in &rows {
        let similarity = previous.map(|before| text_similarity(before.body(), row.body()));
        let verdict = match similarity {
            None => "first".to_string(),
            Some(score) if score >= threshold => {
                dropped += 1;
                "DROP".to_string()
            }
            Some(_) => "keep".to_string(),
        };
        let mut cells = row_cells(row);
        cells.splice(
            2..2,
            [similarity.map(|s| format!("{s:.3}")).unwrap_or_else(|| "--".to_string()), verdict],
        );
        table.push(cells);
        previous = Some(row);
    }
    out.push_str(&table.render());
    out.push('\n');
    out.push_str(&labelled("would drop", &format!("{dropped} of {} rows", rows.len())));
    Ok(out)
}

/// Does a stored `videofile_name` belong to the segment the user named?
///
/// Exact match first; otherwise the 19-character stamp prefix, because the pipeline renames a
/// segment with `-INDEX`, `-VIDEO` and `-SCREENSHOTS-OCRED` suffixes while its rows keep the name
/// they were written under. Requiring a parseable stamp is what stops `a.mp4` from matching
/// everything.
fn matches_segment(wanted: &str, stored: &str) -> bool {
    if wanted == stored {
        return true;
    }
    let prefix: String = wanted.chars().take(STAMP_LEN).collect();
    prefix.chars().count() == STAMP_LEN && LocalParts::from_stamp(&prefix).is_some() && stored.starts_with(&prefix)
}

// ---------------------------------------------------------------------------------------------
// snap
// ---------------------------------------------------------------------------------------------

/// Grab the desktop and write it out, timed.
///
/// Exists so the native UI can be verified by screenshot from a headless session: the pixels go
/// through one GDI pass into an RGB buffer and straight to a JPEG, with no re-encode from disk.
fn snap_report(options: &args::SnapArgs) -> Report {
    use windcap::capture::{make_thread_dpi_aware, Grabber};
    make_thread_dpi_aware();
    let mut grabber = Grabber::new(options.width).map_err(reason)?;
    let started = Instant::now();
    let frame = match grabber.grab().map_err(reason)? {
        Some(frame) => frame,
        // A monitor change between building the grabber and using it is rare; one rebuild absorbs it.
        None => {
            grabber = Grabber::new(options.width).map_err(reason)?;
            grabber.grab().map_err(reason)?.ok_or_else(|| "display topology kept changing".to_string())?
        }
    };
    let grab_ms = started.elapsed().as_secs_f64() * 1000.0;
    let (w, h) = (frame.width as usize, frame.height as usize);

    let started = Instant::now();
    let jpeg = image::encode_jpeg(&frame.rgb, w, h, SNAP_QUALITY)?;
    let encode_ms = started.elapsed().as_secs_f64() * 1000.0;

    if let Some(parent) = options.path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&options.path, &jpeg).map_err(|e| format!("{}: {e}", options.path.display()))?;

    let mut out = String::new();
    out.push_str(&labelled("wrote", &options.path.display().to_string()));
    out.push_str(&labelled("frame", &format!("{w}x{h} RGB, {} KB JPEG at quality {SNAP_QUALITY}", jpeg.len() / 1024)));
    out.push_str(&labelled("grab", &format!("{grab_ms:.2} ms, one GDI pass straight into RGB")));
    out.push_str(&labelled("encode", &format!("{encode_ms:.2} ms")));
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// bench-search
// ---------------------------------------------------------------------------------------------

/// Which window a benchmark query runs over, resolved against the library rather than against the
/// wall clock, so two runs on one machine measure the same rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Hour,
    Day,
    Month,
}

impl Window {
    fn label(self) -> &'static str {
        match self {
            Window::Hour => "hour",
            Window::Day => "day",
            Window::Month => "month",
        }
    }
}

/// One measured query.
struct Probe {
    label: &'static str,
    keywords: &'static str,
    exclude: &'static str,
    /// Whether the shape-similar Chinese table is applied. This is the difference between the two
    /// `文件` rows and the most expensive knob in the search path.
    expand: bool,
    window: Window,
}

/// A fixed battery, in increasing order of work. `ChatGPT` and `文件` are the two tokens a real
/// zh-CN install is most likely to have indexed; a miss is still a valid timing sample, because the
/// `LIKE` scan costs the same either way.
const BATTERY: &[Probe] = &[
    Probe { label: "empty range, no keywords", keywords: "", exclude: "", expand: false, window: Window::Hour },
    Probe { label: "single token", keywords: "ChatGPT", exclude: "", expand: false, window: Window::Day },
    Probe { label: "two tokens", keywords: "ChatGPT token", exclude: "", expand: false, window: Window::Day },
    Probe { label: "token + exclude", keywords: "ChatGPT", exclude: "Windrecorder", expand: false, window: Window::Day },
    Probe { label: "chinese token, exact", keywords: "文件", exclude: "", expand: false, window: Window::Day },
    Probe { label: "chinese token, expanded", keywords: "文件", exclude: "", expand: true, window: Window::Day },
    Probe { label: "whole month, no keywords", keywords: "", exclude: "", expand: false, window: Window::Month },
    Probe { label: "whole month, one token", keywords: "ChatGPT", exclude: "", expand: false, window: Window::Month },
];

/// The regression net for `wind-store`: the same eight queries, in the same order, every time.
fn bench_search_report(options: &args::BenchSearchArgs) -> Report {
    let library = open_library(options.root.clone())?;
    let latest = library.latest_time()?.ok_or_else(|| format!("no rows under {}", library.db_dir().display()))?;
    let when = LocalParts::from_naive_epoch(latest);
    let day_begin = library.day_begin_minutes();
    let day = range::product_day(when.year, when.month, when.day, day_begin, Origin::Day);
    let hour = Span { from: latest - 3600, to: latest, origin: Origin::Explicit, day_begin_minutes: day_begin };
    let mut month = range::parse_month(&format!("{:04}-{:02}", when.year, when.month))?;
    month.day_begin_minutes = day_begin;
    let size = library.config.i64_or("max_page_result", 20).max(1) as usize;
    let similar = library.similar();

    let mut out = String::new();
    out.push_str(&labelled("root", &library.root.display().to_string()));
    out.push_str(&labelled("anchor", &format!("latest row {}, windows drawn around it", when.display())));
    out.push_str(&labelled("day", &day.label()));
    out.push_str(&labelled("month", &month.label()));
    out.push_str(&labelled("iterations", &options.iterations.to_string()));
    out.push_str(&labelled(
        "glyph table",
        &match &similar {
            Some(table) => format!("{} characters", table.covered_characters()),
            None => "not found; expanded queries fall back to the literal token".to_string(),
        },
    ));
    out.push('\n');

    let mut table = Table::new(
        &["query", "window", "months", "best", "mean", "rows hit"],
        &[Align::Left, Align::Left, Align::Right, Align::Right, Align::Right, Align::Right],
    );
    for probe in BATTERY {
        let span = match probe.window {
            Window::Hour => &hour,
            Window::Day => &day,
            Window::Month => &month,
        };
        let months = library.months_covering(span.from, span.to);
        let query = build_query(
            span,
            probe.keywords,
            probe.exclude,
            if probe.expand { similar.clone() } else { None },
            size,
            1,
        );
        let mut timing = Timing::new(options.iterations.max(1) as usize);
        for _ in 0..options.iterations {
            let started = Instant::now();
            let (found, _) = library.rows_in(&months, &query)?;
            timing.observe(started.elapsed().as_secs_f64() * 1000.0, found.rows.len());
        }
        table.push(vec![
            probe.label.to_string(),
            format!("{} ({} min)", probe.window.label(), span.seconds() / 60),
            months.len().to_string(),
            millis(timing.best),
            millis(timing.mean()),
            timing.rows.to_string(),
        ]);
    }
    out.push_str(&table.render());
    out.push('\n');
    out.push_str("mean covers routing plus both statements per month file; rows hit is the last iteration's page\n");
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// the capture-side probes
// ---------------------------------------------------------------------------------------------

fn status_report() -> String {
    use windcap::winstate;
    let s = winstate::snapshot();
    format!(
        "status={:?} desktop={:?} idle={:?}s quns={:?}\nrecordable={}\n",
        s.status, s.desktop_name, s.idle_seconds, s.notification_state, s.recordable()
    )
}

fn bench_report(iters: u32) -> String {
    use windcap::winstate;
    let mut out = String::new();
    // Warm the handles and DLL pages so the first call does not carry the whole cost.
    for _ in 0..16 {
        let _ = winstate::input_desktop_name();
        let _ = winstate::idle_seconds();
    }
    let probes: [(&str, fn() -> bool); 3] = [
        ("input_desktop_name", || winstate::input_desktop_name().is_some()),
        ("idle_seconds", || winstate::idle_seconds().is_some()),
        ("snapshot", || winstate::snapshot().recordable()),
    ];
    out.push_str(&format!("iters={iters}\n"));
    out.push_str(&format!("{:<22} {:>12} {:>12}\n", "probe", "best(us)", "mean(us)"));
    out.push_str(&format!("{:-<48}\n", ""));
    for (label, f) in probes {
        let mut best = f64::MAX;
        let mut total = 0.0f64;
        for _ in 0..iters {
            let t = Instant::now();
            let _ = f();
            let dt = t.elapsed().as_secs_f64() * 1e6;
            best = best.min(dt);
            total += dt;
        }
        out.push_str(&format!("{:<22} {:>12.3} {:>12.3}\n", label, best, total / f64::from(iters.max(1))));
    }
    out
}

fn grab_report(iters: u32, width: u32, source: Option<(i32, i32)>) -> Report {
    use windcap::capture::{make_thread_dpi_aware, virtual_desktop, Grabber, VirtualDesktop};
    use windcap::gate::{dhash, hamming, ChangeGate, GateConfig};
    let mut out = String::new();
    make_thread_dpi_aware();
    let desktop = virtual_desktop();
    let custom = source.map(|(w, h)| VirtualDesktop { x: 0, y: 0, width: w, height: h });
    out.push_str(&format!(
        "virtual desktop: {}x{} ({:.1} MP) | source rect: {}\n",
        desktop.width,
        desktop.height,
        f64::from(desktop.width * desktop.height) / 1e6,
        custom.map(|s| format!("{}x{}", s.width, s.height)).unwrap_or_else(|| "the live desktop".into())
    ));
    let mut grabber = match custom {
        Some(rect) => Grabber::with_source(width, rect, false),
        None => Grabber::new(width),
    }
    .map_err(reason)?;
    let (tw, th) = grabber.target_size();
    out.push_str(&format!("target buffer: {tw}x{th} luma ({} KB)\n\n", tw as usize * th as usize / 1024));

    let mut gate = ChangeGate::new(GateConfig::default());
    let (mut grab_times, mut gate_times) = (Vec::new(), Vec::new());
    let mut changes = 0u32;
    let mut last_hash = 0u64;
    for i in 0..iters {
        let t = Instant::now();
        let frame = match grabber.grab() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                out.push_str(&format!("topology changed at iter {i}, rebuilding\n"));
                grabber = Grabber::new(width).map_err(reason)?;
                continue;
            }
            Err(e) => return Err(format!("grab failed at iter {i}: {e}")),
        };
        let grab_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let decision = gate.observe(frame.width, frame.height, &frame.luma);
        let hash = dhash(&frame.luma, frame.width as usize, frame.height as usize);
        let gate_ms = t.elapsed().as_secs_f64() * 1000.0;
        if decision.changed {
            changes += 1;
        }
        grab_times.push(grab_ms);
        gate_times.push(gate_ms);
        if i < 5 || decision.changed {
            out.push_str(&format!(
                "iter {:>3}  grab {:>7.2}ms  gate {:>6.2}ms  blocks {:>4}  changed {:>5}  hamming {:>2}\n",
                i,
                grab_ms,
                gate_ms,
                decision.changed_blocks,
                decision.changed,
                if i == 0 { 0 } else { hamming(hash, last_hash) }
            ));
        }
        last_hash = hash;
        std::thread::sleep(Duration::from_millis(120));
    }
    let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    let best = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    out.push_str(&format!(
        "\n{} iters: grab best {:.2}ms mean {:.2}ms | gate+dhash best {:.2}ms mean {:.2}ms | changes {}/{}\n",
        grab_times.len(),
        best(&grab_times),
        mean(&grab_times),
        best(&gate_times),
        mean(&gate_times),
        changes,
        grab_times.len()
    ));
    Ok(out)
}

/// Fixtures for the end-to-end tests below: real month files, written by the real writer into the
/// temp directory, because the only thing that proves the plumbing lines up is a reporter reading
/// bytes that `wind-store` put on disk.
#[cfg(test)]
mod fixtures {
    use std::path::PathBuf;

    use wind_base::clock::LocalParts;
    use wind_store::{Record, Store};

    /// The segment names every row shares its first 19 characters with, per the on-disk convention.
    pub const SEG_FIRST: &str = "2026-09-21_21-16-12.mp4";
    pub const SEG_LATE: &str = "2026-09-22_00-55-00.mp4";
    pub const SEG_MAIN: &str = "2026-09-22_10-00-00.mp4";
    pub const SEG_TAIL: &str = "2026-09-22_19-30-00.mp4";

    pub fn stamp(text: &str) -> i64 {
        LocalParts::from_stamp(text).expect("fixture stamp").naive_epoch_seconds()
    }

    /// A library whose install *config* is also under test.
    ///
    /// Every other fixture here inherits `day_begin_minutes` by not writing a file at all, which is
    /// the shipped default of 180. This one lets a test state the setting explicitly — which is the
    /// only way to prove both that a shifted day is honoured and that an unshifted one is unchanged.
    pub fn library_configured(tag: &str, rows: &[Record], settings: &str) -> PathBuf {
        let dir = root(tag);
        let src = dir.join("windrecorder").join("config_src");
        std::fs::create_dir_all(&src).expect("config_src");
        std::fs::write(src.join("config_default.json"), format!("{settings}\n")).expect("config file");
        let mut store = Store::open_month(&dir.join("userdata").join("db"), "default", 2026, 9).expect("open month");
        store.append(rows).expect("append");
        dir
    }

    pub fn record(segment: &str, time: i64, body: &str, title: Option<&str>) -> Record {
        Record {
            videofile_name: segment.to_string(),
            picturefile_name: format!("{time}.jpg"),
            videofile_time: time,
            ocr_text: body.to_string(),
            win_title: title.map(str::to_string),
            deep_linking: None,
            thumbnail: Some("AAAJRg==".to_string()),
        }
    }

    pub fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcapctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A `userdata/db` month file holding `rows`. `glyph_table` adds the shape-similar Chinese file,
    /// which is what makes the expansion path in `--exact` testable rather than aspirational.
    ///
    /// The table is written next to a `config_default.json`, because that file is what *makes* the
    /// directory the install's settings directory — a bare `config_src/` with a lookup table and no
    /// defaults is a directory the sentinel correctly does not recognise, and a fixture that relied
    /// on it would be testing a layout no install can be in.
    pub fn library(tag: &str, rows: &[Record], glyph_table: bool) -> PathBuf {
        let dir = root(tag);
        let mut store = Store::open_month(&dir.join("userdata").join("db"), "default", 2026, 9).expect("open month");
        store.append(rows).expect("append");
        assert_eq!(store.row_count().expect("count"), rows.len() as i64);
        if glyph_table {
            let src = dir.join(wind_base::install::CONFIG_SRC);
            std::fs::create_dir_all(&src).expect("config_src");
            std::fs::write(src.join(wind_base::install::DEFAULTS_BASENAME), "{}").expect("defaults file");
            std::fs::write(src.join("similar_CN_characters.txt"), "文，又\n").expect("glyph file");
        }
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use args::{BenchSearchArgs, DayArgs, IndexArgs, InspectArgs, StatsArgs};
    use wind_store::Record;

    /// Six rows across two product days, one of them carrying the four characters that break a
    /// hand-rolled JSON writer, and one exact repeat of another row's text for the dedup line.
    fn sample() -> Vec<Record> {
        vec![
            record(SEG_FIRST, stamp("2026-09-21_21-16-12"), "Welcome! Go to Setting", None),
            record(SEG_LATE, stamp("2026-09-22_01-00-00"), "多云 0 0", Some("QQ")),
            record(
                SEG_MAIN,
                stamp("2026-09-22_10-00-00"),
                "文件 ChatGPT \"quote\" back\\slash line\nbell \u{1}",
                Some("ChatGPT"),
            ),
            record(SEG_MAIN, stamp("2026-09-22_10-06-00"), "revenue forecast 收入", Some("ChatGPT")),
            record(SEG_TAIL, stamp("2026-09-22_19-30-00"), "新聊天", Some("Qoder CN")),
            record(SEG_TAIL, stamp("2026-09-22_19-30-30"), "新聊天", Some("Qoder CN")),
        ]
    }

    fn query(keywords: &str, day: &str) -> QueryArgs {
        QueryArgs {
            root: None,
            keywords: keywords.to_string(),
            window: range::RangeArg {
                day: Some(day.to_string()),
                from: None,
                to: None,
            },
            exclude: None,
            page: None,
            size: None,
            exact: false,
            json: false,
        }
    }

    /// The assertion that the whole command exists to make: the day boundary is the product's, not
    /// the calendar's, and the rows come back in the order the recorder wrote them.
    #[test]
    fn query_returns_the_rows_of_one_product_day_and_names_the_files_it_opened() {
        let root = library("query", &sample(), false);
        let report = query_report(&QueryArgs { root: Some(root.clone()), ..query("", "2026-09-22") }).unwrap();
        assert!(report.contains("keywords <everything in range>"), "{report}");
        assert!(report.contains("1 month file(s): default_2026-09_wind.db"), "{report}");
        assert!(report.contains("4 of 4 hits across 1 month files"), "{report}");
        for time in ["10:00:00", "10:06:00", "19:30:00", "19:30:30"] {
            assert!(report.contains(time), "{time} missing from:\n{report}");
        }
        assert!(!report.contains("01:00:00"), "01:00 belongs to the 21st under day_begin 03:00:\n{report}");
        assert!(report.contains("+0:00:00") && report.contains("+0:06:00"), "segment offsets are the seek targets:\n{report}");

        let previous = query_report(&QueryArgs { root: Some(root.clone()), ..query("", "2026-09-21") }).unwrap();
        assert!(previous.contains("2 of 2 hits across 1 month files"), "{previous}");
        assert!(previous.contains("01:00:00"), "{previous}");
        drop(previous);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_keyword_narrows_the_window_and_the_footer_keeps_the_timing_real() {
        let root = library("keyword", &sample(), false);
        let report = query_report(&QueryArgs { root: Some(root.clone()), ..query("ChatGPT", "2026-09-22") }).unwrap();
        assert!(report.contains("keywords 'ChatGPT'"), "{report}");
        assert!(report.contains("2 of 2 hits"), "title-only matches count too:\n{report}");
        let footer = report.lines().find(|l| l.ends_with("ms/query)")).expect("footer");
        let ms: f64 = footer
            .split(" in ")
            .nth(1)
            .and_then(|rest| rest.split(" ms").next())
            .and_then(|num| num.parse().ok())
            .expect("the footer carries a real duration");
        assert!(ms > 0.0, "a measured zero means the timer is not wired up: {footer}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// The one test that shows `--exact` is a real switch and not a label.
    #[test]
    fn the_glyph_table_widens_a_chinese_query_and_exact_turns_it_back_off() {
        let root = library("expand", &sample(), true);
        let expanded = query_report(&QueryArgs { root: Some(root.clone()), ..query("又件", "2026-09-22") }).unwrap();
        assert!(expanded.contains("1 of 1 hits"), "文 is confusable with 又, so 文件 must match:\n{expanded}");
        let exact = query_report(&QueryArgs { root: Some(root.clone()), exact: true, ..query("又件", "2026-09-22") }).unwrap();
        assert!(exact.contains("0 of 0 hits"), "{exact}");
        assert!(exact.contains("similar off (--exact)"), "{exact}");
        assert!(exact.contains("(no rows on this page)"), "{exact}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// `--json` is the escape hatch from a console that cannot show these bytes; it has to be valid
    /// on the way out, including the characters that break a naive writer.
    #[test]
    fn json_output_is_one_object_per_line_and_survives_the_nastiest_row() {
        let root = library("json", &sample(), false);
        let report = query_report(&QueryArgs { root: Some(root.clone()), json: true, ..query("ChatGPT", "2026-09-22") }).unwrap();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 2, "one line per hit, and no header: {report}");
        for line in &lines {
            assert!(line.starts_with("{\"videofile_name\"") && line.ends_with('}'), "{line}");
        }
        assert!(lines[0].contains("\\\""), "an unescaped quote: {}", lines[0]);
        assert!(lines[0].contains(r"back\\slash"), "an unescaped backslash: {}", lines[0]);
        assert!(lines[0].contains(r"\nbell"), "an unescaped newline: {}", lines[0]);
        assert!(lines[0].contains(r"\u0001"), "an unescaped control character: {}", lines[0]);
        assert!(lines[0].contains("\"offset_in_segment\":0"), "{}", lines[0]);
        assert!(lines[0].contains("\"thumbnail_base64\":\"AAAJRg==\""), "{}", lines[0]);
        assert!(report.contains('\n') && !report.contains('\r'), "the record separators are newlines, not CRLF");

        // The same row through the table has its newlines folded, so the table stays one row tall.
        let table = query_report(&QueryArgs { root: Some(root.clone()), ..query("ChatGPT", "2026-09-22") }).unwrap();
        assert!(table.contains("line bell"), "{table}");
        assert_eq!(table.lines().count(), 2 + 2 + 2 + 1, "context, header, rule, two rows, footer:\n{table}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn the_day_report_adds_up_and_draws_a_chart_the_terminal_can_hold() {
        let root = library("day", &sample(), false);
        let report = day_report(&DayArgs { root: Some(root.clone()), day: "2026-09-22".into(), detail: 4 }).unwrap();
        assert!(report.contains(&labelled("rows", "4")), "{report}");
        assert!(report.contains(&labelled("first", "2026-09-22 10:00:00")), "{report}");
        assert!(report.contains(&labelled("last", "2026-09-22 19:30:30")), "{report}");
        assert!(report.contains(&labelled("months", "1 month file(s): default_2026-09_wind.db")), "{report}");
        assert!(report.contains("ChatGPT") && report.contains("Qoder CN"), "where the time went:\n{report}");

        let chart: Vec<&str> = report.lines().skip_while(|l| !l.starts_with("activity,")).skip(1).take_while(|l| {
            l.starts_with('#') || l.starts_with('.')
        }).collect();
        assert_eq!(chart.len(), CHART_HEIGHT, "one line per chart row:\n{report}");
        for line in &chart {
            assert_eq!(line.chars().count(), CHART_COLUMNS, "every row is the full width");
            assert!(line.chars().all(|c| c == '#' || c == '.'), "{line}");
        }
        // 10:00 and 10:06 share a 6-hour slot, 19:30 and 19:30:30 share the next; the other two are
        // empty, and an empty slot draws nothing rather than inventing a thumbnail.
        assert!(report.contains("timeline (2 of 4 samples"), "{report}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn stats_counts_days_in_the_product_calendar_not_the_string_one() {
        let root = library("stats", &sample(), false);
        let report = stats_report(&StatsArgs { root: Some(root.clone()), month: Some("2026-09".into()) }).unwrap();
        assert!(report.contains(&labelled("rows", "6")), "{report}");
        assert!(report.contains(&labelled("months", "1")), "{report}");
        assert!(report.contains(&labelled("earliest", "2026-09-21 21:16:12")), "{report}");
        assert!(report.contains(&labelled("latest", "2026-09-22 19:30:30")), "{report}");
        assert!(report.contains("2026-09-21     2"), "the 01:00 row joins the 21st:\n{report}");
        assert!(report.contains("2026-09-22     4"), "{report}");
        assert!(report.contains(&labelled("busiest", "2026-09-22")), "{report}");
        assert!(report.contains("#"), "every day gets a bar:\n{report}");

        let missing = stats_report(&StatsArgs { root: Some(root.clone()), month: Some("2026-01".into()) }).unwrap();
        assert!(missing.contains("no month file for 2026-01"), "{missing}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// The pair a user actually cross-checks: two commands, one word, one window.
    ///
    /// This is the same claim `windmcp`'s `--day` tests make, pinned to the same two instants, and
    /// the reason it is written out twice rather than derived is that the bug was never the
    /// arithmetic — `clock::day_bounds` had always been right — it was one command not calling it.
    #[test]
    fn query_and_day_resolve_the_same_day_to_the_same_window() {
        let root = library("day-parity", &sample(), false);
        let searched = query_report(&QueryArgs { root: Some(root.clone()), ..query("", "2026-09-22") }).unwrap();
        let day = day_report(&DayArgs { root: Some(root.clone()), day: "2026-09-22".into(), detail: 4 }).unwrap();
        let (from, to) = wind_base::clock::day_bounds(2026, 9, 22, 180);
        let window = format!("{} .. {}", LocalParts::from_naive_epoch(from).display(), LocalParts::from_naive_epoch(to).display());
        assert_eq!(window, "2026-09-22 03:00:00 .. 2026-09-23 02:59:59", "the shared helper moved");
        assert!(searched.contains(&format!("window {window} (day, day_begin 03:00)")), "query hid its window:\n{searched}");
        assert!(day.contains(&labelled("window", &window)), "day hid its window:\n{day}");
        assert!(day.contains(&labelled("day", "2026-09-22 (day, day_begin 03:00)")), "{day}");
        // And the same window reads the same rows: the 01:00 frame is the 21st's in both.
        assert!(searched.contains("4 of 4 hits"), "{searched}");
        assert!(day.contains(&labelled("rows", "4")), "{day}");
        assert!(day.contains(&labelled("first", "2026-09-22 10:00:00")), "{day}");
        assert!(day.contains(&labelled("last", "2026-09-22 19:30:30")), "{day}");

        let previous = query_report(&QueryArgs { root: Some(root.clone()), ..query("", "2026-09-21") }).unwrap();
        let earlier = day_report(&DayArgs { root: Some(root.clone()), day: "2026-09-21".into(), detail: 4 }).unwrap();
        assert!(previous.contains("2 of 2 hits"), "{previous}");
        assert!(earlier.contains(&labelled("rows", "2")), "{earlier}");
        assert!(earlier.contains(&labelled("last", "2026-09-22 01:00:00")), "the small hours are the 21st's work:\n{earlier}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// The compatibility case for every install that never touched the setting: a day beginning at
    /// midnight is the calendar day, exactly, with nothing shifted and nothing re-labelled.
    #[test]
    fn an_install_that_starts_its_day_at_midnight_sees_the_calendar_day_it_saw_before() {
        let root = library_configured("midnight", &sample(), r#"{"day_begin_minutes": 0}"#);
        let day = day_report(&DayArgs { root: Some(root.clone()), day: "2026-09-22".into(), detail: 4 }).unwrap();
        assert!(day.contains(&labelled("window", "2026-09-22 00:00:00 .. 2026-09-22 23:59:59")), "{day}");
        assert!(day.contains(&labelled("day", "2026-09-22 (day, day_begin 00:00)")), "{day}");
        // Five frames, not four: the 01:00 row is the 22nd's own once nothing shifts it back.
        assert!(day.contains(&labelled("rows", "5")), "{day}");
        assert!(day.contains(&labelled("first", "2026-09-22 01:00:00")), "{day}");
        let searched = query_report(&QueryArgs { root: Some(root.clone()), ..query("", "2026-09-22") }).unwrap();
        assert!(searched.contains("window 2026-09-22 00:00:00 .. 2026-09-22 23:59:59 (day, day_begin 00:00)"), "{searched}");
        assert!(searched.contains("5 of 5 hits"), "{searched}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn inspect_scores_each_row_against_the_one_before_it() {
        let root = library("inspect", &sample(), false);
        // The tail segment is the one holding two identical screens 30 seconds apart, and its name
        // routes the lookup to the right month file without scanning the library.
        let report = inspect_report(&InspectArgs { root: Some(root.clone()), segment: SEG_TAIL.into() }).unwrap();
        assert!(report.contains(&labelled("rows", "2")), "{report}");
        assert!(report.contains("first"), "the oldest row has nothing to compare against:\n{report}");
        assert!(report.contains("1.000  DROP"), "an identical body is where the dedup line cuts:\n{report}");
        assert!(report.contains(&labelled("would drop", "1 of 2 rows")), "{report}");

        // A segment whose rows are all different still prints, with nothing dropped.
        let kept = inspect_report(&InspectArgs { root: Some(root.clone()), segment: SEG_MAIN.into() }).unwrap();
        assert!(kept.contains(&labelled("rows", "2")), "{kept}");
        assert!(kept.contains("keep"), "{kept}");
        assert!(kept.contains(&labelled("would drop", "0 of 2 rows")), "{kept}");

        let absent = inspect_report(&InspectArgs { root: Some(root.clone()), segment: "2026-09-25_10-00-00.mp4".into() });
        assert!(absent.unwrap_err().contains("no rows for segment"), "a typo must not read as an empty segment");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// The one command allowed to write, run against a temp fixture rather than a user's library.
    #[test]
    fn index_measures_both_states_and_leaves_the_index_in_place() {
        let base = stamp("2026-09-01_00-00-00");
        let rows: Vec<Record> = (0..20_000)
            .map(|i| record("2026-09-01_00-00-00.mp4", base + i * 60, &format!("screen {i} 文件 ChatGPT token"), Some("ChatGPT")))
            .collect();
        let root = library("index", &rows, false);

        let report = index_report(&IndexArgs { root: Some(root.clone()), status_only: false }).unwrap();
        println!("index report over 20000 rows:\n{report}");
        assert!(report.contains("20000 rows, index absent"), "{report}");
        assert!(report.contains("no index") && report.contains("with index"), "{report}");
        assert!(report.contains("ratio"), "{report}");
        assert!(report.contains("1 created, 0 already present"), "{report}");
        assert!(!report.contains("not measurable"), "both states must produce real numbers:\n{report}");
        for key in ["no index", "with index"] {
            let line = report.lines().find(|l| l.trim_start().starts_with(key)).expect(key);
            let mean: f64 = line
                .split("mean")
                .nth(1)
                .and_then(|rest| rest.split(" ms").next())
                .and_then(|num| num.trim().parse().ok())
                .unwrap_or_else(|| panic!("no mean in {line}"));
            assert!(mean > 0.0 && mean.is_finite(), "{line}");
        }

        // The file itself, not the report, is the evidence.
        let month = Month::from_path(&root.join("userdata/db/default_2026-09_wind.db")).expect("month");
        let conn = month.open_write().expect("reopen");
        assert!(index_present(&conn).expect("probe"), "the index must survive the reporter");
        drop(conn);
        drop(month);

        let after = index_report(&IndexArgs { root: Some(root.clone()), status_only: true }).unwrap();
        assert!(after.contains("index present"), "{after}");
        assert!(after.contains("1/1 month files carry the index"), "{after}");
        assert!(after.contains("no index file was modified"), "{after}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn bench_search_times_the_whole_battery() {
        let root = library("battery", &sample(), true);
        let report = bench_search_report(&BenchSearchArgs { root: Some(root.clone()), iterations: 3 }).unwrap();
        let rows: Vec<&str> = report.lines().filter(|l| BATTERY.iter().any(|p| l.starts_with(p.label))).collect();
        assert_eq!(rows.len(), BATTERY.len(), "one table row per probe:\n{report}");
        for line in &rows {
            assert!(line.contains(" ms"), "{line}");
            assert!(!line.contains("inf"), "{line}");
        }
        assert!(report.contains("glyph table 2 characters"), "{report}");
        assert!(report.contains("whole month, one token"), "{report}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_library_with_no_files_fails_with_a_path_not_a_panic() {
        let dir = root("empty");
        let error = query_report(&QueryArgs { root: Some(dir.clone()), ..query("", "2026-09-22") }).unwrap_err();
        assert!(error.contains("has no index directory"), "{error}");
        // An empty but existing directory is a different answer, and it must name the directory too.
        std::fs::create_dir_all(dir.join("userdata/db")).unwrap();
        let error = index_report(&IndexArgs { root: Some(dir.clone()), status_only: true }).unwrap_err();
        assert!(error.contains("no month files under"), "{error}");
        let error = stats_report(&StatsArgs { root: Some(dir.clone()), month: None }).unwrap();
        assert!(error.contains("no month files"), "{error}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- the pure pieces -----------------------------------------------------------------------

    #[test]
    fn a_segment_name_matches_its_own_rows_and_their_pipeline_suffixes() {
        assert!(matches_segment("2026-09-22_10-00-00.mp4", "2026-09-22_10-00-00.mp4"));
        assert!(matches_segment("2026-09-22_10-00-00.mp4", "2026-09-22_10-00-00-INDEX.mp4"));
        assert!(matches_segment("2026-09-22_10-00-00", "2026-09-22_10-00-00-SCREENSHOTS-OCRED.mp4"));
        assert!(!matches_segment("2026-09-22_10-00-00.mp4", "2026-09-22_11-00-00.mp4"));
        assert!(!matches_segment("a", "anything at all"), "an unstamped query must not match everything");
        assert!(!matches_segment("2026-09-22_10-00", "2026-09-22_10-00-00.mp4"), "too short to be a stamp");
    }

    #[test]
    fn the_footer_says_what_it_measured() {
        assert_eq!(query_footer(2, 41, 3, 12.5), "2 of 41 hits across 3 month files in 12.500 ms (4.167 ms/query)\n");
        assert_eq!(query_footer(0, 0, 0, 0.0), "0 of 0 hits across 0 month files in 0.000 ms (0.000 ms/query)\n");
    }

    #[test]
    fn the_ratio_keeps_its_sign() {
        let mut slow = Timing::new(2);
        slow.observe(10.0, 5);
        slow.observe(10.0, 5);
        let mut fast = Timing::new(2);
        fast.observe(2.0, 5);
        fast.observe(2.0, 5);
        assert_eq!(ratio_line(&slow, &fast), "5.00x faster with the index");
        assert_eq!(ratio_line(&fast, &slow), "5.00x slower with the index at 5 rows");
        assert_eq!(ratio_line(&Timing::new(0), &fast), "not measurable at this size");
        assert!(timing_line(&slow).contains("best"), "{}", timing_line(&slow));
    }

    #[test]
    fn an_empty_exclude_is_not_a_search_term() {
        let span = Span { from: 10, to: 20, origin: Origin::Explicit, day_begin_minutes: 0 };
        let (sql, empty_binds) = build_query(&span, "x", "", None, 20, 1).where_clause();
        assert!(!sql.contains("NOT LIKE"), "{sql}");
        assert_eq!(empty_binds.len(), 4, "one token, two patterns, two bounds");
        let (sql, _) = build_query(&span, "x", "secret", None, 20, 1).where_clause();
        assert!(sql.contains("ocr_text NOT LIKE ?"), "{sql}");
        // Paging is applied, and page 1 is the first page rather than a skipped one.
        let query = build_query(&span, "", "", None, 20, 1);
        assert_eq!((query.limit, query.offset), (Some(20), 0));
        // Expansion adds one (body OR title) pattern pair per generated variant, and the variant the
        // user did not type has to be among the bound values.
        let (expanded, expanded_binds) = build_query(&span, "文件", "", Some(SimilarChars::parse("文，又\n")), 20, 1).where_clause();
        let (plain, plain_binds) = build_query(&span, "文件", "", None, 20, 1).where_clause();
        assert_eq!(plain_binds.len(), 4, "one token, two patterns, two bounds: {plain_binds:?}");
        assert_eq!(expanded_binds.len(), 6, "two variants, two patterns each, plus the bounds");
        assert!(expanded_binds.iter().any(|b| format!("{b:?}").contains("又件")), "{expanded_binds:?}");
        assert!(!plain_binds.iter().any(|b| format!("{b:?}").contains("又件")), "{plain_binds:?}");
        assert_ne!(expanded, plain);
    }

    #[test]
    fn windows_are_labelled_in_the_bench_table() {
        assert_eq!(Window::Hour.label(), "hour");
        assert_eq!(Window::Month.label(), "month");
        assert_eq!(endpoint(Some(stamp("2026-09-22_10-00-00"))), "2026-09-22 10:00:00");
        assert_eq!(endpoint(None), "<none>");
    }

    #[test]
    fn a_library_span_is_measured_from_its_own_bounds() {
        let facts = vec![MonthFacts {
            month: Month { user: "default".into(), year: 2026, month: 9, path: PathBuf::new() },
            rows: 2,
            segments: 1,
            bounds: Some((stamp("2026-09-01_00-00-00"), stamp("2026-09-03_00-00-00"))),
        }];
        assert_eq!(span_days(&facts), 3);
        assert_eq!(span_days(&[]), 0);
    }
}
