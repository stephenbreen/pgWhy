//! The one identifier-quoting helper for every piece of SQL pgWhy generates.
//!
//! Catalog names are attacker-controlled input: a hostile or compromised remote can name a
//! table `x"; drop table y; --`. Every catalog-derived identifier that ends up in generated SQL
//! (Script-as, the criteria row, ORDER BY, edit statements) goes through [`quote_ident`] /
//! [`qualified`]; values are never spliced into SQL — they always travel as bind parameters.
//!
//! [`quote_ident`] mirrors the server's own `quote_ident()`: a name is left bare only when it is
//! lowercase ASCII `[a-z_][a-z0-9_]*` and not a keyword that the grammar reserves in any
//! position (reserved, column-name, or type/function-name keywords). Everything else is
//! double-quoted with embedded `"` doubled.

/// Keywords that cannot be used as bare identifiers everywhere (PostgreSQL 18's
/// `pg_get_keywords()` rows with `catcode <> 'U'`). A superset is safe — quoting an
/// identifier unnecessarily never changes its meaning — so words dropped by newer servers stay.
/// An integration test asserts this list covers every supported server's list.
const NON_UNRESERVED_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// True when `word` is a keyword that needs quoting to be used as an identifier.
pub fn is_reserved_keyword(word: &str) -> bool {
    NON_UNRESERVED_KEYWORDS.binary_search(&word).is_ok()
}

/// True when `name` must be double-quoted to be read back as exactly `name`.
pub fn needs_quoting(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return true; // "" is not a valid bare identifier
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return true;
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return true;
    }
    is_reserved_keyword(name)
}

/// Quote `name` as a SQL identifier when needed (matches the server's `quote_ident()`).
pub fn quote_ident(name: &str) -> String {
    if needs_quoting(name) {
        quote_ident_always(name)
    } else {
        name.to_string()
    }
}

/// Always double-quote `name`, doubling embedded quotes.
pub fn quote_ident_always(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `schema.name`, each part quoted as needed — the form generated SQL must use, since an
/// unqualified name resolves through the session's `search_path`.
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// A psql/pg_dump *pattern* (`pg_dump -t`, `-n`) that matches exactly `schema.name` and
/// nothing else. Patterns are not identifiers: unquoted parts are case-folded and `*`, `?`,
/// `%`-like regex characters act as wildcards; inside double quotes everything is literal and
/// `""` is a literal quote. So both parts are always quoted.
///
/// Returns `None` for names containing a newline or carriage return, which pg_dump refuses in
/// patterns (callers should disable the action with that reason).
pub fn dump_pattern(schema: &str, name: &str) -> Option<String> {
    let bad = |s: &str| s.contains('\n') || s.contains('\r');
    if bad(schema) || bad(name) {
        return None;
    }
    Some(format!(
        "{}.{}",
        quote_ident_always(schema),
        quote_ident_always(name)
    ))
}

/// Escape `%`, `_` and `\` so `text` matches literally inside a `LIKE`/`ILIKE` pattern
/// (default escape character `\`). The result is still bound as a parameter.
pub fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_list_is_sorted_for_binary_search() {
        let mut sorted = NON_UNRESERVED_KEYWORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, NON_UNRESERVED_KEYWORDS);
    }

    #[test]
    fn plain_names_stay_bare() {
        for n in ["users", "_x", "a1", "order_items", "name", "year", "text"] {
            assert_eq!(quote_ident(n), n, "{n}");
        }
    }

    #[test]
    fn adversarial_names_are_quoted_inertly() {
        let cases = [
            ("Users", r#""Users""#),
            ("select", r#""select""#),
            ("user", r#""user""#),
            ("order", r#""order""#),
            ("with space", r#""with space""#),
            (r#"Weird "Name""#, r#""Weird ""Name""""#),
            (r#"x"; drop table y; --"#, r#""x""; drop table y; --""#),
            ("100%", r#""100%""#),
            ("a*b", r#""a*b""#),
            ("ünïcödé", r#""ünïcödé""#),
            ("日本", r#""日本""#),
            ("1abc", r#""1abc""#),
            ("a$b", r#""a$b""#),
            ("", r#""""#),
            ("\"", r#""""""#),
            ("tab\there", "\"tab\there\""),
        ];
        for (name, want) in cases {
            assert_eq!(quote_ident(name), want, "{name:?}");
        }
    }

    #[test]
    fn qualified_quotes_each_part() {
        assert_eq!(qualified("public", "users"), "public.users");
        assert_eq!(qualified("My Schema", "select"), r#""My Schema"."select""#);
        assert_eq!(qualified("a.b", "c"), r#""a.b".c"#);
    }

    #[test]
    fn dump_patterns_are_always_quoted_and_literal() {
        assert_eq!(
            dump_pattern("public", "users").as_deref(),
            Some(r#""public"."users""#)
        );
        assert_eq!(
            dump_pattern("s", r#"a*"b%"#).as_deref(),
            Some(r#""s"."a*""b%""#)
        );
        assert_eq!(dump_pattern("s", "line\nbreak"), None);
    }

    #[test]
    fn like_metacharacters_are_escaped() {
        assert_eq!(escape_like(r"50%_off\x"), r"50\%\_off\\x");
        assert_eq!(escape_like("plain"), "plain");
    }
}
