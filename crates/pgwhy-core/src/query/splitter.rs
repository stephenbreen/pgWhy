//! Statement splitter and leading-keyword classifier.
//!
//! Splitting is a single forward scan over the text that tracks exactly the lexical contexts in
//! which a `;` does *not* end a statement — the same set psql's lexer tracks:
//!
//! - `'…'` literals with `''` doubling; `E'…'` literals additionally honour backslash escapes
//!   (plain strings treat `\` literally under `standard_conforming_strings = on`, the default;
//!   with it `off` the server gives plain strings backslash escapes too, so `'\'; x'` is one
//!   literal — pass the session's value to the `*_with` functions. `U&'…'` and bit strings
//!   never take backslash escapes to end a quote, whatever the setting);
//! - `"…"` quoted identifiers with `""` doubling;
//! - `--` line comments and nestable `/* … */` block comments;
//! - dollar-quoted bodies `$$…$$` / `$tag$…$tag$` — a `$` only opens one when it is not part of
//!   an identifier (`a$b$` is an identifier) and is not a positional parameter (`$1`);
//! - parentheses (`CREATE RULE … DO (a; b)`), and `BEGIN ATOMIC … END` bodies of SQL-standard
//!   `CREATE FUNCTION/PROCEDURE` (PG 14+), where `CASE … END` nests inside.
//!
//! All delimiters are ASCII, so every offset produced is a UTF-8 character boundary.
//!
//! Offsets are byte offsets into the text passed in (or into the whole document when using
//! [`split_range`]), so the executor can map server error positions back onto the editor buffer.

use std::ops::Range;

/// A lexical context left open at the end of the text — the statement runs to end of input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unterminated {
    SingleQuote,
    DoubleQuote,
    DollarQuote,
    BlockComment,
}

/// One statement found by the splitter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementRange {
    /// The whole span this statement owns: from the end of the previous terminator up to and
    /// including its own `;` (or end of input). Consecutive ranges tile the input.
    pub range: Range<usize>,
    /// What is sent to the server: leading whitespace/comments and trailing whitespace removed,
    /// terminator excluded. Never empty.
    pub trimmed: Range<usize>,
    /// Set when the text ended inside a literal, identifier, dollar body or comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unterminated: Option<Unterminated>,
}

impl StatementRange {
    /// The statement text to execute, sliced from the same text the ranges were computed on.
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.trimmed.clone()]
    }
}

/// Split a whole script, assuming `standard_conforming_strings = on` (the server default).
/// Empty statements (only whitespace/comments, `;;`) are omitted. Use [`split_with`] when the
/// session's real setting is known.
pub fn split(sql: &str) -> Vec<StatementRange> {
    split_with(sql, true)
}

/// [`split`] for a session whose `standard_conforming_strings` is `scs`.
pub fn split_with(sql: &str, scs: bool) -> Vec<StatementRange> {
    split_range_with(sql, 0..sql.len(), scs)
}

/// Split only `within` (e.g. the editor selection), returning offsets into the whole `sql`.
///
/// `within` is clamped to the text and must lie on character boundaries (editor selections do).
pub fn split_range(sql: &str, within: Range<usize>) -> Vec<StatementRange> {
    split_range_with(sql, within, true)
}

/// [`split_range`] for a session whose `standard_conforming_strings` is `scs`.
pub fn split_range_with(sql: &str, within: Range<usize>, scs: bool) -> Vec<StatementRange> {
    let end = within.end.min(sql.len());
    let start = within.start.min(end);
    let mut out = Vec::new();
    Scanner::new(sql.as_bytes(), start, end, scs).run(|s| out.push(s));
    out
}

/// The statement whose owned span contains `offset` ("run statement at cursor"). A cursor in
/// trailing whitespace/comments after the last `;` resolves to the last statement.
pub fn statement_at(statements: &[StatementRange], offset: usize) -> Option<&StatementRange> {
    statements
        .iter()
        .find(|s| s.range.contains(&offset) || s.trimmed.end == offset)
        .or_else(|| statements.iter().rev().find(|s| s.range.end <= offset))
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_char(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'$'
}

/// True when the quote at `at` opens an `E'…'` (escape-string) literal.
fn e_prefixed(b: &[u8], at: usize) -> bool {
    at > 0 && matches!(b[at - 1], b'e' | b'E') && (at < 2 || !is_ident_char(b[at - 2]))
}

/// True when the quote at `at` opens a literal that never takes backslash escapes whatever
/// `standard_conforming_strings` says: `U&'…'` (backslash introduces unicode escapes there) and
/// the bit-string forms `B'…'` / `X'…'`.
fn never_backslash(b: &[u8], at: usize) -> bool {
    let word_start = |k: usize| k == 0 || !is_ident_char(b[k - 1]);
    if at >= 1 && matches!(b[at - 1], b'b' | b'B' | b'x' | b'X') && word_start(at - 1) {
        return true;
    }
    at >= 2 && b[at - 1] == b'&' && matches!(b[at - 2], b'u' | b'U') && word_start(at - 2)
}

/// If a dollar-quote tag opens at `i` (`bytes[i] == b'$'`), its length including both `$`.
fn dollar_tag_len(bytes: &[u8], i: usize, end: usize) -> Option<usize> {
    let mut j = i + 1;
    if j < end && bytes[j] == b'$' {
        return Some(2);
    }
    if j >= end || !is_ident_start(bytes[j]) {
        return None; // `$1`, lone `$`
    }
    while j < end && is_ident_char(bytes[j]) && bytes[j] != b'$' {
        j += 1;
    }
    (j < end && bytes[j] == b'$').then(|| j + 1 - i)
}

/// Tracks the first words of the current statement to recognise `CREATE [OR REPLACE]
/// FUNCTION|PROCEDURE`, inside which `BEGIN` (ATOMIC) opens a block that `;` doesn't end.
#[derive(Default)]
struct Head {
    words: u8,
    create: bool,
    routine: bool,
}

impl Head {
    fn word(&mut self, w: &[u8]) {
        let n = self.words;
        self.words = self.words.saturating_add(1);
        match n {
            0 => self.create = w.eq_ignore_ascii_case(b"create"),
            1..=3 if self.create => {
                if w.eq_ignore_ascii_case(b"function") || w.eq_ignore_ascii_case(b"procedure") {
                    self.routine = true;
                    self.create = false;
                } else if !(w.eq_ignore_ascii_case(b"or") || w.eq_ignore_ascii_case(b"replace")) {
                    self.create = false;
                }
            }
            _ => {}
        }
    }
}

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
    end: usize,
    /// The session's `standard_conforming_strings`.
    scs: bool,
}

impl<'a> Scanner<'a> {
    fn new(b: &'a [u8], start: usize, end: usize, scs: bool) -> Self {
        Self {
            b,
            i: start,
            end,
            scs,
        }
    }

    fn run(mut self, mut emit: impl FnMut(StatementRange)) {
        while self.i < self.end {
            if let Some(s) = self.statement() {
                emit(s);
            }
        }
    }

    /// Scan one statement starting at `self.i`; `None` if it had no content.
    fn statement(&mut self) -> Option<StatementRange> {
        let b = self.b;
        let end = self.end;
        let start = self.i;
        let mut content_start: Option<usize> = None;
        let mut content_end = start;
        let mut parens: u32 = 0;
        let mut atomic: u32 = 0;
        let mut head = Head::default();
        let mut unterminated = None;

        while self.i < end {
            let c = b[self.i];
            let at = self.i;
            match c {
                b';' if parens == 0 && atomic == 0 => {
                    self.i += 1;
                    return content_start.map(|cs| StatementRange {
                        range: start..self.i,
                        trimmed: cs..content_end,
                        unterminated: None,
                    });
                }
                b'-' if self.peek(1) == Some(b'-') => {
                    while self.i < end && b[self.i] != b'\n' {
                        self.i += 1;
                    }
                    // a comment before any content is "leading"; after content it is kept
                    if content_start.is_some() {
                        content_end = self.i;
                    }
                    continue;
                }
                b'/' if self.peek(1) == Some(b'*') => {
                    let closed = self.block_comment();
                    if content_start.is_some() {
                        content_end = self.i;
                    }
                    if !closed {
                        unterminated = Some(Unterminated::BlockComment);
                    }
                    continue;
                }
                c if c.is_ascii_whitespace() => {
                    self.i += 1;
                    continue;
                }
                _ => {}
            }
            content_start.get_or_insert(at);
            match c {
                b'\'' => {
                    let escapes = e_prefixed(b, at) || (!self.scs && !never_backslash(b, at));
                    if !self.quoted(b'\'', escapes) {
                        unterminated = Some(Unterminated::SingleQuote);
                    }
                }
                b'"' => {
                    if !self.quoted(b'"', false) {
                        unterminated = Some(Unterminated::DoubleQuote);
                    }
                }
                b'$' => match dollar_tag_len(b, at, end) {
                    Some(n) => {
                        if !self.dollar_body(n) {
                            unterminated = Some(Unterminated::DollarQuote);
                        }
                    }
                    None => {
                        // `$1` parameter (or stray `$`): consume the digits with it
                        self.i += 1;
                        while self.i < end && b[self.i].is_ascii_digit() {
                            self.i += 1;
                        }
                    }
                },
                b'(' => {
                    parens += 1;
                    self.i += 1;
                }
                b')' => {
                    parens = parens.saturating_sub(1);
                    self.i += 1;
                }
                c if is_ident_start(c) => {
                    while self.i < end && is_ident_char(b[self.i]) {
                        self.i += 1;
                    }
                    let w = &b[at..self.i];
                    // `E'…'`: let the quote branch see the prefix, don't treat it as a word
                    if !(w.len() == 1 && self.peek(0) == Some(b'\'')) {
                        head.word(w);
                        if head.routine && parens == 0 {
                            let opens = w.eq_ignore_ascii_case(b"begin")
                                || (atomic > 0 && w.eq_ignore_ascii_case(b"case"));
                            if opens {
                                atomic += 1;
                            } else if atomic > 0 && w.eq_ignore_ascii_case(b"end") {
                                atomic -= 1;
                            }
                        }
                    }
                }
                _ => self.i += 1,
            }
            content_end = self.i;
        }
        content_start.map(|cs| StatementRange {
            range: start..end,
            trimmed: cs..content_end,
            unterminated,
        })
    }

    fn peek(&self, ahead: usize) -> Option<u8> {
        let j = self.i + ahead;
        (j < self.end).then(|| self.b[j])
    }

    /// Consume a `'…'` / `"…"` token starting at the opening quote. False if unterminated.
    fn quoted(&mut self, q: u8, backslash_escapes: bool) -> bool {
        self.i += 1;
        while self.i < self.end {
            let c = self.b[self.i];
            if backslash_escapes && c == b'\\' {
                self.i += 2;
                continue;
            }
            self.i += 1;
            if c == q {
                if self.peek(0) == Some(q) {
                    self.i += 1; // doubled quote
                } else {
                    return true;
                }
            }
        }
        self.i = self.end;
        false
    }

    /// Consume a nestable block comment starting at `/*`. False if unterminated.
    fn block_comment(&mut self) -> bool {
        let mut depth = 0u32;
        while self.i < self.end {
            if self.b[self.i] == b'/' && self.peek(1) == Some(b'*') {
                depth += 1;
                self.i += 2;
            } else if self.b[self.i] == b'*' && self.peek(1) == Some(b'/') {
                depth -= 1;
                self.i += 2;
                if depth == 0 {
                    return true;
                }
            } else {
                self.i += 1;
            }
        }
        false
    }

    /// Consume a dollar-quoted body whose opening tag is `tag_len` bytes at `self.i`.
    fn dollar_body(&mut self, tag_len: usize) -> bool {
        let tag = &self.b[self.i..self.i + tag_len];
        self.i += tag_len;
        while self.i < self.end {
            if self.b[self.i] == b'$' && self.b[self.i..self.end].starts_with(tag) {
                self.i += tag_len;
                return true;
            }
            self.i += 1;
        }
        false
    }
}

// ---------------------------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------------------------

/// An explicit transaction-control statement (tracked for the transaction indicator; refused in post-restore scripts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TxnControl {
    /// `BEGIN`, `START TRANSACTION`.
    Begin,
    /// `COMMIT`, `END`; `chain` for `AND CHAIN`.
    Commit {
        chain: bool,
    },
    /// `ROLLBACK`, `ABORT`; `chain` for `AND CHAIN`.
    Rollback {
        chain: bool,
    },
    Savepoint,
    Release,
    /// `ROLLBACK TO [SAVEPOINT]`.
    RollbackToSavepoint,
    PrepareTransaction,
    CommitPrepared,
    RollbackPrepared,
}

/// What the leading keywords say about a statement. Best-effort and lexical: it never parses
/// SQL, so it errs towards the statement forms Postgres actually documents.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementInfo {
    /// Upper-cased command label in the style of a command tag ("SELECT", "CREATE TABLE",
    /// "ALTER SYSTEM"). Shown in the messages pane — tokio-postgres's simple-query API does not
    /// expose the server's own tag text.
    pub command: String,
    pub txn_control: Option<TxnControl>,
    /// Why this statement cannot run inside a transaction block (`VACUUM`, `CREATE DATABASE`,
    /// `CREATE INDEX CONCURRENTLY`, …), or `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_transaction_block: Option<String>,
    /// A `COPY … FROM STDIN` / `TO STDOUT`: needs the client side of the COPY protocol, which
    /// the executor does not speak (psql's `\copy` is a meta-command).
    pub copy_client: bool,
    /// `SELECT`/`VALUES`/`TABLE`/`WITH`, or DML with `RETURNING`: worth describing up front to
    /// learn column types.
    pub returns_rows: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Word(&'a str),
    Open,
    Close,
    Other,
}

/// Top-level tokens of one statement: words (lowercase comparison is the caller's job),
/// parentheses, and everything else collapsed to `Other`. Literals, quoted identifiers,
/// dollar bodies and comments are skipped (a quoted identifier yields `Other`).
fn tokens(s: &str) -> impl Iterator<Item = Tok<'_>> {
    let b = s.as_bytes();
    let end = b.len();
    // classification only reads the leading words, which literals later in the text cannot move
    let mut sc = Scanner::new(b, 0, end, true);
    std::iter::from_fn(move || loop {
        if sc.i >= end {
            return None;
        }
        let at = sc.i;
        let c = b[at];
        match c {
            b'-' if sc.peek(1) == Some(b'-') => {
                while sc.i < end && b[sc.i] != b'\n' {
                    sc.i += 1;
                }
            }
            b'/' if sc.peek(1) == Some(b'*') => {
                sc.block_comment();
            }
            c if c.is_ascii_whitespace() => sc.i += 1,
            b'\'' => {
                sc.quoted(b'\'', e_prefixed(b, at));
                return Some(Tok::Other);
            }
            b'"' => {
                sc.quoted(b'"', false);
                return Some(Tok::Other);
            }
            b'$' => {
                match dollar_tag_len(b, at, end) {
                    Some(n) => {
                        sc.dollar_body(n);
                    }
                    None => {
                        sc.i += 1;
                        while sc.i < end && b[sc.i].is_ascii_digit() {
                            sc.i += 1;
                        }
                    }
                }
                return Some(Tok::Other);
            }
            b'(' => {
                sc.i += 1;
                return Some(Tok::Open);
            }
            b')' => {
                sc.i += 1;
                return Some(Tok::Close);
            }
            c if is_ident_start(c) => {
                while sc.i < end && is_ident_char(b[sc.i]) {
                    sc.i += 1;
                }
                // E'…' / B'…' / X'…' / U&'…' prefixes belong to the literal
                if sc.i - at == 1 && sc.peek(0) == Some(b'\'') {
                    continue;
                }
                return Some(Tok::Word(&s[at..sc.i]));
            }
            _ => {
                sc.i += 1;
                return Some(Tok::Other);
            }
        }
    })
}

/// Words at parenthesis depth 0, lowercased, up to `limit`.
fn top_words(stmt: &str, limit: usize) -> Vec<String> {
    let mut depth = 0u32;
    let mut out = Vec::new();
    for t in tokens(stmt) {
        match t {
            Tok::Open => depth += 1,
            Tok::Close => depth = depth.saturating_sub(1),
            Tok::Word(w) if depth == 0 => {
                out.push(w.to_ascii_lowercase());
                if out.len() >= limit {
                    break;
                }
            }
            _ => {}
        }
    }
    out
}

fn has(words: &[String], w: &str) -> bool {
    words.iter().any(|x| x == w)
}

fn upper(words: &[&str]) -> String {
    words.join(" ").to_ascii_uppercase()
}

/// Classify one statement's text (as produced by [`StatementRange::text`]).
pub fn classify(stmt: &str) -> StatementInfo {
    // enough words for `create unique index concurrently if not exists`, `alter table only x
    // detach partition y concurrently`, `copy (…) to stdout with …`
    let words = top_words(stmt, 48);
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
    let first = w(0);
    let mut info = StatementInfo {
        command: first.to_ascii_uppercase(),
        txn_control: None,
        no_transaction_block: None,
        copy_client: false,
        returns_rows: false,
    };
    let chain = has(&words, "chain") && !has(&words, "no");
    let no_txn = |what: &str| Some(format!("{what} cannot run inside a transaction block"));
    match first {
        "begin" => {
            info.txn_control = Some(TxnControl::Begin);
            info.command = "BEGIN".into();
        }
        "start" if w(1) == "transaction" => {
            info.txn_control = Some(TxnControl::Begin);
            info.command = "START TRANSACTION".into();
        }
        "commit" | "end" if w(1) == "prepared" => {
            info.txn_control = Some(TxnControl::CommitPrepared);
            info.command = "COMMIT PREPARED".into();
            info.no_transaction_block = no_txn("COMMIT PREPARED");
        }
        "commit" | "end" => {
            info.txn_control = Some(TxnControl::Commit { chain });
            info.command = "COMMIT".into();
        }
        "rollback" | "abort" if w(1) == "prepared" => {
            info.txn_control = Some(TxnControl::RollbackPrepared);
            info.command = "ROLLBACK PREPARED".into();
            info.no_transaction_block = no_txn("ROLLBACK PREPARED");
        }
        "rollback" | "abort" if has(&words, "to") => {
            info.txn_control = Some(TxnControl::RollbackToSavepoint);
            info.command = "ROLLBACK TO SAVEPOINT".into();
        }
        "rollback" | "abort" => {
            info.txn_control = Some(TxnControl::Rollback { chain });
            info.command = "ROLLBACK".into();
        }
        "savepoint" => info.txn_control = Some(TxnControl::Savepoint),
        "release" => {
            info.txn_control = Some(TxnControl::Release);
            info.command = "RELEASE".into();
        }
        "prepare" if w(1) == "transaction" => {
            info.txn_control = Some(TxnControl::PrepareTransaction);
            info.command = "PREPARE TRANSACTION".into();
        }
        "select" | "values" | "table" | "with" => {
            info.returns_rows = true;
            // `WITH … INSERT/UPDATE/DELETE` without RETURNING still describes fine (no columns)
            info.command = if first == "with" {
                "WITH".into()
            } else {
                first.to_ascii_uppercase()
            };
        }
        "insert" | "update" | "delete" | "merge" => {
            info.returns_rows = has(&words, "returning");
        }
        "vacuum" => info.no_transaction_block = no_txn("VACUUM"),
        "cluster" if words.len() == 1 || (words.len() == 2 && w(1) == "verbose") => {
            info.no_transaction_block = no_txn("CLUSTER without a table name");
        }
        "reindex" => {
            info.command = "REINDEX".into();
            if has(&words, "concurrently") {
                info.no_transaction_block = no_txn("REINDEX CONCURRENTLY");
            } else if has(&words, "database") || has(&words, "system") {
                info.no_transaction_block = no_txn("REINDEX DATABASE/SYSTEM");
            }
        }
        "create" | "drop" | "alter" => {
            let object = words
                .iter()
                .skip(1)
                .map(String::as_str)
                .find(|x| {
                    !matches!(
                        *x,
                        "or" | "replace"
                            | "unique"
                            | "temp"
                            | "temporary"
                            | "unlogged"
                            | "global"
                            | "local"
                            | "recursive"
                            | "trusted"
                            | "procedural"
                            | "default"
                            | "constraint"
                    )
                })
                .unwrap_or("");
            // multi-word object kinds keep their second word ("FOREIGN TABLE", "EVENT TRIGGER")
            let pos = words.iter().position(|x| x == object).unwrap_or(1);
            let second = w(pos + 1);
            let two_word = matches!(
                (object, second),
                ("foreign", "table" | "data")
                    | ("event", "trigger")
                    | ("text", "search")
                    | ("user", "mapping")
                    | ("access", "method")
                    | ("operator", "class" | "family")
                    | ("materialized", "view")
            );
            info.command = if object.is_empty() {
                first.to_ascii_uppercase()
            } else if two_word {
                upper(&[first, object, second])
            } else {
                upper(&[first, object])
            };
            info.no_transaction_block = match (first, object) {
                ("create" | "drop", "database") => no_txn(&upper(&[first, "database"])),
                ("create" | "drop", "tablespace") => no_txn(&upper(&[first, "tablespace"])),
                ("alter", "system") => no_txn("ALTER SYSTEM"),
                ("create" | "drop", "index") if has(&words, "concurrently") => {
                    no_txn(&upper(&[first, "index concurrently"]))
                }
                ("alter", "table") if has(&words, "detach") && has(&words, "concurrently") => {
                    no_txn("ALTER TABLE … DETACH PARTITION CONCURRENTLY")
                }
                ("alter", "database") if has(&words, "tablespace") && has(&words, "set") => {
                    no_txn("ALTER DATABASE … SET TABLESPACE")
                }
                // subscriptions that create/drop a replication slot (the default)
                ("create" | "drop", "subscription") => no_txn(&upper(&[first, "subscription"])),
                _ => None,
            };
        }
        "copy" => {
            // `COPY x FROM STDIN` / `COPY x TO STDOUT` (and `copy (query) to stdout`)
            info.copy_client = words.windows(2).any(|p| {
                matches!(
                    (p[0].as_str(), p[1].as_str()),
                    ("from", "stdin") | ("to", "stdout")
                )
            });
        }
        "discard" if w(1) == "all" => info.no_transaction_block = no_txn("DISCARD ALL"),
        _ => {}
    }
    info
}

/// Post-restore script checks over a whole script, for the workbench warning, preflight and
/// the attach-time refusal. Indices refer to `statements`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScriptAnalysis {
    pub statements: Vec<StatementRange>,
    /// Statements that are explicit transaction control (`BEGIN`, `COMMIT`, `SAVEPOINT`, …).
    pub txn_control: Vec<usize>,
    /// Statements that cannot run inside a transaction block, with the reason.
    pub needs_no_transaction: Vec<(usize, String)>,
    /// `COPY … FROM STDIN / TO STDOUT` statements (the executor refuses them).
    pub copy_client: Vec<usize>,
    /// Statements the text ended inside of (unterminated literal/comment/dollar body).
    pub unterminated: Vec<usize>,
}

impl ScriptAnalysis {
    /// True when the script controls its own transactions — refused for single-transaction
    /// post-restore scripts.
    pub fn contains_transaction_control(&self) -> bool {
        !self.txn_control.is_empty()
    }

    /// True when the script can only run with the per-workflow "run without transaction" option.
    pub fn needs_no_transaction_mode(&self) -> bool {
        !self.needs_no_transaction.is_empty()
    }
}

/// Split and classify a whole script, assuming `standard_conforming_strings = on`.
pub fn analyze(sql: &str) -> ScriptAnalysis {
    analyze_with(sql, true)
}

/// [`analyze`] for a session whose `standard_conforming_strings` is `scs`.
pub fn analyze_with(sql: &str, scs: bool) -> ScriptAnalysis {
    let statements = split_with(sql, scs);
    let mut a = ScriptAnalysis {
        statements: Vec::new(),
        txn_control: Vec::new(),
        needs_no_transaction: Vec::new(),
        copy_client: Vec::new(),
        unterminated: Vec::new(),
    };
    for (i, s) in statements.iter().enumerate() {
        let info = classify(s.text(sql));
        if info.txn_control.is_some() {
            a.txn_control.push(i);
        }
        if let Some(why) = info.no_transaction_block {
            a.needs_no_transaction.push((i, why));
        }
        if info.copy_client {
            a.copy_client.push(i);
        }
        if s.unterminated.is_some() {
            a.unterminated.push(i);
        }
    }
    a.statements = statements;
    a
}
