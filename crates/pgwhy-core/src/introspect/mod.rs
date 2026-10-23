//! Introspection (U8): batched pg_catalog pulls, per-database catalog cache, expansion state.
//!
//! - [`Introspector`] — the service the tree, details panel, completion, Script-as and the
//!   grid's edit layer are built against. One round trip per schema expansion; details are
//!   four pipelined statements on select; everything cached per (connection, database).
//! - [`model`] — serde types for all of the above, including [`EditProvenance`].
//! - [`quote`] — the one identifier-quoting helper every SQL generator must use.
//! - [`tree_state`] — persisted expansion paths (`tree_state.json`).

mod cache;
pub mod model;
mod queries;
pub mod quote;
pub mod script_as;
pub mod tree_state;

pub use cache::Introspector;
pub use model::*;
pub use quote::{dump_pattern, escape_like, qualified, quote_ident, quote_ident_always};
pub use script_as::{script_as, unavailable_reason, ScriptKind};
pub use tree_state::{ExpansionStore, NodePath, TREE_STATE_FILE};
