//! Statement splitter and classifier (U18, EC-17): naive `;` splitting corrupts function bodies.

use pgwhy_core::query::splitter::{
    analyze, analyze_with, classify, split, split_range, split_range_with, split_with,
    statement_at, TxnControl, Unterminated,
};

/// The trimmed statement texts.
fn texts(sql: &str) -> Vec<&str> {
    split(sql).iter().map(|s| s.text(sql)).collect()
}

#[test]
fn simple_statements_and_trailing_statement_without_semicolon() {
    assert_eq!(
        texts("select 1; select 2;\nselect 3"),
        ["select 1", "select 2", "select 3"]
    );
}

#[test]
fn ranges_tile_the_input_and_trimmed_excludes_leading_comments() {
    let sql = "  -- first\n/* lead */ select 1 ;\n\n  select 2 -- tail\n;";
    let st = split(sql);
    assert_eq!(st.len(), 2);
    assert_eq!(st[0].range.start, 0);
    assert_eq!(st[0].range.end, st[1].range.start);
    assert_eq!(st[1].range.end, sql.len());
    assert_eq!(st[0].text(sql), "select 1");
    // a comment after content stays part of the statement
    assert_eq!(st[1].text(sql), "select 2 -- tail");
    assert!(st.iter().all(|s| s.unterminated.is_none()));
}

#[test]
fn empty_statements_are_omitted() {
    assert_eq!(texts(";;  ; -- nothing\n; /* x */ ;"), Vec::<&str>::new());
    assert_eq!(
        texts("select 1;;select 2; -- bye"),
        ["select 1", "select 2"]
    );
    assert!(split("").is_empty());
    assert!(split("   \n\t").is_empty());
}

#[test]
fn dollar_quoted_function_bodies_keep_their_semicolons() {
    let body = "create function f() returns int language plpgsql as $$\nbegin\n  perform 1;\n  return 2;\nend;\n$$";
    let sql = format!("{body};\nselect f();");
    assert_eq!(texts(&sql), [body, "select f()"]);
}

#[test]
fn tagged_and_nested_dollar_quotes() {
    let outer =
        "do $outer$ begin execute $inner$ select ';' $inner$; raise notice '$$;'; end $outer$";
    let sql = format!("{outer}; select 2");
    assert_eq!(texts(&sql), [outer, "select 2"]);
    // a $$ inside a $fn$ body does not close it
    let s = "select $fn$ a $$ ; b $fn$; select 3";
    assert_eq!(texts(s), ["select $fn$ a $$ ; b $fn$", "select 3"]);
}

#[test]
fn positional_parameters_and_identifiers_with_dollars_are_not_quotes() {
    // `$1`/`$2` are parameters; `a$b$` is an identifier — neither opens a dollar quote
    let sql = "prepare p as select $1 + $2; select a$b$ from t; select 1";
    assert_eq!(
        texts(sql),
        [
            "prepare p as select $1 + $2",
            "select a$b$ from t",
            "select 1"
        ]
    );
}

#[test]
fn strings_with_semicolons_and_doubled_quotes() {
    let sql = "select 'a;b', 'it''s; fine'; select \"we;ird\"\"id\" from t; select 2";
    assert_eq!(
        texts(sql),
        [
            "select 'a;b', 'it''s; fine'",
            "select \"we;ird\"\"id\" from t",
            "select 2"
        ]
    );
}

#[test]
fn e_strings_honour_backslash_escapes_standard_strings_do_not() {
    // E'\'' is one quote char; the ; after it is inside the literal
    let sql = r"select E'it\'s; ok', e'\\'; select 'C:\'; select 3";
    assert_eq!(
        texts(sql),
        [r"select E'it\'s; ok', e'\\'", r"select 'C:\'", "select 3"]
    );
    // an identifier ending in e is not an E prefix
    let sql = r"select name'x\'; select 4";
    assert_eq!(texts(sql), [r"select name'x\'", "select 4"]);
}

#[test]
fn comments_with_semicolons_and_nesting() {
    let sql = "select 1 /* a; /* nested; */ still; */ + 1; -- c;\nselect 2";
    assert_eq!(
        texts(sql),
        ["select 1 /* a; /* nested; */ still; */ + 1", "select 2"]
    );
}

#[test]
fn parentheses_and_begin_atomic_bodies() {
    let rule = "create rule r as on insert to t do also (insert into a values (1); insert into b values (2))";
    let sql = format!("{rule}; select 1");
    assert_eq!(texts(&sql), [rule.to_string(), "select 1".into()]);

    let f = "create or replace function f(x int) returns int language sql\nbegin atomic\n  select case when x > 0 then 1 else 0 end;\n  select 2;\nend";
    let sql = format!("{f};\nselect f(1);");
    assert_eq!(texts(&sql), [f, "select f(1)"]);
    // a plain transaction BEGIN still splits
    assert_eq!(
        texts("begin; select 1; commit;"),
        ["begin", "select 1", "commit"]
    );
}

#[test]
fn unterminated_contexts_run_to_end_and_are_flagged() {
    for (sql, kind) in [
        ("select 'abc; select 2", Unterminated::SingleQuote),
        ("select \"abc; select 2", Unterminated::DoubleQuote),
        ("select $$abc; select 2", Unterminated::DollarQuote),
        ("select 1 /* abc; select 2", Unterminated::BlockComment),
    ] {
        let st = split(sql);
        assert_eq!(st.len(), 1, "{sql}");
        assert_eq!(st[0].unterminated, Some(kind), "{sql}");
        assert_eq!(st[0].range.end, sql.len());
    }
}

#[test]
fn multibyte_text_keeps_char_boundaries() {
    let sql = "select 'héllo; 日本'; select '✓' as ok";
    let st = split(sql);
    assert_eq!(texts(sql), ["select 'héllo; 日本'", "select '✓' as ok"]);
    for s in &st {
        assert!(sql.is_char_boundary(s.trimmed.start) && sql.is_char_boundary(s.trimmed.end));
    }
}

#[test]
fn split_range_returns_absolute_offsets() {
    let sql = "select 0;\nselect 1; select 2;\nselect 3;";
    let sel_start = sql.find("select 1").unwrap();
    let sel_end = sql.find("select 3").unwrap();
    let st = split_range(sql, sel_start..sel_end);
    assert_eq!(st.len(), 2);
    assert_eq!(st[0].text(sql), "select 1");
    assert_eq!(st[1].text(sql), "select 2");
    assert_eq!(st[0].trimmed.start, sel_start);
}

#[test]
fn statement_at_cursor() {
    let sql = "select 1;\nselect 2;\n-- trailing";
    let st = split(sql);
    assert_eq!(statement_at(&st, 0).unwrap().text(sql), "select 1");
    assert_eq!(statement_at(&st, 12).unwrap().text(sql), "select 2");
    assert_eq!(statement_at(&st, sql.len()).unwrap().text(sql), "select 2");
}

#[test]
fn classify_transaction_control() {
    use TxnControl::*;
    let cases = [
        ("BEGIN", Some(Begin)),
        ("begin isolation level serializable", Some(Begin)),
        ("start transaction read only", Some(Begin)),
        ("commit", Some(Commit { chain: false })),
        ("END", Some(Commit { chain: false })),
        ("commit and chain", Some(Commit { chain: true })),
        ("commit and no chain", Some(Commit { chain: false })),
        ("rollback", Some(Rollback { chain: false })),
        ("abort", Some(Rollback { chain: false })),
        ("rollback to savepoint s1", Some(RollbackToSavepoint)),
        ("rollback to s1", Some(RollbackToSavepoint)),
        ("savepoint s1", Some(Savepoint)),
        ("release savepoint s1", Some(Release)),
        ("prepare transaction 'gid'", Some(PrepareTransaction)),
        ("commit prepared 'gid'", Some(CommitPrepared)),
        ("rollback prepared 'gid'", Some(RollbackPrepared)),
        ("prepare p as select 1", None),
        ("select 'begin'", None),
        ("/* commit */ select 1", None),
        ("do $$ begin commit; end $$", None),
    ];
    for (sql, want) in cases {
        assert_eq!(classify(sql).txn_control, want, "{sql}");
    }
}

#[test]
fn classify_no_transaction_block_statements() {
    let yes = [
        "vacuum",
        "VACUUM (VERBOSE, ANALYZE) t",
        "create database x",
        "drop database if exists x",
        "create index concurrently i on t(x)",
        "create unique index concurrently if not exists i on t(x)",
        "drop index concurrently i",
        "reindex index concurrently i",
        "reindex (verbose) table concurrently t",
        "reindex database x",
        "alter system set work_mem = '64MB'",
        "create tablespace ts location '/x'",
        "alter table p detach partition c concurrently",
        "cluster",
        "discard all",
        "commit prepared 'g'",
    ];
    for sql in yes {
        assert!(classify(sql).no_transaction_block.is_some(), "{sql}");
    }
    let no = [
        "analyze t",
        "create index i on t(x)",
        "create index concurrently_named on t(x)",
        "reindex table t",
        "refresh materialized view concurrently mv",
        "cluster t using i",
        "select 'vacuum'",
        "alter table t add column c int",
    ];
    for sql in no {
        assert!(classify(sql).no_transaction_block.is_none(), "{sql}");
    }
}

#[test]
fn classify_command_labels_and_flags() {
    assert_eq!(classify("select 1").command, "SELECT");
    assert!(classify("select 1").returns_rows);
    assert!(classify("with x as (select 1) select * from x").returns_rows);
    assert!(classify("insert into t values (1) returning id").returns_rows);
    assert!(!classify("insert into t values (1)").returns_rows);
    assert_eq!(
        classify("create or replace function f() returns int as $$ select 1 $$ language sql")
            .command,
        "CREATE FUNCTION"
    );
    assert_eq!(
        classify("CREATE UNIQUE INDEX i ON t(x)").command,
        "CREATE INDEX"
    );
    assert_eq!(
        classify("create temp table t(x int)").command,
        "CREATE TABLE"
    );
    assert_eq!(
        classify("create materialized view v as select 1").command,
        "CREATE MATERIALIZED VIEW"
    );
    assert_eq!(
        classify("create foreign table f() server s").command,
        "CREATE FOREIGN TABLE"
    );
    assert_eq!(classify("drop table t").command, "DROP TABLE");
    assert!(classify("copy t from stdin").copy_client);
    assert!(classify("COPY (select 'stdin') TO STDOUT with csv").copy_client);
    assert!(!classify("copy t to '/tmp/x.csv'").copy_client);
    assert!(!classify("copy t(stdin) from '/tmp/x'").copy_client);
}

#[test]
fn analyze_scripts_for_r22() {
    let a = analyze("create table t(x int);\ninsert into t values (1);\ncommit;");
    assert!(a.contains_transaction_control());
    assert_eq!(a.txn_control, [2]);
    assert!(!a.needs_no_transaction_mode());

    let a = analyze("create index concurrently i on t(x); vacuum analyze t;");
    assert!(!a.contains_transaction_control());
    assert!(a.needs_no_transaction_mode());
    assert_eq!(
        a.needs_no_transaction
            .iter()
            .map(|(i, _)| *i)
            .collect::<Vec<_>>(),
        [0, 1]
    );

    // a function body containing COMMIT is not explicit transaction control
    let a = analyze("create procedure p() language plpgsql as $$ begin commit; end $$;");
    assert!(!a.contains_transaction_control());
}

// ---- standard_conforming_strings = off ----------------------------------------------------

fn texts_scs(sql: &str, scs: bool) -> Vec<&str> {
    split_with(sql, scs).iter().map(|s| s.text(sql)).collect()
}

#[test]
fn scs_off_backslash_quote_does_not_end_a_plain_string() {
    let sql = "select '\\'; delete from t; --'; select 2";
    // scs on: the string is `\`, so the `;` after it really ends the statement
    assert_eq!(
        texts_scs(sql, true),
        ["select '\\'", "delete from t"] // the rest is a comment
    );
    // scs off: one literal runs to the final quote
    assert_eq!(
        texts_scs(sql, false),
        ["select '\\'; delete from t; --'", "select 2"]
    );
}

#[test]
fn scs_off_leaves_doubling_e_strings_and_non_escape_literals_alone() {
    // doubled quotes still work
    assert_eq!(texts_scs("select 'a''b;c'; select 2", false).len(), 2);
    // E'' is the same either way
    assert_eq!(texts_scs("select E'\\';x'; select 2", false).len(), 2);
    assert_eq!(texts_scs("select E'\\';x'; select 2", true).len(), 2);
    // U&'' and bit strings never let a backslash escape the closing quote
    assert_eq!(texts_scs("select U&'\\'; select 2", false).len(), 2);
    assert_eq!(texts_scs("select u&'\\'; select 2", false).len(), 2);
    assert_eq!(texts_scs("select x'\\'; select 2", false).len(), 2);
    assert_eq!(texts_scs("select b'\\'; select 2", false).len(), 2);
    // quoted identifiers never take backslash escapes
    assert_eq!(texts_scs("select \"a\\\"; select 2", false).len(), 2);
}

#[test]
fn scs_off_split_range_and_analyze() {
    let sql = "select 1; select '\\'; x";
    let st = split_range_with(sql, 10..sql.len(), false);
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].unterminated, Some(Unterminated::SingleQuote));
    assert_eq!(
        analyze_with("select '\\'; select 2'", false)
            .statements
            .len(),
        1
    );
    assert_eq!(analyze("select '\\'; select 2'").statements.len(), 2);
}
