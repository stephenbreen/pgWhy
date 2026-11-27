//! Workflows v2 unified pipeline model: entry modes, typed runtime parameters, five WYSIWYG
//! step blocks, and starter templates.
//!
//! Every visual block compiles deterministically to PostgreSQL SQL via [`crate::compiler`], so
//! users can inspect the exact SQL or convert any visual block to a manual SQL step.

use crate::definition::{DbRef, RefreshOptions, RefreshWorkflow, WorkflowId};
use pgwhy_core::config::ConnectionId;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Current workflow TOML schema version (v1 files without `schema_version` auto-load as v1
/// `from_remote_db` pipelines).
pub const WORKFLOW_SCHEMA_VERSION: u32 = 2;

fn default_true() -> bool {
    true
}

fn default_test_domain() -> String {
    "example.test".to_string()
}

/// How a v2 workflow acquires its working database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum WorkflowSource {
    /// Dump a remote/source database via `pg_dump` and restore into staging (the v1 Refresh flow).
    FromRemoteDb { source: DbRef },
    /// Restore from a backup archive on disk (Custom `.dump`, Directory, or Plain `.sql`).
    FromBackupFile { archive: BackupSource },
    /// Run transform and verify steps directly against an existing database inside a transaction
    /// (with optional pre-run backup and Dry Run rollback).
    InPlaceTarget {
        /// Take a safety backup before mutating the target database.
        #[serde(default)]
        pre_run_backup: bool,
    },
}

/// Where a `from_backup_file` workflow gets its archive path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackupSource {
    /// A fixed archive path on disk.
    FixedPath { path: PathBuf },
    /// Prompt the user to pick an archive at run time (optionally bound to a `file_path` param).
    PromptAtRun {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        param: Option<String>,
    },
    /// Automatically pick the newest reusable backup of `<connection>/<database>` from History.
    LatestFromHistory {
        connection: ConnectionId,
        database: String,
    },
}

/// Declared type of a workflow runtime parameter (`{{param_name}}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    Text,
    Number,
    FilePath,
    /// Masked in UI, held in memory for the run only, never persisted to TOML or run history.
    Secret,
}

/// A typed runtime parameter referenced as `{{name}}` in visual step values or SQL steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowParam {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub kind: ParamType,
    #[serde(default = "default_true")]
    pub required: bool,
    /// Default value — must always be `None` for [`ParamType::Secret`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl WorkflowParam {
    /// Validate parameter identifier syntax (`^[A-Za-z_][A-Za-z0-9_]{0,63}$`) and secret rules.
    pub fn validate(&self) -> AppResult<()> {
        if !is_valid_param_name(&self.name) {
            return Err(AppError::new(
                ErrorKind::Config,
                format!(
                    "invalid parameter name \"{}\": use letters, digits, and underscores starting with a letter or underscore",
                    self.name
                ),
            ));
        }
        if self.kind == ParamType::Secret && self.default.as_deref().is_some_and(|d| !d.is_empty())
        {
            return Err(AppError::new(
                ErrorKind::Config,
                format!(
                    "secret parameter \"{}\" cannot store a default value in the workflow file",
                    self.name
                ),
            )
            .with_hint(
                "secret parameters are always entered at run time and never saved to disk",
            ));
        }
        if self.kind == ParamType::Number {
            if let Some(d) = self.default.as_deref().filter(|s| !s.is_empty()) {
                if !is_valid_number_literal(d) {
                    return Err(AppError::new(
                        ErrorKind::Config,
                        format!(
                            "default value \"{d}\" for number parameter \"{}\" is not a valid number",
                            self.name
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn is_valid_param_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Strict SQL numeric literal validator (`-?digits(.digits)?(e[+-]?digits)?`).
pub fn is_valid_number_literal(raw: &str) -> bool {
    let s = raw.trim();
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    if bytes[i] == b'-' || bytes[i] == b'+' {
        i += 1;
        if i >= bytes.len() {
            return false;
        }
    }
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let has_int = i > int_start;
    let mut has_frac = false;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        has_frac = i > frac_start;
        if !has_frac && !has_int {
            return false;
        }
    }
    if !has_int && !has_frac {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == bytes.len()
}

/// How multiple WHERE predicates are combined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalOp {
    #[default]
    And,
    Or,
}

/// Predicate comparison operators for visual WHERE builders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    Like,
    #[serde(rename = "ilike")]
    ILike,
    NotLike,
    #[serde(rename = "not_ilike")]
    NotILike,
    In,
    NotIn,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    pub fn is_unary(self) -> bool {
        matches!(self, FilterOp::IsNull | FilterOp::IsNotNull)
    }

    pub fn is_list(self) -> bool {
        matches!(self, FilterOp::In | FilterOp::NotIn)
    }
}

/// Comparison operator for row-count verify assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl CompareOp {
    pub fn eval(self, actual: i64, expected: i64) -> bool {
        match self {
            CompareOp::Eq => actual == expected,
            CompareOp::Neq => actual != expected,
            CompareOp::Lt => actual < expected,
            CompareOp::Lte => actual <= expected,
            CompareOp::Gt => actual > expected,
            CompareOp::Gte => actual >= expected,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Neq => "!=",
            CompareOp::Lt => "<",
            CompareOp::Lte => "<=",
            CompareOp::Gt => ">",
            CompareOp::Gte => ">=",
        }
    }
}

/// A value cell in visual `insert_rows`, `update_mask`, or `WHERE` predicates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellValue {
    Literal {
        value: String,
    },
    Number {
        value: String,
    },
    Bool {
        value: bool,
    },
    Null,
    Default,
    /// Reference to a declared workflow parameter `{{name}}`.
    Param {
        name: String,
    },
    /// Raw PostgreSQL expression, e.g. `gen_random_uuid()`, `now()`, `crypt({{test_password}}, gen_salt('bf'))`.
    Expr {
        sql: String,
    },
    /// Value list for `IN (...)` / `NOT IN (...)` predicates.
    List {
        items: Vec<CellValue>,
    },
}

/// One visual `WHERE` filter condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predicate {
    pub column: String,
    pub op: FilterOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<CellValue>,
}

/// Mode of a `delete_rows` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DeleteMode {
    DeleteWhere {
        #[serde(default)]
        conditions: Vec<Predicate>,
        #[serde(default)]
        combinator: LogicalOp,
    },
    Truncate {
        #[serde(default)]
        restart_identity: bool,
        #[serde(default)]
        cascade: bool,
    },
}

/// Conflict handling for an `insert_rows` step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Error,
    DoNothing {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        columns: Vec<String>,
    },
    DoUpdate {
        columns: Vec<String>,
        /// Columns to overwrite (`col = EXCLUDED.col`); empty = every inserted non-conflict column.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        update_columns: Vec<String>,
    },
}

/// Per-column anonymization or update transform in an `update_mask` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MaskTransform {
    /// Generates a unique synthetic email (`user_<id>@<domain>` or `user_<md5(ctid)>@<domain>`).
    SyntheticEmail {
        #[serde(default = "default_test_domain")]
        domain: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id_column: Option<String>,
    },
    FixedValue {
        value: CellValue,
    },
    SetNull,
    /// Built-in PostgreSQL `encode(sha256(convert_to(COALESCE(col::text, '') || salt, 'UTF8')), 'hex')`
    /// (requires no extensions on PostgreSQL 11+).
    Sha256Hash {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        salt: Option<CellValue>,
    },
    CustomExpr {
        sql: String,
    },
}

/// One column assignment in an `update_mask` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaskAssignment {
    pub column: String,
    pub transform: MaskTransform,
}

/// Body of a `sql` step: either inline SQL or a linked Script Library entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SqlStepBody {
    Inline { sql: String },
    Library { script_id: String },
}

/// What a custom SQL verify assertion checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomVerifyExpect {
    /// Query executes without error (matches v1 `sanity_query`).
    #[default]
    Succeeds,
    /// Query returns a single row whose first column is boolean `true`.
    TruthyScalar,
    /// Query returns zero rows.
    ZeroRows,
}

/// Structured assertion for a `verify` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyAssertion {
    RowCount {
        schema: String,
        table: String,
        op: CompareOp,
        expected: i64,
        #[serde(default)]
        conditions: Vec<Predicate>,
        #[serde(default)]
        combinator: LogicalOp,
    },
    /// Passes iff zero rows match (e.g. "no real production emails remain").
    ZeroRows {
        schema: String,
        table: String,
        #[serde(default)]
        conditions: Vec<Predicate>,
        #[serde(default)]
        combinator: LogicalOp,
    },
    CustomSql {
        sql: String,
        #[serde(default)]
        expect: CustomVerifyExpect,
    },
}

/// The five WYSIWYG pipeline step blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    DeleteRows {
        schema: String,
        table: String,
        #[serde(flatten)]
        mode: DeleteMode,
    },
    InsertRows {
        schema: String,
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<CellValue>>,
        #[serde(default)]
        on_conflict: ConflictPolicy,
    },
    UpdateMask {
        schema: String,
        table: String,
        #[serde(default)]
        conditions: Vec<Predicate>,
        #[serde(default)]
        combinator: LogicalOp,
        assignments: Vec<MaskAssignment>,
    },
    Sql {
        #[serde(flatten)]
        body: SqlStepBody,
    },
    Verify {
        assertion: VerifyAssertion,
    },
}

/// One step in a Workflows v2 pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Opt out of the single-transaction wrapper (e.g. `VACUUM`, `CREATE INDEX CONCURRENTLY`).
    #[serde(default)]
    pub no_transaction: bool,
    #[serde(flatten)]
    pub kind: StepKind,
}

impl WorkflowStep {
    pub fn is_verify(&self) -> bool {
        matches!(self.kind, StepKind::Verify { .. })
    }
}

/// A built-in starter workflow template offered in the Workflow Editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub workflow: RefreshWorkflow,
}

/// Built-in starter templates shipped with pgWhy.
pub fn starter_templates() -> Vec<WorkflowTemplate> {
    let empty_ref = || DbRef {
        connection: ConnectionId(String::new()),
        database: String::new(),
    };

    // 1. Restore backup, delete users & seed test users
    let restore_and_seed = RefreshWorkflow {
        schema_version: WORKFLOW_SCHEMA_VERSION,
        id: WorkflowId::new(),
        name: "Restore backup & seed test users".into(),
        source: empty_ref(),
        source_mode: Some(WorkflowSource::FromBackupFile {
            archive: BackupSource::PromptAtRun { param: None },
        }),
        target: empty_ref(),
        post_restore_script: None,
        parameters: vec![WorkflowParam {
            name: "test_password".into(),
            label: Some("Test user password".into()),
            description: Some(
                "Password assigned to seeded test accounts (never saved to disk)".into(),
            ),
            kind: ParamType::Secret,
            required: true,
            default: None,
        }],
        steps: vec![
            WorkflowStep {
                id: "step_delete_users".into(),
                name: "Delete existing users".into(),
                enabled: true,
                no_transaction: false,
                kind: StepKind::DeleteRows {
                    schema: "public".into(),
                    table: "users".into(),
                    mode: DeleteMode::DeleteWhere {
                        conditions: Vec::new(),
                        combinator: LogicalOp::And,
                    },
                },
            },
            WorkflowStep {
                id: "step_seed_users".into(),
                name: "Add test users".into(),
                enabled: true,
                no_transaction: false,
                kind: StepKind::InsertRows {
                    schema: "public".into(),
                    table: "users".into(),
                    columns: vec!["email".into(), "role".into(), "password_hash".into()],
                    rows: vec![
                        vec![
                            CellValue::Literal {
                                value: "admin@example.test".into(),
                            },
                            CellValue::Literal {
                                value: "admin".into(),
                            },
                            CellValue::Expr {
                                sql: "encode(sha256(convert_to({{test_password}}, 'UTF8')), 'hex')"
                                    .into(),
                            },
                        ],
                        vec![
                            CellValue::Literal {
                                value: "user@example.test".into(),
                            },
                            CellValue::Literal {
                                value: "user".into(),
                            },
                            CellValue::Expr {
                                sql: "encode(sha256(convert_to({{test_password}}, 'UTF8')), 'hex')"
                                    .into(),
                            },
                        ],
                    ],
                    on_conflict: ConflictPolicy::DoNothing {
                        columns: Vec::new(),
                    },
                },
            },
            WorkflowStep {
                id: "step_verify_users".into(),
                name: "Verify test users exist".into(),
                enabled: true,
                no_transaction: false,
                kind: StepKind::Verify {
                    assertion: VerifyAssertion::RowCount {
                        schema: "public".into(),
                        table: "users".into(),
                        op: CompareOp::Eq,
                        expected: 2,
                        conditions: Vec::new(),
                        combinator: LogicalOp::And,
                    },
                },
            },
        ],
        options: RefreshOptions::default(),
        remembered_confirmation: None,
        has_succeeded: false,
    };

    // 2. Refresh local from remote & anonymize PII
    let refresh_anonymize = RefreshWorkflow {
        schema_version: WORKFLOW_SCHEMA_VERSION,
        id: WorkflowId::new(),
        name: "Refresh local & anonymize PII".into(),
        source: empty_ref(),
        source_mode: Some(WorkflowSource::FromRemoteDb {
            source: empty_ref(),
        }),
        target: empty_ref(),
        post_restore_script: None,
        parameters: vec![WorkflowParam {
            name: "mask_salt".into(),
            label: Some("Anonymization salt".into()),
            description: Some("Optional salt for hashing sensitive fields".into()),
            kind: ParamType::Secret,
            required: false,
            default: None,
        }],
        steps: vec![
            WorkflowStep {
                id: "step_mask_users".into(),
                name: "Anonymize user emails & phones".into(),
                enabled: true,
                no_transaction: false,
                kind: StepKind::UpdateMask {
                    schema: "public".into(),
                    table: "users".into(),
                    conditions: Vec::new(),
                    combinator: LogicalOp::And,
                    assignments: vec![
                        MaskAssignment {
                            column: "email".into(),
                            transform: MaskTransform::SyntheticEmail {
                                domain: "example.test".into(),
                                id_column: Some("id".into()),
                            },
                        },
                        MaskAssignment {
                            column: "phone".into(),
                            transform: MaskTransform::SetNull,
                        },
                    ],
                },
            },
            WorkflowStep {
                id: "step_verify_no_real_emails".into(),
                name: "Verify no production emails remain".into(),
                enabled: true,
                no_transaction: false,
                kind: StepKind::Verify {
                    assertion: VerifyAssertion::ZeroRows {
                        schema: "public".into(),
                        table: "users".into(),
                        conditions: vec![Predicate {
                            column: "email".into(),
                            op: FilterOp::NotILike,
                            value: Some(CellValue::Literal {
                                value: "%@example.test".into(),
                            }),
                        }],
                        combinator: LogicalOp::And,
                    },
                },
            },
        ],
        options: RefreshOptions::default(),
        remembered_confirmation: None,
        has_succeeded: false,
    };

    // 3. Seed test data in place
    let seed_in_place = RefreshWorkflow {
        schema_version: WORKFLOW_SCHEMA_VERSION,
        id: WorkflowId::new(),
        name: "Seed test data in place".into(),
        source: empty_ref(),
        source_mode: Some(WorkflowSource::InPlaceTarget {
            pre_run_backup: false,
        }),
        target: empty_ref(),
        post_restore_script: None,
        parameters: Vec::new(),
        steps: vec![
            WorkflowStep {
                id: "step_seed_rows".into(),
                name: "Insert test accounts".into(),
                enabled: true,
                no_transaction: false,
                kind: StepKind::InsertRows {
                    schema: "public".into(),
                    table: "users".into(),
                    columns: vec!["email".into(), "role".into()],
                    rows: vec![vec![
                        CellValue::Literal {
                            value: "qa@example.test".into(),
                        },
                        CellValue::Literal {
                            value: "tester".into(),
                        },
                    ]],
                    on_conflict: ConflictPolicy::DoNothing {
                        columns: Vec::new(),
                    },
                },
            },
            WorkflowStep {
                id: "step_verify_seeded".into(),
                name: "Verify QA user exists".into(),
                enabled: true,
                no_transaction: false,
                kind: StepKind::Verify {
                    assertion: VerifyAssertion::RowCount {
                        schema: "public".into(),
                        table: "users".into(),
                        op: CompareOp::Gte,
                        expected: 1,
                        conditions: vec![Predicate {
                            column: "email".into(),
                            op: FilterOp::Eq,
                            value: Some(CellValue::Literal {
                                value: "qa@example.test".into(),
                            }),
                        }],
                        combinator: LogicalOp::And,
                    },
                },
            },
        ],
        options: RefreshOptions::default(),
        remembered_confirmation: None,
        has_succeeded: false,
    };

    vec![
        WorkflowTemplate {
            id: "restore_replace_users".into(),
            name: "Restore backup, delete users & seed test users".into(),
            description: "Restore a backup file into staging, clear the users table, insert test accounts, verify row count, and atomically swap into place.".into(),
            workflow: restore_and_seed,
        },
        WorkflowTemplate {
            id: "refresh_anonymize_pii".into(),
            name: "Refresh local from remote & anonymize PII".into(),
            description: "Dump a remote database into local staging, replace user emails with synthetic addresses, null phone numbers, and verify no production emails remain.".into(),
            workflow: refresh_anonymize,
        },
        WorkflowTemplate {
            id: "seed_in_place".into(),
            name: "Seed test data in place".into(),
            description: "Insert test accounts into an existing database inside a single transaction (supports Dry Run rollback preview).".into(),
            workflow: seed_in_place,
        },
    ]
}
