//! psql meta-command detection (R22, RV-7).
//!
//! pgWhy executes scripts with its own SQL executor, which does not understand psql
//! backslash commands (`\c`, `\i`, `\set`, `\copy`, `\gexec`, ...). Workflow attach (U16)
//! refuses scripts containing them and the workbench warns, so detection must not raise
//! false positives on backslashes that are just data: inside string literals (including
//! `E'...'` escapes), quoted identifiers, dollar-quoted bodies and comments.
//!
//! This is a small self-contained lexer rather than a full SQL tokenizer: it only needs to
//! know whether a byte is in "code" context. [`scan_meta_commands`] assumes
//! `standard_conforming_strings = on` (the default since PostgreSQL 9.1), where backslash is
//! literal inside plain `'...'`; a caller that knows the target session has it `off` uses
//! [`scan_meta_commands_with`] (the statement splitter takes the same flag).

use serde::{Deserialize, Serialize};

/// Where on its line a meta-command starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaPosition {
    /// Only whitespace precedes the backslash on its line: the classic psql command line
    /// (`\c db`, `\i file.sql`, `\set x 1`).
    LineStart,
    /// After SQL on the same line (`select 1 \gexec`, `... \g`). psql treats these as
    /// meta-commands too; outside quotes and comments a backslash is never valid SQL, so
    /// flagging them cannot misfire on a valid script.
    Inline,
}

/// One psql meta-command found in a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaCommand {
    /// The command word including the backslash, e.g. `\c`, `\copy`, `\d+`.
    pub command: String,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, counted in characters (not bytes).
    pub column: u32,
    /// Byte offset of the backslash in the script.
    pub offset: usize,
    /// The command and its arguments up to the end of the line, trimmed — for messages.
    pub text: String,
    pub position: MetaPosition,
}

/// Whether the script contains any psql meta-command (at line start or inline).
pub fn has_meta_commands(sql: &str) -> bool {
    !scan_meta_commands(sql).is_empty()
}

/// Find every psql meta-command outside string, identifier, dollar-quote and comment
/// context, in source order. Callers that only care about the line-start form filter on
/// [`MetaCommand::position`].
pub fn scan_meta_commands(sql: &str) -> Vec<MetaCommand> {
    scan_meta_commands_with(sql, true)
}

/// [`scan_meta_commands`] for a session whose `standard_conforming_strings` is `scs`: when
/// `off`, a backslash escapes the next byte in plain `'...'` literals too (bit strings and
/// `U&'...'` excepted, as for the splitter).
pub fn scan_meta_commands_with(sql: &str, scs: bool) -> Vec<MetaCommand> {
    let b = sql.as_bytes();
    let line_starts = line_starts(b);
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                let escapes = is_e_string_prefix(b, i) || (!scs && !is_non_escape_prefix(b, i));
                i = skip_quoted(b, i + 1, b'\'', escapes);
            }
            b'"' => i = skip_quoted(b, i + 1, b'"', false),
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = memchr_newline(b, i + 2);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i + 2),
            b'$' => match dollar_tag(b, i) {
                Some(tag_end) => {
                    let delim = &b[i..tag_end];
                    i = find(b, tag_end, delim).map_or(b.len(), |p| p + delim.len());
                }
                None => i += 1,
            },
            b'\\' => {
                let cmd = meta_at(sql, i, &line_starts);
                // A meta-command consumes the rest of its line (its arguments are not SQL;
                // `\set x 'it''s'` must not open a string that swallows the next lines).
                i = memchr_newline(b, i + 1);
                out.push(cmd);
            }
            _ => i += 1,
        }
    }
    out
}

fn meta_at(sql: &str, at: usize, line_starts: &[usize]) -> MetaCommand {
    let b = sql.as_bytes();
    let line_idx = line_starts.partition_point(|&s| s <= at) - 1;
    let line_start = line_starts[line_idx];
    let line_end = memchr_newline(b, at);

    // Command word: the backslash plus everything up to whitespace or the next backslash;
    // a doubled backslash (`\\`, psql's separator) is a word of its own.
    let mut end = at + 1;
    if b.get(end) == Some(&b'\\') {
        end += 1;
    } else {
        while end < line_end && !b[end].is_ascii_whitespace() && b[end] != b'\\' {
            end += 1;
        }
    }
    let before = &b[line_start..at];
    let position = if before
        .iter()
        .all(|c| matches!(c, b' ' | b'\t' | b'\r' | 0x0c))
    {
        MetaPosition::LineStart
    } else {
        MetaPosition::Inline
    };
    MetaCommand {
        command: sql[at..end].to_string(),
        line: u32::try_from(line_idx + 1).unwrap_or(u32::MAX),
        column: u32::try_from(sql[line_start..at].chars().count() + 1).unwrap_or(u32::MAX),
        offset: at,
        text: sql[at..line_end].trim_end().to_string(),
        position,
    }
}

fn line_starts(b: &[u8]) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(
        b.iter()
            .enumerate()
            .filter(|(_, &c)| c == b'\n')
            .map(|(i, _)| i + 1),
    );
    v
}

/// Index of the next `\n` at or after `from`, or the end of input.
fn memchr_newline(b: &[u8], from: usize) -> usize {
    b[from.min(b.len())..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| from + p)
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from > b.len() {
        return None;
    }
    b[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// `'` at `quote` opens an escape string when directly preceded by a standalone `E`/`e`
/// (not the tail of an identifier such as `type'...'`).
fn is_e_string_prefix(b: &[u8], quote: usize) -> bool {
    quote >= 1 && matches!(b[quote - 1], b'e' | b'E') && (quote < 2 || !is_ident_byte(b[quote - 2]))
}

/// `'` at `quote` opens `U&'...'` or a `B'...'`/`X'...'` bit string, which never give a
/// backslash the power to escape the closing quote.
fn is_non_escape_prefix(b: &[u8], quote: usize) -> bool {
    let word_start = |k: usize| k == 0 || !is_ident_byte(b[k - 1]);
    (quote >= 1 && matches!(b[quote - 1], b'b' | b'B' | b'x' | b'X') && word_start(quote - 1))
        || (quote >= 2
            && b[quote - 1] == b'&'
            && matches!(b[quote - 2], b'u' | b'U')
            && word_start(quote - 2))
}

/// Skip past a `q`-delimited literal whose body starts at `from`; a doubled delimiter is an
/// escaped delimiter, and with `backslash_escapes` a backslash escapes the next byte.
/// Unterminated literals run to the end of input.
fn skip_quoted(b: &[u8], mut i: usize, q: u8, backslash_escapes: bool) -> usize {
    while i < b.len() {
        let c = b[i];
        if backslash_escapes && c == b'\\' {
            i += 2;
        } else if c == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// Skip a (possibly nested, as in PostgreSQL) block comment whose body starts at `from`.
fn skip_block_comment(b: &[u8], mut i: usize) -> usize {
    let mut depth = 1usize;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// If a dollar-quote delimiter (`$$` or `$tag$`) starts at `at`, return the index just past
/// it. `$1` parameters and `$` inside identifiers (`a$b`) are not delimiters.
fn dollar_tag(b: &[u8], at: usize) -> Option<usize> {
    if at > 0 && is_ident_byte(b[at - 1]) {
        return None;
    }
    let mut i = at + 1;
    match b.get(i) {
        Some(b'$') => return Some(i + 1),
        Some(&c) if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {}
        _ => return None,
    }
    while let Some(&c) = b.get(i) {
        if c == b'$' {
            return Some(i + 1);
        }
        if !(c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80) {
            return None;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(sql: &str) -> Vec<String> {
        scan_meta_commands(sql)
            .into_iter()
            .map(|m| m.command)
            .collect()
    }

    #[test]
    fn finds_line_start_commands_with_positions() {
        let sql = "select 1;\n  \\c other\n\\set x 1\nselect 2;\n\t\\i  'file.sql'\n";
        let found = scan_meta_commands(sql);
        assert_eq!(
            found.iter().map(|m| m.command.as_str()).collect::<Vec<_>>(),
            ["\\c", "\\set", "\\i"]
        );
        assert_eq!((found[0].line, found[0].column), (2, 3));
        assert_eq!((found[1].line, found[1].column), (3, 1));
        assert_eq!((found[2].line, found[2].column), (5, 2));
        assert_eq!(found[2].text, "\\i  'file.sql'");
        assert!(found.iter().all(|m| m.position == MetaPosition::LineStart));
        assert_eq!(&sql[found[0].offset..found[0].offset + 2], "\\c");
    }

    #[test]
    fn command_words() {
        assert_eq!(
            commands("\\copy t from 'x'\n\\gexec\n\\d+ t\n\\!ls\n\\\\\n\\"),
            ["\\copy", "\\gexec", "\\d+", "\\!ls", "\\\\", "\\"]
        );
    }

    #[test]
    fn inline_commands_are_reported_as_inline() {
        let found = scan_meta_commands("select format('x') \\gexec\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].command, "\\gexec");
        assert_eq!(found[0].position, MetaPosition::Inline);
        assert_eq!(found[0].column, 20);
    }

    #[test]
    fn ignores_backslashes_in_literals_and_comments() {
        for sql in [
            "select '\n\\c not a command\n';",
            "select E'it\\'s\n\\c still inside\n';",
            "select e'\\\\';\nselect 1;",
            "select \"weird\n\\c ident\";",
            "do $$\nbegin\n\\c nope\nend $$;",
            "do $fn$\n\\i nope $$ still body\n$fn$;",
            "/* block\n\\c nope\n /* nested */ \\c still nope\n*/ select 1;",
            "-- \\c in a line comment\nselect 1;",
            "select 1; -- trailing \\gexec\n",
        ] {
            assert!(commands(sql).is_empty(), "false positive in {sql:?}");
        }
    }

    #[test]
    fn scs_off_plain_strings_take_backslash_escapes() {
        let sql = "select '\\'; \\c db; --'";
        // on: the literal is `\`, then `\c` is a real meta-command
        assert_eq!(commands(sql), ["\\c"]);
        // off: `\'` is an escaped quote, the literal runs to the end
        assert!(scan_meta_commands_with(sql, false).is_empty());
        // U&'' never escapes its closing quote
        assert_eq!(scan_meta_commands_with("select U&'\\' \\g", false).len(), 1);
    }

    #[test]
    fn e_string_escape_does_not_end_literal_early() {
        // In an E-string `\'` is an escaped quote, so `\c` below is still in the literal;
        // in a standard string the same bytes end the literal and `\c` is code.
        assert!(commands("select E'a\\';\n\\c x\n';").is_empty());
        assert_eq!(commands("select 'a\\';\n\\c x\n"), ["\\c"]);
        // An identifier ending in `e` is not an E prefix.
        assert_eq!(commands("select type'a\\';\n\\c x\n"), ["\\c"]);
    }

    #[test]
    fn code_after_literals_is_scanned() {
        let sql = "select 'a''b', \"q\"\"x\", $$body$$, $t$x$t$;\n\\c db\nselect $1, a$b$;\n\\q";
        assert_eq!(commands(sql), ["\\c", "\\q"]);
    }

    #[test]
    fn meta_line_arguments_do_not_open_literals() {
        assert_eq!(commands("\\set x 'it\nselect 1;\n\\c db"), ["\\set", "\\c"]);
    }

    #[test]
    fn unterminated_contexts_run_to_end() {
        assert!(commands("select 'abc\n\\c x").is_empty());
        assert!(commands("/* open\n\\c x").is_empty());
        assert!(commands("select $$open\n\\c x").is_empty());
    }

    #[test]
    fn columns_count_characters() {
        let found = scan_meta_commands("select 'é' \\g");
        assert_eq!(found[0].column, 12);
        assert_eq!(found[0].offset, 12);
        assert!(!has_meta_commands("select 'é';"));
    }
}
