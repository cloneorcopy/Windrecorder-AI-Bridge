//! `wind-mcp` — Windrecorder's MCP bridge, as a library and a binary.
//!
//! The fork's namesake feature, rebuilt native. It exposes the user's screen history to an AI
//! assistant over a persistent, token-authenticated streamable-HTTP service, reading the same monthly
//! SQLite index the recorder writes. Nine of its eleven tools only read. Two write, and only into the
//! two AI summary directories this feature owns: an outside assistant can file a stretch's summary and
//! a day's summary there, which is the same text this machine's own `windai summarize` files. Nothing
//! in this crate can write an index row, and `destructiveHint` is false on both writers because both
//! are idempotent upserts of one paragraph.
//!
//! ```text
//!   main.rs     argv, one reporter per subcommand, the only `println!`
//!   jsonrpc.rs  the MCP method table              ┐ transport, no idea what a row is
//!   http.rs     HTTP/1.1 framing                  ┘
//!   server.rs   sessions, the bearer gate, the accept loop
//!   tools.rs    the six history tools, as pure functions   ┐
//!   summaries.rs the queue, the two readers and the writers ├ the product
//!   stream.rs   one titled stream, one gap rule            │
//!   title.rs    the shared window-title normaliser         ┘
//!   runtime.rs  config and paths                   ┐
//!   library.rs  the only files-and-database access ┤ the outside world
//!   axis.rs     which axis a timestamp is on       ┘
//! ```
//!
//! The split at the bottom of that list is the one the build is judged on: nothing below
//! `library.rs`'s public surface reaches SQLite, and `library.rs` itself issues no statement. A
//! bridge that could write a `SELECT` is a bridge that will eventually write a `WHERE` nobody
//! reviewed, and the query belongs in `wind-store` where its tests are. The summaries go through
//! `wind-summary`, which owns those files and has no rusqlite dependency to make an index write
//! possible even by accident.
//!
//! Three decisions are load-bearing and are not to be "improved" without reading this paragraph
//! again. The service is **off** by default and **loopback** by default; auth may be switched off
//! only on a loopback bind; and the bearer token lives only in `userdata/config_user.json`, never
//! in argv or the environment. The bridge was stdio-only before it was this, and converting it back
//! would be the regression — a per-client spawned process is exactly what the resident service
//! replaced so that one recorder can serve several assistants.

pub mod args;
pub mod auth;
pub mod fixture;
pub mod axis;
pub mod http;
pub mod jsonrpc;
pub mod library;
pub mod runtime;
pub mod server;
pub mod stream;
pub mod summaries;
pub mod title;
pub mod tools;

pub use axis::Axis;
pub use runtime::Runtime;
pub use tools::NAMES as TOOL_NAMES;
