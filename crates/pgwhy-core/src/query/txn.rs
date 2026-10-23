//! Transaction-state tracking for one connection (R19).
//!
//! tokio-postgres swallows the `ReadyForQuery` status byte (libpq's `PQtransactionStatus`), so
//! the state is *derived*: every statement on the connection goes through the executor, which
//! feeds [`TxnTracker::after_statement`] with the statement's [`TxnControl`] class and its
//! outcome. The rules mirror the server's:
//!
//! - success: `BEGIN` → in-transaction; `COMMIT`/`ROLLBACK`/`PREPARE TRANSACTION` → idle (the
//!   `AND CHAIN` forms → in-transaction); `ROLLBACK TO SAVEPOINT` → in-transaction (it is the
//!   way out of a failed state); everything else leaves the state alone;
//! - error: idle stays idle (autocommit); in-transaction → failed (the server aborted the
//!   block — this includes a cancelled statement); failed stays failed (`25P02`).
//!
//! Where the outcome is genuinely ambiguous — a transaction-control statement that itself
//! failed (a deferred-constraint `COMMIT` failure rolls the transaction back; `COMMIT foo` is a
//! syntax error that aborts it) — the tracker marks itself uncertain and the executor resyncs
//! with [`PROBE_SQL`]: `now()` is the transaction start and `statement_timestamp()` the current
//! statement's start, equal only when the probe runs in its own implicit transaction (idle),
//! different inside an explicit block (in-transaction), and the probe fails with `25P02` in an
//! aborted block (failed). It has no side effects in any state.

use super::splitter::TxnControl;

/// Server-side transaction status of a connection (the `ReadyForQuery` I/T/E byte).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TxnStatus {
    #[default]
    Idle,
    InTransaction,
    /// An error occurred inside the block; only `ROLLBACK` (or `ROLLBACK TO SAVEPOINT`) helps.
    Failed,
}

/// What a session looks like to its owner (editor tab, grid, workflow step).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Running,
    InTransaction,
    FailedTransaction,
    /// The connection is gone; any open transaction was rolled back by the server.
    Disconnected,
}

impl SessionState {
    pub(crate) fn from_txn(t: TxnStatus) -> Self {
        match t {
            TxnStatus::Idle => SessionState::Idle,
            TxnStatus::InTransaction => SessionState::InTransaction,
            TxnStatus::Failed => SessionState::FailedTransaction,
        }
    }
}

/// Side-effect-free status probe; see the module docs.
pub const PROBE_SQL: &str = "select now() = statement_timestamp()";

/// SQLSTATE `25P02`: "current transaction is aborted, commands ignored until end of block".
pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";

/// How a statement ended, as far as transaction state is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementOutcome {
    Ok,
    /// Any server error, including cancellation (`57014`).
    Error,
}

/// The derivation state machine. Pure; unit-tested below.
#[derive(Debug, Clone, Default)]
pub struct TxnTracker {
    status: TxnStatus,
    uncertain: bool,
}

impl TxnTracker {
    pub fn new(status: TxnStatus) -> Self {
        Self {
            status,
            uncertain: false,
        }
    }

    pub fn status(&self) -> TxnStatus {
        self.status
    }

    /// True when the last transition could not be derived and a probe should confirm it.
    pub fn uncertain(&self) -> bool {
        self.uncertain
    }

    /// Record the probe's (or any authoritative) answer.
    pub fn set(&mut self, status: TxnStatus) {
        self.status = status;
        self.uncertain = false;
    }

    pub fn after_statement(&mut self, ctl: Option<TxnControl>, outcome: StatementOutcome) {
        use StatementOutcome as O;
        use TxnControl as C;
        use TxnStatus as S;
        let (next, uncertain) = match (outcome, ctl) {
            (O::Ok, Some(C::Begin)) => (S::InTransaction, false),
            (O::Ok, Some(C::Commit { chain: true } | C::Rollback { chain: true })) => {
                (S::InTransaction, false)
            }
            (O::Ok, Some(C::Commit { .. } | C::Rollback { .. } | C::PrepareTransaction)) => {
                (S::Idle, false)
            }
            // ROLLBACK TO only succeeds inside a block
            (O::Ok, Some(C::RollbackToSavepoint)) => (S::InTransaction, false),
            (O::Ok, _) => (self.status, false),
            (O::Error, ctl) => match (self.status, ctl) {
                (S::Idle, _) => (S::Idle, false),
                (
                    S::InTransaction | S::Failed,
                    Some(C::Commit { .. } | C::Rollback { .. } | C::PrepareTransaction),
                ) => (S::Failed, true),
                (S::InTransaction | S::Failed, _) => (S::Failed, false),
            },
        };
        self.status = next;
        self.uncertain = uncertain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementOutcome::{Error, Ok};
    use TxnControl as C;

    fn run(steps: &[(Option<TxnControl>, StatementOutcome)]) -> TxnTracker {
        let mut t = TxnTracker::default();
        for (c, o) in steps {
            t.after_statement(*c, *o);
        }
        t
    }

    #[test]
    fn autocommit_errors_stay_idle() {
        let t = run(&[(None, Ok), (None, Error), (None, Ok)]);
        assert_eq!(t.status(), TxnStatus::Idle);
        assert!(!t.uncertain());
    }

    #[test]
    fn begin_error_rollback_cycle() {
        let t = run(&[(Some(C::Begin), Ok), (None, Ok)]);
        assert_eq!(t.status(), TxnStatus::InTransaction);
        let t = run(&[(Some(C::Begin), Ok), (None, Error), (None, Error)]);
        assert_eq!(t.status(), TxnStatus::Failed);
        let t = run(&[
            (Some(C::Begin), Ok),
            (None, Error),
            (Some(C::Rollback { chain: false }), Ok),
        ]);
        assert_eq!(t.status(), TxnStatus::Idle);
        // COMMIT of a failed block succeeds (server answers ROLLBACK)
        let t = run(&[
            (Some(C::Begin), Ok),
            (None, Error),
            (Some(C::Commit { chain: false }), Ok),
        ]);
        assert_eq!(t.status(), TxnStatus::Idle);
    }

    #[test]
    fn savepoints_recover_failed_blocks() {
        let t = run(&[
            (Some(C::Begin), Ok),
            (Some(C::Savepoint), Ok),
            (None, Error),
            (Some(C::RollbackToSavepoint), Ok),
        ]);
        assert_eq!(t.status(), TxnStatus::InTransaction);
    }

    #[test]
    fn chain_and_prepare() {
        let t = run(&[(Some(C::Begin), Ok), (Some(C::Commit { chain: true }), Ok)]);
        assert_eq!(t.status(), TxnStatus::InTransaction);
        let t = run(&[(Some(C::Begin), Ok), (Some(C::PrepareTransaction), Ok)]);
        assert_eq!(t.status(), TxnStatus::Idle);
    }

    #[test]
    fn failed_commit_is_uncertain() {
        let t = run(&[
            (Some(C::Begin), Ok),
            (Some(C::Commit { chain: false }), Error),
        ]);
        assert!(t.uncertain());
    }
}
