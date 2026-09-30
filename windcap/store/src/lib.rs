//! `wind-store` — the monthly SQLite index as a contract.
//!
//! Everything the product knows about the user's history lives in these files, and every other
//! native binary reads or writes them through this crate. Two properties matter more than tidiness:
//!
//!   * **byte compatibility** — the recorder writes, the UI reads, and the Python app the user may
//!     still be running does both. The schema, the column order, the `" -||- "` title convention and
//!     the naive-local epoch in `videofile_time` are all wire format. See `schema` and
//!     `wind_base::clock`.
//!   * **non-blocking reads** — a query never touches a live file (`read::temp_read_for`), because a
//!     long search that holds a shared lock is how a recording segment gets lost.

pub mod aggregate;
pub mod maintain;
pub mod read;
pub mod schema;
pub mod search;
pub mod similar;
pub mod write;

pub use aggregate::{DayOverview, DayStat, TitleInterval, Bucket};
pub use maintain::{MaintenancePlan, MaintenanceReport};
pub use read::{count_rows, discover, months_in_range, rows_in_window, time_bounds, Month, Row};
pub use schema::{column_names, ensure_schema, StoreError, COLUMNS, DDL, TITLE_SEPARATOR};
pub use search::{count, search, search_months, Query, SearchResult};
pub use similar::SimilarChars;
pub use write::{Record, Store};
