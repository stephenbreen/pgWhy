//! Query execution core (U18): statement splitter, executor, cancel, transaction state; result
//! buffers (U12); edit SQL (U13).
//!
//! Consumers (editor tabs, grids, run-against, the workflow's post-restore step) hold a
//! [`QuerySession`] over a dedicated connection and never touch tokio-postgres directly.

mod cancel;
pub mod edit;
mod executor;
pub mod results;
pub mod splitter;
pub mod topn;
pub mod txn;

pub use cancel::CancelHandle;
pub use edit::{
    commit_edits, plan_edits, CommitOptions, CommitReport, EditColumn, EditKind, EditPlan,
    EditStatement, EditTarget, KeyValue, RowMismatch,
};
pub use executor::{
    char_position_to_byte, CollectSink, CollectedEvent, CollectedStatement, ColumnMeta,
    ErrorPolicy, QuerySession, ResultSink, RowBatch, RowRef, RunOptions, RunOutcome, RunSummary,
    ScriptMode, ScriptReport, SqlError, StatementDone,
};
pub use results::{
    BufferEvent, BufferOptions, BufferSink, CsvOptions, EvictedResult, EvictionReport,
    ExportProgress, ExportReport, PendingChange, PendingCounts, PendingHold, RegistryOptions,
    ResultInfo, ResultRegistry, ResultSet, ResultState, DEFAULT_BYTE_CAP, DEFAULT_ROW_CAP,
    MAX_WINDOW_ROWS, ROW_CLEAN, ROW_DELETED, ROW_INSERTED, ROW_UPDATED,
};
pub use splitter::{
    analyze, analyze_with, classify, split, split_range, split_range_with, split_with,
    statement_at, ScriptAnalysis, StatementInfo, StatementRange, TxnControl, Unterminated,
};
pub use topn::{
    build_topn, run_topn, safe_type_name, text_expr, CriteriaOp, Criterion, SortKey, TopNColumn,
    TopNOutcome, TopNOutputColumn, TopNQuery, TopNRequest, DEFAULT_TOP_N,
};
pub use txn::{SessionState, TxnStatus};
