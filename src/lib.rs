//! HIIS: a segment-based full-text search storage engine.
//!
//! Text goes through [`analysis`] into positioned terms, lands in an
//! in-memory builder, and is flushed as an immutable [`index::segment`]
//! holding compressed posting lists ([`codec`]). Background merges keep the
//! segment count down; [`search`] ranks with BM25.
//!
//! ```no_run
//! use hiis::{Config, Engine};
//!
//! let engine = Engine::open("./idx", Config::default())?;
//! engine.add_document("the quick brown fox")?;
//! engine.commit()?;
//! for hit in engine.search("\"quick brown\" -dog", 10)? {
//!     println!("{} {:.3}", hit.doc_id, hit.score);
//! }
//! # Ok::<(), hiis::Error>(())
//! ```

pub mod analysis;
pub mod codec;
pub mod engine;
pub mod error;
pub mod index;
pub mod search;
mod wal;

pub use engine::{Config, Engine, Stats};
pub use error::{Error, Result};
pub use index::DocId;
pub use search::Hit;
