//! Incremental tree-sitter syntax highlighting for the editor buffer.
//!
//! The grammar is `tree-sitter-sequel`, whose node kinds are mapped onto the
//! small [`HighlightClass`] palette the editor renders. Spans are emitted only
//! for leaves of the parse tree, so they can never nest or overlap.

use std::ops::Range;

use tree_sitter::{InputEdit, Language, Node, Parser, Point, Tree};

use crate::types::{HighlightClass, HighlightedLine};

/// Owns a tree-sitter parser plus the last parse tree so edits reparse
/// incrementally rather than from scratch.
pub struct Highlighter {
    parser: Parser,
    /// Parse tree of `source`, when the last parse succeeded.
    tree: Option<Tree>,
    /// The exact text `tree` was built from. Diffed against the incoming text
    /// to synthesise the `InputEdit` that makes incremental reparsing legal.
    source: String,
}

impl Highlighter {
    pub fn new() -> Result<Self, String> {
        let mut parser = Parser::new();
        let language: Language = tree_sitter_sequel::LANGUAGE.into();
        parser
            .set_language(&language)
            .map_err(|e| format!("failed to load SQL grammar: {e}"))?;
        Ok(Self {
            parser,
            tree: None,
            source: String::new(),
        })
    }

    /// Reparse `text`, reusing the previous tree when possible, and return one
    /// entry per line of `text` describing that line's spans in byte offsets
    /// relative to the line start.
    pub fn highlight(&mut self, text: &str) -> Vec<HighlightedLine> {
        let tree = self.reparse(text);
        let spans = match tree {
            Some(tree) => collect_spans(&tree, text),
            None => Vec::new(),
        };
        distribute(text, &spans)
    }

    /// Run the parser, feeding it the old tree when we can describe the change
    /// as a single edit. Returns the fresh tree, or `None` if parsing failed.
    fn reparse(&mut self, text: &str) -> Option<Tree> {
        let old_tree = match self.tree.take() {
            None => None,
            Some(mut old) => {
                if self.source == text {
                    Some(old)
                } else if let Some(edit) = compute_edit(&self.source, text) {
                    old.edit(&edit);
                    Some(old)
                } else {
                    // Diff looked unsafe — correctness beats incrementality.
                    None
                }
            }
        };

        let new_tree = self.parser.parse(text, old_tree.as_ref());
        match new_tree {
            Some(tree) => {
                self.tree = Some(tree.clone());
                self.source.clear();
                self.source.push_str(text);
                Some(tree)
            }
            None => {
                // Parser bailed (timeout/cancellation). Drop state so the next
                // call starts clean rather than editing a stale tree.
                self.tree = None;
                self.source.clear();
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// incremental edit synthesis
// ---------------------------------------------------------------------------

/// Describe `old` → `new` as one contiguous replacement, derived from the
/// common prefix and suffix. Returns `None` when the offsets can't be trusted.
fn compute_edit(old: &str, new: &str) -> Option<InputEdit> {
    if old == new {
        return None;
    }
    let ob = old.as_bytes();
    let nb = new.as_bytes();

    // Common prefix, backed off to a boundary valid in both strings.
    let max_prefix = ob.len().min(nb.len());
    let mut prefix = 0usize;
    while prefix < max_prefix && ob[prefix] == nb[prefix] {
        prefix += 1;
    }
    while prefix > 0 && !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    // Common suffix, not allowed to run back past the prefix.
    let max_suffix = (ob.len() - prefix).min(nb.len() - prefix);
    let mut suffix = 0usize;
    while suffix < max_suffix && ob[ob.len() - 1 - suffix] == nb[nb.len() - 1 - suffix] {
        suffix += 1;
    }
    let mut old_end = ob.len() - suffix;
    let mut new_end = nb.len() - suffix;
    while (old_end < ob.len() || new_end < nb.len())
        && !(old.is_char_boundary(old_end) && new.is_char_boundary(new_end))
    {
        old_end += 1;
        new_end += 1;
    }

    if old_end > ob.len() || new_end > nb.len() || old_end < prefix || new_end < prefix {
        return None;
    }
    if !old.is_char_boundary(prefix)
        || !new.is_char_boundary(prefix)
        || !old.is_char_boundary(old_end)
        || !new.is_char_boundary(new_end)
    {
        return None;
    }

    Some(InputEdit {
        start_byte: prefix,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(old, prefix),
        old_end_position: point_at(old, old_end),
        new_end_position: point_at(new, new_end),
    })
}

/// tree-sitter `Point`: row is the 0-based line, column is a *byte* offset
/// within that line.
fn point_at(text: &str, byte: usize) -> Point {
    let head = &text.as_bytes()[..byte.min(text.len())];
    let row = head.iter().filter(|b| **b == b'\n').count();
    let column = match head.iter().rposition(|b| *b == b'\n') {
        Some(nl) => head.len() - nl - 1,
        None => head.len(),
    };
    Point { row, column }
}

// ---------------------------------------------------------------------------
// tree walk
// ---------------------------------------------------------------------------

/// Depth-first walk emitting one span per parse-tree leaf, in source order.
fn collect_spans(tree: &Tree, text: &str) -> Vec<(Range<usize>, HighlightClass)> {
    let mut out: Vec<(Range<usize>, HighlightClass)> = Vec::new();
    let mut cursor = tree.walk();
    let limit = text.len();
    loop {
        let node = cursor.node();
        if node.child_count() == 0 {
            let start = node.start_byte().min(limit);
            let end = node.end_byte().min(limit);
            if start < end
                && !node.is_missing()
                && out.last().map(|(r, _)| r.end <= start).unwrap_or(true)
                && let Some(class) = classify(&node, &text[start..end])
            {
                out.push((start..end, class));
            }
        } else if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}

/// Map a leaf node onto a highlight class. `text` is the node's own source
/// slice, used to tell numeric literals from string literals (the grammar's
/// own highlight queries do the same with regex predicates).
fn classify(node: &Node, text: &str) -> Option<HighlightClass> {
    let kind = node.kind();

    if node.is_error() {
        return None;
    }

    // The sequel grammar spells every SQL keyword as its own `keyword_*` node.
    if kind.starts_with("keyword") {
        return Some(if KEYWORD_FUNCTIONS.contains(&kind) {
            HighlightClass::Function
        } else {
            HighlightClass::Keyword
        });
    }

    match kind {
        "comment" | "marginalia" => Some(HighlightClass::Comment),
        "literal" => Some(literal_class(text)),
        "dollar_quote" => Some(HighlightClass::String),
        "identifier" => Some(if is_function_name(node) {
            HighlightClass::Function
        } else {
            HighlightClass::Identifier
        }),
        // Non-leaf in practice, but classify defensively so a degenerate parse
        // still colours sensibly.
        "object_reference" | "column" | "term" | "field" | "relation" | "all_fields" => {
            Some(HighlightClass::Identifier)
        }
        "parameter" => Some(HighlightClass::Identifier),
        "op_other" | "op_unary_other" | "bang" => Some(HighlightClass::Operator),
        // Bare type nodes the grammar exposes without a `keyword_` prefix.
        k if TYPE_LEAVES.contains(&k) => Some(HighlightClass::Keyword),
        _ => {
            if node.is_named() {
                None
            } else if kind.starts_with(|c: char| c.is_ascii_alphabetic()) {
                // Anonymous alphabetic token — still a keyword to the reader.
                Some(HighlightClass::Keyword)
            } else {
                // `(`, `)`, `,`, `.`, `;`, `+`, `=`, `<>` …
                Some(HighlightClass::Operator)
            }
        }
    }
}

/// `(invocation (object_reference name: (identifier)))` — the grammar's own
/// rule for a function call name.
fn is_function_name(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "invocation" => true,
        "object_reference" => parent
            .parent()
            .map(|gp| gp.kind() == "invocation")
            .unwrap_or(false),
        _ => false,
    }
}

/// The grammar folds strings, numbers and booleans into one `literal` node.
fn literal_class(text: &str) -> HighlightClass {
    let first = text.as_bytes().first().copied();
    if matches!(first, Some(b'\'') | Some(b'"') | Some(b'`') | Some(b'$')) {
        return HighlightClass::String;
    }
    if is_numeric(text) {
        HighlightClass::Number
    } else {
        HighlightClass::String
    }
}

fn is_numeric(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if body.is_empty() {
        return false;
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_exp = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot && !seen_exp => seen_dot = true,
            'e' | 'E' if seen_digit && !seen_exp => {
                seen_exp = true;
                if matches!(chars.peek(), Some('+') | Some('-')) {
                    chars.next();
                }
                match chars.peek() {
                    Some(d) if d.is_ascii_digit() => {}
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    seen_digit
}

/// Keywords the grammar's own highlight query treats as function calls.
const KEYWORD_FUNCTIONS: &[&str] = &[
    "keyword_cast",
    "keyword_gist",
    "keyword_btree",
    "keyword_hash",
    "keyword_spgist",
    "keyword_gin",
    "keyword_brin",
    "keyword_array",
    "keyword_object_id",
];

/// Type nodes that can appear as leaves without a `keyword_` prefix.
const TYPE_LEAVES: &[&str] = &[
    "double",
    "int",
    "bigint",
    "smallint",
    "tinyint",
    "mediumint",
    "float",
    "decimal",
    "numeric",
    "char",
    "nchar",
    "varchar",
    "nvarchar",
    "binary",
    "varbinary",
    "bit",
    "time",
    "timestamp",
    "datetimeoffset",
    "interval",
    "direction",
];

// ---------------------------------------------------------------------------
// span → per-line projection
// ---------------------------------------------------------------------------

/// Project absolute byte spans onto per-line, line-relative spans. Returns
/// exactly one entry per line of `text` (`text.split('\n')`), with spans that
/// straddle a newline split across the lines they cover.
fn distribute(text: &str, spans: &[(Range<usize>, HighlightClass)]) -> Vec<HighlightedLine> {
    // (start, end) byte range of each line, newline excluded.
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0usize;
    for line in text.split('\n') {
        lines.push((offset, offset + line.len()));
        offset += line.len() + 1;
    }

    let mut out: Vec<HighlightedLine> = vec![Vec::new(); lines.len()];
    let mut line_idx = 0usize;
    for (range, class) in spans {
        // Advance to the first line this span can touch. Spans arrive sorted,
        // so this walk is linear overall.
        while line_idx + 1 < lines.len() && lines[line_idx].1 < range.start {
            line_idx += 1;
        }
        let mut i = line_idx;
        while i < lines.len() && lines[i].0 < range.end {
            let (ls, le) = lines[i];
            let start = range.start.max(ls);
            let end = range.end.min(le);
            if start < end {
                out[i].push((start - ls..end - ls, *class));
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_of(text: &str) -> Vec<HighlightedLine> {
        let mut h = Highlighter::new().expect("grammar loads");
        h.highlight(text)
    }

    /// Every line's spans must be sorted and non-overlapping.
    fn assert_well_formed(lines: &[HighlightedLine]) {
        for line in lines {
            let mut prev_end = 0usize;
            for (range, _) in line {
                assert!(range.start < range.end, "empty span {range:?}");
                assert!(range.start >= prev_end, "overlapping span {range:?}");
                prev_end = range.end;
            }
        }
    }

    fn find(line: &HighlightedLine, class: HighlightClass) -> Option<Range<usize>> {
        line.iter()
            .find(|(_, c)| *c == class)
            .map(|(r, _)| r.clone())
    }

    #[test]
    fn grammar_loads() {
        assert!(Highlighter::new().is_ok());
    }

    #[test]
    fn keyword_and_comment_offsets() {
        let text = "SELECT id FROM t -- note";
        let lines = spans_of(text);
        assert_eq!(lines.len(), 1);
        assert_well_formed(&lines);

        // `SELECT` is the first keyword, at 0..6.
        assert_eq!(find(&lines[0], HighlightClass::Keyword), Some(0..6));
        // `FROM` at 10..14 is also a keyword.
        assert!(
            lines[0].contains(&(10..14, HighlightClass::Keyword)),
            "expected FROM keyword span, got {:?}",
            lines[0]
        );
        // `-- note` starts at byte 17 and runs to the end of the line.
        assert_eq!(
            find(&lines[0], HighlightClass::Comment),
            Some(17..text.len())
        );
        // `id` is an identifier.
        assert!(
            lines[0].contains(&(7..9, HighlightClass::Identifier)),
            "expected id identifier span, got {:?}",
            lines[0]
        );
    }

    #[test]
    fn one_entry_per_line() {
        let text = "SELECT 1\nFROM t\n";
        let lines = spans_of(text);
        // Trailing newline yields a final empty line.
        assert_eq!(lines.len(), 3);
        assert!(lines[2].is_empty());
        assert_well_formed(&lines);

        assert_eq!(spans_of("").len(), 1);
        assert_eq!(spans_of("\n\n\n").len(), 4);
    }

    #[test]
    fn classifies_strings_numbers_and_functions() {
        let text = "SELECT count(x), 'abc', 42 FROM t";
        let lines = spans_of(text);
        assert_well_formed(&lines);
        let classes: Vec<_> = lines[0].iter().map(|(_, c)| *c).collect();
        assert!(classes.contains(&HighlightClass::Function), "{lines:?}");
        assert!(classes.contains(&HighlightClass::String), "{lines:?}");
        assert!(classes.contains(&HighlightClass::Number), "{lines:?}");
        assert!(classes.contains(&HighlightClass::Operator), "{lines:?}");

        assert_eq!(find(&lines[0], HighlightClass::String), Some(17..22));
        assert_eq!(find(&lines[0], HighlightClass::Number), Some(24..26));
    }

    #[test]
    fn multiline_span_is_split_across_lines() {
        let text = "SELECT 1 /* one\ntwo\nthree */ FROM t";
        let lines = spans_of(text);
        assert_eq!(lines.len(), 3);
        assert_well_formed(&lines);
        assert_eq!(find(&lines[0], HighlightClass::Comment), Some(9..15));
        assert_eq!(find(&lines[1], HighlightClass::Comment), Some(0..3));
        assert_eq!(find(&lines[2], HighlightClass::Comment), Some(0..8));
    }

    #[test]
    fn multibyte_text_stays_on_char_boundaries() {
        let text = "SELECT 'café' AS naïve\nFROM t";
        let lines = spans_of(text);
        assert_well_formed(&lines);
        let raw: Vec<&str> = text.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            for (range, _) in line {
                assert!(
                    raw[i].is_char_boundary(range.start) && raw[i].is_char_boundary(range.end),
                    "span {range:?} splits a char on line {i}"
                );
            }
        }
    }

    /// The incremental path must agree with a cold parse, keystroke by
    /// keystroke.
    #[test]
    fn incremental_matches_fresh() {
        let steps = [
            "SELECT id FROM t",
            "SELECT id, name FROM t",
            "SELECT id, name FROM t WHERE id = 1",
            "SELECT id, name FROM t WHERE id = 1 -- note",
            "SELECT id, name\nFROM t\nWHERE id = 1 -- note",
            "SELECT id, name\nFROM t\nWHERE id = 1 -- note\n",
            // Deletion in the middle.
            "SELECT id\nFROM t\nWHERE id = 1 -- note\n",
            // Multi-byte edit.
            "SELECT id, 'café'\nFROM t\nWHERE id = 1 -- nöte\n",
            // Wholesale replacement.
            "INSERT INTO t (a, b) VALUES (1, 'x')",
            "",
        ];
        let mut incremental = Highlighter::new().unwrap();
        for step in steps {
            let got = incremental.highlight(step);
            let want = spans_of(step);
            assert_eq!(got, want, "incremental mismatch for {step:?}");
            assert_well_formed(&got);
        }
    }

    #[test]
    fn repeated_identical_calls_are_stable() {
        let mut h = Highlighter::new().unwrap();
        let text = "SELECT a FROM b";
        let first = h.highlight(text);
        assert_eq!(h.highlight(text), first);
        assert_eq!(h.highlight(text), first);
    }

    #[test]
    fn broken_sql_does_not_panic() {
        for text in [
            "SELE",
            "SELECT FROM WHERE )))",
            "'unterminated",
            "/* unterminated",
            "🌵🌵🌵",
            "SELECT 1;;;;",
        ] {
            let lines = spans_of(text);
            assert_eq!(lines.len(), text.split('\n').count());
            assert_well_formed(&lines);
        }
    }

    #[test]
    fn compute_edit_describes_an_insertion() {
        let edit = compute_edit("SELECT a", "SELECT ab").unwrap();
        assert_eq!(edit.start_byte, 8);
        assert_eq!(edit.old_end_byte, 8);
        assert_eq!(edit.new_end_byte, 9);
        assert_eq!(edit.start_position, Point { row: 0, column: 8 });
    }

    #[test]
    fn compute_edit_describes_a_newline_insertion() {
        let edit = compute_edit("SELECT a", "SELECT\na").unwrap();
        assert_eq!(edit.new_end_position, Point { row: 1, column: 0 });
        assert_eq!(edit.old_end_position.row, 0);
    }

    #[test]
    fn compute_edit_is_none_for_identical_text() {
        assert!(compute_edit("SELECT 1", "SELECT 1").is_none());
    }

    #[test]
    fn compute_edit_stays_on_char_boundaries() {
        let old = "SELECT 'café'";
        let new = "SELECT 'cafè'";
        let edit = compute_edit(old, new).unwrap();
        assert!(old.is_char_boundary(edit.start_byte));
        assert!(new.is_char_boundary(edit.start_byte));
        assert!(old.is_char_boundary(edit.old_end_byte));
        assert!(new.is_char_boundary(edit.new_end_byte));
    }

    #[test]
    fn is_numeric_rules() {
        for good in ["1", "42", "3.14", "-7", "+0.5", "1e10", "2.5E-3", ".5"] {
            assert!(is_numeric(good), "{good} should be numeric");
        }
        for bad in ["", "abc", "1a", "1.2.3", "1e", "e5", "-"] {
            assert!(!is_numeric(bad), "{bad} should not be numeric");
        }
    }
}
