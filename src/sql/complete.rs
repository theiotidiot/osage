//! Context-aware autocomplete over the cached catalog.
//!
//! Three pieces fit together:
//!
//! * [`prefix_at`] — the partially-typed identifier ending at the cursor.
//! * [`context_at`] — what the cursor position *means* (table slot, column
//!   slot, keyword slot), resolved against the current statement only.
//! * [`candidates`] — a fuzzy-ranked candidate list drawn from the cached
//!   catalog tree.
//!
//! Context detection is a hybrid: tree-sitter supplies the structural view
//! (`relation` nodes with their alias fields, enclosing clause nodes) while a
//! hand-rolled token scan supplies robustness. Half-typed SQL rarely parses
//! cleanly, and completion has to keep working mid-keystroke, so the token
//! scan is authoritative whenever the two disagree about what is in scope.

use std::collections::HashSet;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::types::{CatalogNode, Completion, CompletionContext, CompletionKind, NodeKind};

/// Keywords offered in keyword position. Doubles as the reserved-word set used
/// to reject things like `WHERE` when looking for a table alias.
const KEYWORDS: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COLUMN",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FETCH",
    "FILTER",
    "FOREIGN",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "RETURNING",
    "RIGHT",
    "ROW",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Keywords that put the cursor straight into a table slot.
const TABLE_INTRO: &[&str] = &["FROM", "JOIN", "INTO", "UPDATE", "TABLE"];

// ---------------------------------------------------------------------------
// prefix
// ---------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The partially-typed identifier immediately before `cursor`, if any.
///
/// The maximal run of `[A-Za-z0-9_]` ending at `cursor`; empty when the
/// preceding character is anything else. Never panics: out-of-range or
/// mid-codepoint cursors are floored to the nearest char boundary.
pub fn prefix_at(text: &str, cursor: usize) -> &str {
    let mut end = cursor.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let bytes = text.as_bytes();
    let mut start = end;
    // Identifier bytes are all ASCII, so walking back byte-wise can never land
    // inside a multi-byte codepoint.
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    &text[start..end]
}

// ---------------------------------------------------------------------------
// tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokKind {
    /// Bare or quoted identifier / keyword.
    Word,
    /// String or numeric literal.
    Literal,
    /// A single punctuation character.
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    kind: TokKind,
    /// For quoted identifiers this is the *unquoted* inner text.
    text: &'a str,
    /// Byte offset just past the token in the string that was tokenized.
    end: usize,
}

impl<'a> Token<'a> {
    fn is_word(&self) -> bool {
        self.kind == TokKind::Word
    }

    fn is_punct(&self, c: char) -> bool {
        self.kind == TokKind::Punct && self.text.len() == c.len_utf8() && self.text.starts_with(c)
    }

    fn upper(&self) -> String {
        self.text.to_ascii_uppercase()
    }

    fn is_keyword(&self) -> bool {
        self.is_word() && KEYWORDS.contains(&self.upper().as_str())
    }
}

/// Lex SQL well enough to reason about identifier positions. Comments are
/// dropped; strings collapse to a single `Literal`; quoted identifiers become
/// `Word`s carrying their unquoted text.
fn tokenize(sql: &str) -> Vec<Token<'_>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // line comment
        if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // block comment
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // string literal (also handles unterminated input)
        if b == b'\'' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Token {
                kind: TokKind::Literal,
                text: &sql[start..i],
                end: i,
            });
            continue;
        }
        // quoted identifier
        if b == b'"' || b == b'`' || b == b'[' {
            let close = match b {
                b'"' => b'"',
                b'`' => b'`',
                _ => b']',
            };
            i += 1;
            let inner_start = i;
            while i < bytes.len() && bytes[i] != close {
                i += 1;
            }
            let inner_end = i;
            if i < bytes.len() {
                i += 1; // closing delimiter
            }
            out.push(Token {
                kind: TokKind::Word,
                text: &sql[inner_start..inner_end],
                end: i,
            });
            continue;
        }
        if is_ident_byte(b) || b == b'$' {
            let start = i;
            let numeric = b.is_ascii_digit();
            while i < bytes.len() && (is_ident_byte(bytes[i]) || bytes[i] == b'$') {
                i += 1;
            }
            // A leading digit means a number, not an identifier.
            if numeric {
                while i < bytes.len() && (bytes[i] == b'.' || bytes[i].is_ascii_digit()) {
                    i += 1;
                }
            }
            out.push(Token {
                kind: if numeric {
                    TokKind::Literal
                } else {
                    TokKind::Word
                },
                text: &sql[start..i],
                end: i,
            });
            continue;
        }
        // any other byte is punctuation; step a whole char so we stay on
        // boundaries with multi-byte input
        let start = i;
        i += 1;
        while i < bytes.len() && !sql.is_char_boundary(i) {
            i += 1;
        }
        out.push(Token {
            kind: TokKind::Punct,
            text: &sql[start..i],
            end: i,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// scope: tables visible from the current statement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Scoped {
    /// Table reference as written, e.g. `orders` or `main.public.orders`.
    table: String,
    alias: Option<String>,
}

impl Scoped {
    /// Dedup key: the name a qualifier would have to use to reach this table.
    fn key(&self) -> String {
        match &self.alias {
            Some(a) => a.to_ascii_lowercase(),
            None => last_segment(&self.table).to_ascii_lowercase(),
        }
    }

    fn answers_to(&self, qualifier: &str) -> bool {
        if let Some(a) = &self.alias
            && a.eq_ignore_ascii_case(qualifier)
        {
            return true;
        }
        self.table.eq_ignore_ascii_case(qualifier)
            || last_segment(&self.table).eq_ignore_ascii_case(qualifier)
    }
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Read a dotted identifier chain starting at token `i`. Returns the joined
/// name and the index just past it.
fn read_dotted(toks: &[Token<'_>], i: usize) -> Option<(String, usize)> {
    if !toks.get(i)?.is_word() {
        return None;
    }
    let mut name = toks[i].text.to_string();
    let mut j = i + 1;
    while toks.get(j).is_some_and(|t| t.is_punct('.')) {
        match toks.get(j + 1) {
            Some(t) if t.is_word() => {
                name.push('.');
                name.push_str(t.text);
                j += 2;
            }
            // trailing dot: `FROM public.` — stop, the chain is still useful
            _ => {
                j += 1;
                break;
            }
        }
    }
    Some((name, j))
}

/// Token-scan for FROM/JOIN/INTO/UPDATE relations. Works on partial input,
/// which the parse tree often does not.
fn scoped_by_scan(toks: &[Token<'_>]) -> Vec<Scoped> {
    let mut out: Vec<Scoped> = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let t = &toks[i];
        if !t.is_word() {
            i += 1;
            continue;
        }
        let kw = t.upper();
        if !matches!(kw.as_str(), "FROM" | "JOIN" | "INTO" | "UPDATE") {
            i += 1;
            continue;
        }
        // `DELETE FROM t`, `INSERT INTO t`, `SELECT ... FROM t1 a, t2 b JOIN ...`
        let mut j = i + 1;
        while let Some((table, mut next)) = read_dotted(toks, j) {
            let mut alias = None;
            if toks
                .get(next)
                .is_some_and(|t| t.is_word() && t.upper() == "AS")
            {
                if let Some(a) = toks.get(next + 1).filter(|t| t.is_word()) {
                    alias = Some(a.text.to_string());
                    next += 2;
                }
            } else if let Some(a) = toks.get(next).filter(|t| t.is_word() && !t.is_keyword()) {
                // bare alias, but not the head of another dotted chain
                if !toks.get(next + 1).is_some_and(|t| t.is_punct('.')) {
                    alias = Some(a.text.to_string());
                    next += 1;
                }
            }
            push_scoped(&mut out, Scoped { table, alias });
            // comma-separated relation list in a FROM clause
            if kw == "FROM" && toks.get(next).is_some_and(|t| t.is_punct(',')) {
                j = next + 1;
                continue;
            }
            j = next;
            break;
        }
        i = j.max(i + 1);
    }
    out
}

fn push_scoped(out: &mut Vec<Scoped>, s: Scoped) {
    if s.table.is_empty() {
        return;
    }
    let key = s.key();
    if out.iter().any(|e| e.key() == key) {
        return;
    }
    out.push(s);
}

// ---------------------------------------------------------------------------
// tree-sitter
// ---------------------------------------------------------------------------

fn parse(sql: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .ok()?;
    parser.parse(sql, None)
}

/// Walk every `relation` node in the tree, pulling out its object reference
/// and (grammar-supplied) alias field.
fn scoped_by_tree(sql: &str, tree: &tree_sitter::Tree) -> Vec<Scoped> {
    let src = sql.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "relation" {
            let mut table = None;
            for idx in 0..node.named_child_count() {
                let Some(child) = node.named_child(idx as u32) else {
                    continue;
                };
                if child.kind() == "object_reference" {
                    table = child.utf8_text(src).ok().map(|s| s.to_string());
                    break;
                }
            }
            if let Some(table) = table {
                let alias = node
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(src).ok())
                    .map(|s| s.to_string());
                push_scoped(&mut out, Scoped { table, alias });
            }
        }
        for idx in 0..node.named_child_count() {
            if let Some(child) = node.named_child(idx as u32) {
                stack.push(child);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    Column,
    Table,
}

/// Which clause encloses `offset`, according to the parse tree. Only consulted
/// when the token scan cannot classify the position on its own.
fn clause_by_tree(tree: &tree_sitter::Tree, offset: usize) -> Option<Clause> {
    let probe = offset.saturating_sub(1);
    let mut node = tree.root_node().descendant_for_byte_range(probe, offset)?;
    loop {
        match node.kind() {
            "where" | "select_expression" | "select" | "group_by" | "order_by" | "order_target"
            | "term" | "field" | "having" => return Some(Clause::Column),
            "relation" | "object_reference" | "from" => return Some(Clause::Table),
            _ => {}
        }
        node = node.parent()?;
    }
}

// ---------------------------------------------------------------------------
// context
// ---------------------------------------------------------------------------

/// Determine what the cursor position means syntactically.
///
/// `cursor` is a byte offset into `text`. Column contexts resolve table
/// aliases declared in the current statement's FROM/JOIN clauses; the table an
/// explicit qualifier resolves to is placed first in `scoped_tables` so
/// ranking can prefer it.
pub fn context_at(text: &str, cursor: usize) -> CompletionContext {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    // Scope ourselves to the statement under the cursor.
    let (sql, base) = match crate::sql::statement::statement_at(text, cursor) {
        Some(stmt) if stmt.start <= cursor => (stmt.sql, stmt.start),
        _ => (text.to_string(), 0),
    };
    // `sql` is whitespace-trimmed, so it cannot tell us what sits between the
    // last token and the cursor. Tokenize the raw run up to the cursor for
    // that, and keep the trimmed statement for scope resolution.
    let head = &text[base..cursor];
    let local = cursor.saturating_sub(base).min(sql.len());
    let toks = tokenize(&sql);
    let tree = parse(&sql);

    // Tables in scope: the parse tree first, topped up by the token scan for
    // whatever a half-written statement kept it from seeing.
    let mut scope = match &tree {
        Some(t) => scoped_by_tree(&sql, t),
        None => Vec::new(),
    };
    for s in scoped_by_scan(&toks) {
        push_scoped(&mut scope, s);
    }

    // Tokens before the cursor, minus the identifier currently being typed.
    let head_toks = tokenize(head);
    let mut ctx: Vec<&Token<'_>> = head_toks.iter().collect();
    if ctx
        .last()
        .is_some_and(|t| t.is_word() && t.end == head.len())
    {
        ctx.pop();
    }

    let column = |scope: &[Scoped]| CompletionContext::ColumnName {
        scoped_tables: scope.iter().map(|s| s.table.clone()).collect(),
    };

    let Some(last) = ctx.last().copied() else {
        // Nothing but whitespace ahead of us: only a keyword can start a
        // statement.
        return CompletionContext::Keyword;
    };

    // --- qualified position: `something.|`
    if last.is_punct('.') {
        let mut chain: Vec<&str> = Vec::new();
        let mut k = ctx.len(); // index just past the '.'
        // expect word, dot, word, dot, ...
        while let Some(w) = ctx.get(k.wrapping_sub(2)).filter(|t| t.is_word()) {
            chain.push(w.text);
            if ctx.get(k.wrapping_sub(3)).is_some_and(|t| t.is_punct('.')) {
                k -= 2;
                continue;
            }
            k -= 2;
            break;
        }
        chain.reverse();
        if chain.is_empty() {
            return CompletionContext::Unknown;
        }
        // `FROM public.|` is still a table position.
        if ctx
            .get(k.wrapping_sub(1))
            .is_some_and(|t| t.is_word() && TABLE_INTRO.contains(&t.upper().as_str()))
        {
            return CompletionContext::TableName;
        }
        let qualifier = *chain.last().unwrap();
        if let Some(pos) = scope.iter().position(|s| s.answers_to(qualifier)) {
            let mut tables: Vec<String> = Vec::with_capacity(scope.len());
            tables.push(scope[pos].table.clone());
            for (i, s) in scope.iter().enumerate() {
                if i != pos {
                    tables.push(s.table.clone());
                }
            }
            return CompletionContext::ColumnName {
                scoped_tables: tables,
            };
        }
        // Unresolved qualifier: treat the chain itself as the scope so
        // `public.orders.|` still finds orders' columns.
        return CompletionContext::ColumnName {
            scoped_tables: vec![chain.join(".")],
        };
    }

    // --- immediately after a table-introducing keyword
    if last.is_word() && TABLE_INTRO.contains(&last.upper().as_str()) {
        return CompletionContext::TableName;
    }

    // --- otherwise: nearest enclosing clause keyword
    let mut clause = None;
    for t in ctx.iter().rev() {
        if !t.is_word() {
            continue;
        }
        match t.upper().as_str() {
            "SELECT" | "WHERE" | "HAVING" | "ON" | "SET" | "GROUP" | "ORDER" | "PARTITION"
            | "USING" | "RETURNING" => {
                clause = Some(Clause::Column);
                break;
            }
            "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" => {
                clause = Some(Clause::Table);
                break;
            }
            // `BY`, `AND`, `NOT`, identifiers, … keep looking
            _ => {}
        }
    }

    match clause {
        Some(Clause::Column) => column(&scope),
        Some(Clause::Table) => {
            // `FROM a, |` is another table slot; `FROM a |` is the alias slot,
            // which we cannot usefully complete.
            if last.is_punct(',') {
                CompletionContext::TableName
            } else {
                CompletionContext::Unknown
            }
        }
        None => match tree.as_ref().and_then(|t| clause_by_tree(t, local)) {
            Some(Clause::Column) => column(&scope),
            Some(Clause::Table) => CompletionContext::TableName,
            None => CompletionContext::Unknown,
        },
    }
}

// ---------------------------------------------------------------------------
// candidates
// ---------------------------------------------------------------------------

/// A relation (table or view) found in the cached catalog tree.
struct Relation<'a> {
    node: &'a CatalogNode,
    catalog: Option<&'a str>,
    schema: Option<&'a str>,
}

impl<'a> Relation<'a> {
    fn name(&self) -> &'a str {
        &self.node.name
    }

    fn qualified(&self) -> String {
        [self.catalog, self.schema, Some(self.name())]
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .join(".")
    }

    fn schema_qualified(&self) -> Option<String> {
        self.schema.map(|s| format!("{s}.{}", self.name()))
    }

    fn detail(&self) -> &'static str {
        match self.node.kind {
            NodeKind::View => "view",
            _ => "table",
        }
    }

    /// Does this relation satisfy a scope entry written as `scoped`?
    fn matches(&self, scoped: &str) -> bool {
        let want = scoped.to_ascii_lowercase();
        let full = self.qualified().to_ascii_lowercase();
        if self.name().eq_ignore_ascii_case(&want) || full == want {
            return true;
        }
        if self
            .schema_qualified()
            .is_some_and(|q| q.eq_ignore_ascii_case(&want))
        {
            return true;
        }
        full.ends_with(&format!(".{want}"))
    }
}

/// Depth-first walk of the cached tree, tracking the innermost catalog and
/// schema names. Whatever is not loaded simply is not there.
fn collect_relations<'a>(
    node: &'a CatalogNode,
    catalog: Option<&'a str>,
    schema: Option<&'a str>,
    out: &mut Vec<Relation<'a>>,
) {
    match &node.kind {
        NodeKind::Catalog => {
            for child in &node.children {
                collect_relations(child, Some(&node.name), None, out);
            }
        }
        NodeKind::Schema => {
            for child in &node.children {
                collect_relations(child, catalog, Some(&node.name), out);
            }
        }
        NodeKind::Table | NodeKind::View => out.push(Relation {
            node,
            catalog,
            schema,
        }),
        NodeKind::Column { .. } => {}
    }
}

fn columns_of<'a>(rel: &Relation<'a>) -> impl Iterator<Item = (&'a str, &'a str)> {
    rel.node.children.iter().filter_map(|c| match &c.kind {
        NodeKind::Column { data_type, .. } => Some((c.name.as_str(), data_type.as_str())),
        _ => None,
    })
}

/// A pool entry plus its position in the default (no-prefix) ordering.
struct Candidate {
    completion: Completion,
    rank: usize,
}

#[derive(Default)]
struct Pool {
    items: Vec<Candidate>,
    seen: HashSet<(u8, String)>,
}

impl Pool {
    fn push(&mut self, text: impl Into<String>, detail: impl Into<String>, kind: CompletionKind) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        let tag = match kind {
            CompletionKind::Keyword => 0u8,
            CompletionKind::Table => 1,
            CompletionKind::Column => 2,
            CompletionKind::Schema => 3,
        };
        if !self.seen.insert((tag, text.to_ascii_lowercase())) {
            return;
        }
        let rank = self.items.len();
        self.items.push(Candidate {
            completion: Completion {
                text,
                detail: detail.into(),
                kind,
            },
            rank,
        });
    }
}

fn push_tables(pool: &mut Pool, rels: &[Relation<'_>]) {
    // A bare name is ambiguous when two different schemas expose it.
    for rel in rels {
        pool.push(rel.name(), rel.detail(), CompletionKind::Table);
    }
    for rel in rels {
        if let Some(q) = rel.schema_qualified() {
            let ambiguous = rels
                .iter()
                .filter(|o| o.schema_qualified().as_deref() == Some(q.as_str()))
                .count()
                > 1;
            if !ambiguous {
                pool.push(q, rel.detail(), CompletionKind::Table);
            }
        }
    }
    let mut schemas: Vec<&str> = rels.iter().filter_map(|r| r.schema).collect();
    schemas.sort_unstable();
    schemas.dedup();
    for s in schemas {
        pool.push(s, "schema", CompletionKind::Schema);
    }
}

fn push_columns(pool: &mut Pool, rels: &[Relation<'_>], scoped_tables: &[String]) {
    // Preserve caller-supplied scope order: the alias-resolved table comes
    // first, so its columns lead the list.
    let mut matched: Vec<&Relation<'_>> = Vec::new();
    for scoped in scoped_tables {
        for rel in rels {
            if rel.matches(scoped) && !matched.iter().any(|m| std::ptr::eq(*m, rel)) {
                matched.push(rel);
            }
        }
    }
    let chosen: Vec<&Relation<'_>> = if matched.is_empty() {
        rels.iter().collect()
    } else {
        matched
    };
    for rel in chosen {
        for (name, data_type) in columns_of(rel) {
            pool.push(name, data_type, CompletionKind::Column);
        }
    }
}

fn push_keywords(pool: &mut Pool) {
    for kw in KEYWORDS {
        pool.push(*kw, "keyword", CompletionKind::Keyword);
    }
}

/// Rank candidates drawn from the cached catalog against `prefix`.
///
/// CONTRACT: reads only `catalogs`; never triggers a catalog fetch.
pub fn candidates(
    catalogs: &[&CatalogNode],
    context: &CompletionContext,
    prefix: &str,
    limit: usize,
) -> Vec<Completion> {
    if limit == 0 {
        return Vec::new();
    }
    let mut rels: Vec<Relation<'_>> = Vec::new();
    for root in catalogs {
        collect_relations(root, None, None, &mut rels);
    }

    let mut pool = Pool::default();
    match context {
        CompletionContext::TableName => push_tables(&mut pool, &rels),
        CompletionContext::ColumnName { scoped_tables } => {
            push_columns(&mut pool, &rels, scoped_tables)
        }
        CompletionContext::Keyword => push_keywords(&mut pool),
        CompletionContext::Unknown => {
            push_columns(&mut pool, &rels, &[]);
            push_tables(&mut pool, &rels);
            push_keywords(&mut pool);
        }
    }

    let mut scored: Vec<(u32, &Candidate)> = if prefix.is_empty() {
        // No filtering: keep the pool's construction order.
        pool.items.iter().map(|c| (0u32, c)).collect()
    } else {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(prefix, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        pool.items
            .iter()
            .filter_map(|c| {
                let hay = Utf32Str::new(&c.completion.text, &mut buf);
                pattern.score(hay, &mut matcher).map(|s| (s, c))
            })
            .collect()
    };

    // Highest score wins; ties fall back to pool order, then alphabetical, so
    // the list is stable across runs.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.rank.cmp(&b.1.rank))
            .then_with(|| {
                a.1.completion
                    .text
                    .to_ascii_lowercase()
                    .cmp(&b.1.completion.text.to_ascii_lowercase())
            })
            .then_with(|| a.1.completion.text.cmp(&b.1.completion.text))
    });
    scored.truncate(limit);
    scored
        .into_iter()
        .map(|(_, c)| c.completion.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LoadState;

    fn col(name: &str, ty: &str) -> CatalogNode {
        CatalogNode::new(
            NodeKind::Column {
                data_type: ty.into(),
                nullable: true,
            },
            name,
        )
    }

    fn with_children(mut node: CatalogNode, children: Vec<CatalogNode>) -> CatalogNode {
        node.children = children;
        node.load_state = LoadState::Loaded;
        node
    }

    /// root → catalog `main` → schema `public` → orders / customers / v_recent
    fn fixture() -> CatalogNode {
        let orders = with_children(
            CatalogNode::new(NodeKind::Table, "orders"),
            vec![
                col("order_id", "int4"),
                col("customer_id", "int4"),
                col("total_amount", "numeric"),
                col("ordered_at", "timestamptz"),
            ],
        );
        let customers = with_children(
            CatalogNode::new(NodeKind::Table, "customers"),
            vec![
                col("customer_id", "int4"),
                col("email", "text"),
                col("name", "text"),
            ],
        );
        let view = with_children(
            CatalogNode::new(NodeKind::View, "v_recent_orders"),
            vec![col("order_id", "int4")],
        );
        let public = with_children(
            CatalogNode::new(NodeKind::Schema, "public"),
            vec![orders, customers, view],
        );
        let main = with_children(CatalogNode::new(NodeKind::Catalog, "main"), vec![public]);
        with_children(CatalogNode::new(NodeKind::Catalog, "profile"), vec![main])
    }

    fn texts(items: &[Completion]) -> Vec<&str> {
        items.iter().map(|c| c.text.as_str()).collect()
    }

    // -- prefix_at ------------------------------------------------------

    #[test]
    fn prefix_empty_input() {
        assert_eq!(prefix_at("", 0), "");
    }

    #[test]
    fn prefix_mid_identifier() {
        let t = "select orders";
        assert_eq!(prefix_at(t, t.len()), "orders");
        assert_eq!(prefix_at(t, 10), "ord");
    }

    #[test]
    fn prefix_after_space_is_empty() {
        assert_eq!(prefix_at("select ", 7), "");
    }

    #[test]
    fn prefix_after_dot_is_empty() {
        assert_eq!(prefix_at("o.", 2), "");
        assert_eq!(prefix_at("o.na", 4), "na");
    }

    #[test]
    fn prefix_underscores_and_digits() {
        let t = "where order_id2";
        assert_eq!(prefix_at(t, t.len()), "order_id2");
    }

    #[test]
    fn prefix_multibyte_never_panics() {
        let t = "select café";
        // cursor at the end: `é` is not an identifier byte
        assert_eq!(prefix_at(t, t.len()), "");
        // cursor inside the `é` codepoint floors to the boundary
        assert_eq!(prefix_at(t, t.len() - 1), "caf");
        // cursor past the end is clamped
        assert_eq!(prefix_at(t, 9999), "");
        assert_eq!(prefix_at("héllo x", 3), "");
    }

    #[test]
    fn prefix_is_a_subslice() {
        let t = "from orders";
        let p = prefix_at(t, t.len());
        assert!(t.as_ptr() as usize <= p.as_ptr() as usize);
    }

    // -- context_at -----------------------------------------------------

    #[test]
    fn empty_buffer_is_keyword() {
        assert_eq!(context_at("", 0), CompletionContext::Keyword);
        assert_eq!(context_at("   ", 3), CompletionContext::Keyword);
    }

    #[test]
    fn start_of_statement_is_keyword() {
        let t = "sel";
        assert_eq!(context_at(t, 3), CompletionContext::Keyword);
    }

    #[test]
    fn after_from_is_table() {
        assert_eq!(
            context_at("select * from ", 14),
            CompletionContext::TableName
        );
        assert_eq!(
            context_at("select * from ord", 17),
            CompletionContext::TableName
        );
    }

    #[test]
    fn after_join_into_update_table_is_table() {
        assert_eq!(
            context_at("select * from a join ", 21),
            CompletionContext::TableName
        );
        assert_eq!(context_at("insert into ", 12), CompletionContext::TableName);
        assert_eq!(context_at("update ", 7), CompletionContext::TableName);
        assert_eq!(
            context_at("create table ", 13),
            CompletionContext::TableName
        );
    }

    #[test]
    fn comma_in_from_list_is_table() {
        let t = "select * from orders, ";
        assert_eq!(context_at(t, t.len()), CompletionContext::TableName);
    }

    #[test]
    fn qualified_schema_after_from_is_table() {
        let t = "select * from public.";
        assert_eq!(context_at(t, t.len()), CompletionContext::TableName);
    }

    #[test]
    fn select_list_scopes_to_from_table() {
        // cursor sits right after `select `, statement is still half-written
        let t = "select  from orders o";
        let cursor = 7;
        assert_eq!(
            context_at(t, cursor),
            CompletionContext::ColumnName {
                scoped_tables: vec!["orders".into()]
            }
        );
    }

    #[test]
    fn alias_qualifier_puts_its_table_first() {
        let t = "select o. from orders o, customers c";
        let cursor = 9; // right after `o.`
        match context_at(t, cursor) {
            CompletionContext::ColumnName { scoped_tables } => {
                assert_eq!(scoped_tables.first().map(String::as_str), Some("orders"));
                assert!(scoped_tables.iter().any(|t| t == "customers"));
            }
            other => panic!("expected ColumnName, got {other:?}"),
        }
        let t2 = "select c. from orders o, customers c";
        match context_at(t2, 9) {
            CompletionContext::ColumnName { scoped_tables } => {
                assert_eq!(scoped_tables.first().map(String::as_str), Some("customers"));
            }
            other => panic!("expected ColumnName, got {other:?}"),
        }
    }

    #[test]
    fn alias_with_as_keyword() {
        let t = "select x. from orders as x";
        match context_at(t, 9) {
            CompletionContext::ColumnName { scoped_tables } => {
                assert_eq!(scoped_tables.first().map(String::as_str), Some("orders"));
            }
            other => panic!("expected ColumnName, got {other:?}"),
        }
    }

    #[test]
    fn three_part_name_with_alias() {
        let t = "select z.ord from main.public.orders z";
        match context_at(t, 12) {
            CompletionContext::ColumnName { scoped_tables } => {
                assert_eq!(
                    scoped_tables.first().map(String::as_str),
                    Some("main.public.orders")
                );
            }
            other => panic!("expected ColumnName, got {other:?}"),
        }
    }

    #[test]
    fn bare_table_without_alias_is_in_scope() {
        let t = "select * from orders where ";
        match context_at(t, t.len()) {
            CompletionContext::ColumnName { scoped_tables } => {
                assert_eq!(scoped_tables, vec!["orders".to_string()]);
            }
            other => panic!("expected ColumnName, got {other:?}"),
        }
    }

    #[test]
    fn clause_keywords_give_column_context() {
        for t in [
            "select * from orders where ",
            "select * from orders group by ",
            "select * from orders order by ",
            "select count(*) from orders group by x having ",
            "select * from orders o join customers c on ",
        ] {
            match context_at(t, t.len()) {
                CompletionContext::ColumnName { scoped_tables } => {
                    assert!(
                        scoped_tables.iter().any(|s| s == "orders"),
                        "{t:?} lost orders from scope: {scoped_tables:?}"
                    );
                }
                other => panic!("{t:?} expected ColumnName, got {other:?}"),
            }
        }
    }

    #[test]
    fn join_table_is_in_scope_too() {
        let t = "select  from orders o left join customers c on o.id = c.id";
        match context_at(t, 7) {
            CompletionContext::ColumnName { scoped_tables } => {
                assert!(scoped_tables.iter().any(|s| s == "orders"));
                assert!(scoped_tables.iter().any(|s| s == "customers"));
            }
            other => panic!("expected ColumnName, got {other:?}"),
        }
    }

    #[test]
    fn scope_is_limited_to_the_current_statement() {
        let t = "select * from customers; select  from orders";
        let cursor = 32; // inside the second statement's select list
        match context_at(t, cursor) {
            CompletionContext::ColumnName { scoped_tables } => {
                assert_eq!(scoped_tables, vec!["orders".to_string()]);
            }
            other => panic!("expected ColumnName, got {other:?}"),
        }
    }

    #[test]
    fn context_never_panics_on_odd_input() {
        for (t, c) in [
            ("select 'unterminated", 20usize),
            ("-- just a comment", 17),
            ("/* open", 7),
            ("naïve.", 7),
            ("select * from t", 9999),
            ("((((", 4),
        ] {
            let _ = context_at(t, c);
        }
    }

    // -- candidates -----------------------------------------------------

    #[test]
    fn table_context_offers_tables_views_and_schemas() {
        let root = fixture();
        let items = candidates(&[&root], &CompletionContext::TableName, "", 50);
        let t = texts(&items);
        assert!(t.contains(&"orders"));
        assert!(t.contains(&"customers"));
        assert!(t.contains(&"v_recent_orders"));
        assert!(t.contains(&"public.orders"));
        assert!(t.contains(&"public"));
        let schema = items.iter().find(|c| c.text == "public").unwrap();
        assert_eq!(schema.kind, CompletionKind::Schema);
        assert_eq!(schema.detail, "schema");
        let view = items.iter().find(|c| c.text == "v_recent_orders").unwrap();
        assert_eq!(view.detail, "view");
        assert_eq!(
            items.iter().find(|c| c.text == "orders").unwrap().detail,
            "table"
        );
    }

    #[test]
    fn exact_prefix_match_ranks_first() {
        let root = fixture();
        let items = candidates(&[&root], &CompletionContext::TableName, "orders", 10);
        assert_eq!(items.first().map(|c| c.text.as_str()), Some("orders"));

        let ctx = CompletionContext::ColumnName {
            scoped_tables: vec!["orders".into()],
        };
        let items = candidates(&[&root], &ctx, "order_id", 10);
        assert_eq!(items.first().map(|c| c.text.as_str()), Some("order_id"));

        let items = candidates(&[&root], &CompletionContext::Keyword, "select", 10);
        assert_eq!(items.first().map(|c| c.text.as_str()), Some("SELECT"));
    }

    #[test]
    fn column_detail_is_the_data_type() {
        let root = fixture();
        let ctx = CompletionContext::ColumnName {
            scoped_tables: vec!["orders".into()],
        };
        let items = candidates(&[&root], &ctx, "", 50);
        let c = items.iter().find(|c| c.text == "total_amount").unwrap();
        assert_eq!(c.detail, "numeric");
        assert_eq!(c.kind, CompletionKind::Column);
        assert_eq!(
            items
                .iter()
                .find(|c| c.text == "ordered_at")
                .unwrap()
                .detail,
            "timestamptz"
        );
        // scoped to orders only
        assert!(!texts(&items).contains(&"email"));
    }

    #[test]
    fn scoped_tables_match_qualified_suffixes() {
        let root = fixture();
        let ctx = CompletionContext::ColumnName {
            scoped_tables: vec!["public.customers".into()],
        };
        let items = candidates(&[&root], &ctx, "", 50);
        assert!(texts(&items).contains(&"email"));
        assert!(!texts(&items).contains(&"total_amount"));
    }

    #[test]
    fn scope_order_leads_the_default_list() {
        let root = fixture();
        let ctx = CompletionContext::ColumnName {
            scoped_tables: vec!["customers".into(), "orders".into()],
        };
        let items = candidates(&[&root], &ctx, "", 50);
        let t = texts(&items);
        let email = t.iter().position(|x| *x == "email").unwrap();
        let total = t.iter().position(|x| *x == "total_amount").unwrap();
        assert!(email < total, "customers' columns should lead: {t:?}");
    }

    #[test]
    fn unmatched_scope_falls_back_to_all_columns() {
        let root = fixture();
        let ctx = CompletionContext::ColumnName {
            scoped_tables: vec!["nope".into()],
        };
        let items = candidates(&[&root], &ctx, "", 50);
        let t = texts(&items);
        assert!(t.contains(&"email"));
        assert!(t.contains(&"total_amount"));

        let empty = CompletionContext::ColumnName {
            scoped_tables: vec![],
        };
        assert!(!candidates(&[&root], &empty, "", 50).is_empty());
    }

    #[test]
    fn empty_prefix_returns_a_sensible_pool() {
        let root = fixture();
        for ctx in [
            CompletionContext::TableName,
            CompletionContext::ColumnName {
                scoped_tables: vec!["orders".into()],
            },
            CompletionContext::Keyword,
            CompletionContext::Unknown,
        ] {
            let items = candidates(&[&root], &ctx, "", 100);
            assert!(!items.is_empty(), "{ctx:?} produced nothing");
        }
        // Unknown blends all three kinds.
        let items = candidates(&[&root], &CompletionContext::Unknown, "", 500);
        assert!(items.iter().any(|c| c.kind == CompletionKind::Column));
        assert!(items.iter().any(|c| c.kind == CompletionKind::Table));
        assert!(items.iter().any(|c| c.kind == CompletionKind::Keyword));
    }

    #[test]
    fn limit_is_respected() {
        let root = fixture();
        assert_eq!(
            candidates(&[&root], &CompletionContext::Unknown, "", 3).len(),
            3
        );
        assert_eq!(
            candidates(&[&root], &CompletionContext::Keyword, "e", 5).len(),
            5
        );
        assert!(candidates(&[&root], &CompletionContext::Unknown, "", 0).is_empty());
    }

    #[test]
    fn results_are_deterministic() {
        let root = fixture();
        let a = candidates(&[&root], &CompletionContext::Unknown, "or", 20);
        let b = candidates(&[&root], &CompletionContext::Unknown, "or", 20);
        assert_eq!(a, b);
    }

    #[test]
    fn unloaded_levels_are_simply_absent() {
        // A catalog whose schemas were never fetched yields nothing, and no fetch.
        let root = CatalogNode::new(NodeKind::Catalog, "profile");
        assert!(candidates(&[&root], &CompletionContext::TableName, "", 10).is_empty());
        // Keywords still work with an empty catalog.
        assert!(!candidates(&[&root], &CompletionContext::Keyword, "", 10).is_empty());
    }

    #[test]
    fn multiple_profiles_are_merged() {
        let a = fixture();
        let mut b = fixture();
        b.name = "other".into();
        b.children[0].name = "warehouse".into();
        b.children[0].children[0].name = "analytics".into();
        b.children[0].children[0].children[0].name = "events".into();
        let items = candidates(&[&a, &b], &CompletionContext::TableName, "", 100);
        let t = texts(&items);
        assert!(t.contains(&"orders"));
        assert!(t.contains(&"events"));
        assert!(t.contains(&"analytics"));
    }

    #[test]
    fn end_to_end_alias_completion() {
        let root = fixture();
        let text = "select o.tot from orders o";
        let cursor = 12; // after `o.tot`
        assert_eq!(prefix_at(text, cursor), "tot");
        let ctx = context_at(text, cursor);
        let items = candidates(&[&root], &ctx, prefix_at(text, cursor), 10);
        assert_eq!(items.first().map(|c| c.text.as_str()), Some("total_amount"));
        assert_eq!(items[0].detail, "numeric");
    }
}
