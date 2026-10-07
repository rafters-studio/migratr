//! Text scanning for stored SQL: `CREATE TABLE` structure and identifier tokens.
//!
//! Every scan here skips string literals, quoted identifiers (with doubled-quote escapes),
//! `[...]` identifiers and `--` / `/* */` comments, so a comma, parenthesis or keyword inside
//! one of those is text, not structure. Each scan returns `None` rather than a guess when the
//! text does not have the expected shape.

use std::ops::Range;

/// `name` as a double-quoted SQL identifier.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `value` as a single-quoted SQL string literal.
pub(crate) fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The top-level structure of a `CREATE TABLE` statement.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TableBody {
    /// Byte index of the `)` closing the column-definition list.
    pub close: usize,
    /// Byte ranges of each item in the list, between the separating commas, untrimmed.
    /// Column definitions come first, then table constraints.
    pub items: Vec<Range<usize>>,
}

/// Splits a `CREATE TABLE` statement's parenthesised list into its top-level items.
/// `None` when the statement has no list or its parentheses are unbalanced.
pub(crate) fn table_body(create_sql: &str) -> Option<TableBody> {
    let mut depth = 0usize;
    let mut open: Option<usize> = None;
    let mut close: Option<usize> = None;
    let mut cuts: Vec<usize> = Vec::new();
    for token in tokens(create_sql)
        .into_iter()
        .filter(|t| t.kind == TokenKind::Punct)
    {
        let at = token.span.start;
        match token.text.as_str() {
            "(" => {
                depth += 1;
                if depth == 1 && open.is_none() {
                    open = Some(at);
                }
            }
            ")" => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && close.is_none() {
                    close = Some(at);
                }
            }
            "," if depth == 1 && close.is_none() => cuts.push(at),
            _ => {}
        }
    }

    if depth != 0 {
        return None;
    }
    let (open, close) = (open?, close?);
    let mut items = Vec::with_capacity(cuts.len() + 1);
    let mut from = open + 1;
    for cut in cuts.into_iter().chain(std::iter::once(close)) {
        items.push(from..cut);
        from = cut + 1;
    }
    Some(TableBody { close, items })
}

/// What a [`Token`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
    /// A bare word: a keyword, an unquoted identifier, or a number.
    Bare,
    /// A `"x"`, `` `x` `` or `[x]` identifier; `text` holds it unquoted.
    Quoted,
    /// One of `(`, `)`, `,` or `.`.
    Punct,
}

/// One token of SQL text, located by its byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub span: Range<usize>,
}

impl Token {
    /// Whether this is an identifier (bare or quoted) equal to `name` as SQLite compares
    /// identifiers, ASCII case-insensitively.
    pub(crate) fn names(&self, name: &str) -> bool {
        self.kind != TokenKind::Punct && self.text.eq_ignore_ascii_case(name)
    }

    /// Whether this is the bare keyword `keyword`.
    pub(crate) fn is_keyword(&self, keyword: &str) -> bool {
        self.kind == TokenKind::Bare && self.text.eq_ignore_ascii_case(keyword)
    }
}

/// The words, quoted identifiers and structural punctuation of `sql`, in order. String
/// literals, comments and operators produce no tokens.
pub(crate) fn tokens(sql: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = sql.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        // SQLite admits any non-ASCII character in a bare identifier.
        let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();
        if is_word(c) {
            let mut end = at + c.len_utf8();
            while let Some(&(i, n)) = chars.peek() {
                if !is_word(n) {
                    break;
                }
                end = i + n.len_utf8();
                chars.next();
            }
            out.push(Token {
                kind: TokenKind::Bare,
                text: sql[at..end].to_string(),
                span: at..end,
            });
            continue;
        }
        match c {
            '\'' => {
                while let Some((_, n)) = chars.next() {
                    if n == '\'' {
                        if chars.peek().map(|&(_, p)| p) == Some('\'') {
                            chars.next();
                            continue;
                        }
                        break;
                    }
                }
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut text = String::new();
                let mut end = sql.len();
                while let Some((i, n)) = chars.next() {
                    if n == close {
                        if c != '[' && chars.peek().map(|&(_, p)| p) == Some(close) {
                            chars.next();
                            text.push(close);
                            continue;
                        }
                        end = i + 1;
                        break;
                    }
                    text.push(n);
                }
                out.push(Token {
                    kind: TokenKind::Quoted,
                    text,
                    span: at..end,
                });
            }
            '-' if chars.peek().map(|&(_, p)| p) == Some('-') => {
                for (_, n) in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek().map(|&(_, p)| p) == Some('*') => {
                chars.next();
                let mut prev = '\0';
                for (_, n) in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            '(' | ')' | ',' | '.' => out.push(Token {
                kind: TokenKind::Punct,
                text: c.to_string(),
                span: at..at + 1,
            }),
            _ => {}
        }
    }
    out
}

/// The first identifier in `sql`: the column name of a column definition, or the leading
/// keyword of a table constraint.
pub(crate) fn leading_identifier(sql: &str) -> Option<String> {
    tokens(sql)
        .into_iter()
        .find(|t| t.kind != TokenKind::Punct)
        .map(|t| t.text)
}

/// Whether `sql` names `ident` as a bare word or quoted identifier.
pub(crate) fn mentions_identifier(sql: &str, ident: &str) -> bool {
    tokens(sql).iter().any(|t| t.names(ident))
}

/// Whether `item`, one comma-separated item of a CREATE TABLE body, names `ident` where it
/// refers to a column of its own table: inside parentheses, as in a CHECK, generated or
/// DEFAULT expression or a constraint's column list. A type name or constraint keyword
/// outside parentheses cannot be a column reference. The parent column list after
/// `REFERENCES` belongs to another table and is skipped.
pub(crate) fn names_own_column(item: &str, ident: &str) -> bool {
    let tokens = tokens(item);
    let mut depth = 0usize;
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token.kind == TokenKind::Punct {
            match token.text.as_str() {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                _ => {}
            }
        } else if token.is_keyword("REFERENCES") {
            // The parent table, an optional `schema.` qualifier, then its column list.
            i += 1;
            while tokens
                .get(i + 1)
                .is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".")
            {
                i += 2;
            }
            i += 1;
            if tokens
                .get(i)
                .is_some_and(|t| t.kind == TokenKind::Punct && t.text == "(")
            {
                while i < tokens.len()
                    && !(tokens[i].kind == TokenKind::Punct && tokens[i].text == ")")
                {
                    i += 1;
                }
            }
        } else if token.names(ident) && depth > 0 {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether a CREATE INDEX statement names `ident` as an indexed column or in its partial
/// WHERE expression. The index's own name and its table, before the column list, do not count.
pub(crate) fn index_names_column(sql: &str, ident: &str) -> bool {
    let tokens = tokens(sql);
    let Some(on) = tokens.iter().position(|t| t.is_keyword("ON")) else {
        return false;
    };
    // The table, with an optional `schema.` qualifier, then the column list.
    let mut after = on + 2;
    while tokens
        .get(after)
        .is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".")
    {
        after += 2;
    }
    tokens.iter().skip(after).any(|t| t.names(ident))
}

/// Whether `sql` contains `keyword` as a bare word. A quoted identifier spelled the same way
/// does not count.
pub(crate) fn has_keyword(sql: &str, keyword: &str) -> bool {
    tokens(sql).iter().any(|t| t.is_keyword(keyword))
}

/// The byte range of the table name in a `CREATE [TEMP] TABLE [IF NOT EXISTS] [schema.]name`
/// statement, including any `schema.` qualifier and quoting.
pub(crate) fn create_table_name_span(create_sql: &str) -> Option<Range<usize>> {
    let b = create_sql.as_bytes();
    let mut i = skip_ws(b, 0);
    i = skip_ws(b, match_kw(b, i, "CREATE")?);
    if let Some(j) = match_kw(b, i, "TEMPORARY").or_else(|| match_kw(b, i, "TEMP")) {
        i = skip_ws(b, j);
    }
    i = skip_ws(b, match_kw(b, i, "TABLE")?);
    if let Some(j) = match_kw(b, i, "IF") {
        let j = skip_ws(b, match_kw(b, skip_ws(b, j), "NOT")?);
        i = skip_ws(b, match_kw(b, j, "EXISTS")?);
    }

    let start = i;
    let mut end = ident_end(b, i)?;
    let after = skip_ws(b, end);
    if b.get(after) == Some(&b'.') {
        end = ident_end(b, skip_ws(b, after + 1))?;
    }
    Some(start..end)
}

/// A byte that can appear in a bare identifier. Bytes of non-ASCII characters count, since
/// SQLite admits those unquoted.
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || !c.is_ascii()
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Matches `word` case-insensitively at `i` followed by a word boundary, returning the index
/// just past it.
fn match_kw(b: &[u8], i: usize, word: &str) -> Option<usize> {
    let end = i.checked_add(word.len())?;
    if !b.get(i..end)?.eq_ignore_ascii_case(word.as_bytes()) {
        return None;
    }
    if b.get(end).is_some_and(|&c| is_ident_byte(c)) {
        return None;
    }
    Some(end)
}

/// The end (exclusive) of the identifier starting at `i`, quoted with `"`, `` ` `` or `[]`,
/// or bare.
fn ident_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        q @ (b'"' | b'`') => {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == q {
                    if b.get(j + 1) == Some(&q) {
                        j += 2;
                        continue;
                    }
                    return Some(j + 1);
                }
                j += 1;
            }
            None
        }
        b'[' => b[i..].iter().position(|&c| c == b']').map(|p| i + p + 1),
        c if is_ident_byte(c) && !c.is_ascii_digit() => {
            let mut j = i;
            while j < b.len() && is_ident_byte(b[j]) {
                j += 1;
            }
            Some(j)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_texts(sql: &str) -> Vec<&str> {
        let body = table_body(sql).expect("splits");
        body.items.iter().map(|r| sql[r.clone()].trim()).collect()
    }

    #[test]
    fn table_body_skips_commas_and_parens_that_are_not_structure() {
        let sql = "CREATE TABLE t (\n  a INTEGER,\n  b TEXT DEFAULT 'x, (y)',\n  \
                   c INTEGER GENERATED ALWAYS AS ((n + 1) * (m - 2)) STORED,\n  \
                   d TEXT, -- a comment, with a comma (and a paren\n  \
                   \"e,f\" INTEGER,\n  [g)h] TEXT,\n  PRIMARY KEY (a, b)\n) WITHOUT ROWID";
        let names: Vec<String> = item_texts(sql)
            .into_iter()
            .map(|t| leading_identifier(t).expect("name"))
            .collect();
        assert_eq!(names, ["a", "b", "c", "d", "e,f", "g)h", "PRIMARY"]);
        assert!(item_texts(sql)[2].contains("((n + 1) * (m - 2))"));
        let body = table_body(sql).expect("splits");
        assert_eq!(sql[body.close + 1..].trim(), "WITHOUT ROWID");
    }

    #[test]
    fn table_body_refuses_what_it_cannot_split() {
        assert!(table_body("CREATE TABLE t (a INTEGER").is_none());
        assert!(table_body("CREATE TABLE t").is_none());
        assert!(table_body("CREATE TABLE t (a))").is_none());
    }

    #[test]
    fn table_body_ranges_cover_the_text_between_separators() {
        let sql = "CREATE TABLE t (a, b)";
        let body = table_body(sql).expect("splits");
        assert_eq!(body.items, vec![16..17, 18..20]);
    }

    #[test]
    fn doubled_quotes_stay_inside_the_identifier() {
        let sql = "CREATE TABLE t (\"na\"\",me\" INTEGER, v TEXT)";
        let names: Vec<String> = item_texts(sql)
            .into_iter()
            .map(|t| leading_identifier(t).expect("name"))
            .collect();
        assert_eq!(names, ["na\",me", "v"]);
    }

    #[test]
    fn identifier_scans_skip_literals_and_comments() {
        assert!(!mentions_identifier(
            "SELECT 'users', 1 -- users\n /* users */",
            "users"
        ));
        assert!(mentions_identifier("SELECT * FROM \"Users\"", "users"));
        assert!(mentions_identifier("SELECT * FROM [users]", "USERS"));
        assert!(has_keyword(
            "id INTEGER PRIMARY KEY autoincrement",
            "AUTOINCREMENT"
        ));
        assert!(!has_keyword("\"autoincrement\" INTEGER", "AUTOINCREMENT"));
    }

    #[test]
    fn create_table_name_span_finds_every_spelling() {
        for (sql, name) in [
            ("CREATE TABLE t (a)", "t"),
            ("create  table \"we\"\"ird\"(a)", "\"we\"\"ird\""),
            (
                "CREATE TEMP TABLE IF NOT EXISTS main.[x y] (a)",
                "main.[x y]",
            ),
            ("CREATE TABLE café (a)", "café"),
        ] {
            let span = create_table_name_span(sql).expect(sql);
            assert_eq!(&sql[span], name);
        }
        assert!(create_table_name_span("CREATE VIEW v AS SELECT 1").is_none());
        assert!(create_table_name_span("CREATE /* c */ TABLE t (a)").is_none());
    }

    #[test]
    fn quoting_escapes_embedded_quotes() {
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }
}
