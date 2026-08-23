//! Splitting a buffer into statements and locating the one under the cursor.

/// A statement and its byte span within the whole buffer.
///
/// The invariant callers rely on is `&text[start..end] == sql`: `start` and
/// `end` are byte offsets into the buffer the statement was split from, with
/// surrounding whitespace and the terminating `;` excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,
    pub start: usize,
    pub end: usize,
}

/// Split `text` on `;`, ignoring separators inside string literals, quoted
/// identifiers, line comments and block comments. Empty statements are dropped.
pub fn split(text: &str) -> Vec<Statement> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut seg_start = 0usize;
    let mut i = 0usize;

    // All delimiters we care about are ASCII, and UTF-8 continuation bytes
    // never collide with ASCII, so byte scanning is safe here.
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_quoted(bytes, i, b'\''),
            b'"' => i = skip_quoted(bytes, i, b'"'),
            b'`' => i = skip_quoted(bytes, i, b'`'),
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line_comment(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block_comment(bytes, i),
            b'$' => match skip_dollar_quoted(bytes, i) {
                Some(next) => i = next,
                None => i += 1,
            },
            b';' => {
                push_statement(text, seg_start, i, &mut out);
                i += 1;
                seg_start = i;
            }
            _ => i += 1,
        }
    }
    push_statement(text, seg_start, bytes.len(), &mut out);
    out
}

/// The statement containing byte offset `cursor`, falling back to the last
/// statement that ends before it.
pub fn statement_at(text: &str, cursor: usize) -> Option<Statement> {
    let statements = split(text);
    if statements.is_empty() {
        return None;
    }
    // Inside a statement (inclusive of both edges) wins.
    if let Some(hit) = statements
        .iter()
        .find(|s| cursor >= s.start && cursor <= s.end)
    {
        return Some(hit.clone());
    }
    // Otherwise the cursor is in the whitespace/semicolon gap: prefer whatever
    // just ended, so pressing run right after `;` re-runs what you typed.
    if let Some(prev) = statements.iter().rev().find(|s| s.end < cursor) {
        return Some(prev.clone());
    }
    // Cursor sits before the first statement.
    statements.into_iter().next()
}

/// Convert a (row, column-in-chars) cursor into a byte offset in `text`.
///
/// `row` is a 0-based line index and `col` a 0-based *character* index within
/// that line — exactly what `tui_textarea::TextArea::cursor()` reports. Lines
/// are assumed to be joined with `\n`. Out-of-range input saturates rather
/// than panicking.
pub fn byte_offset(text: &str, row: usize, col: usize) -> usize {
    let mut offset = 0usize;
    let mut lines = text.split('\n');
    for _ in 0..row {
        match lines.next() {
            Some(line) => offset += line.len() + 1,
            None => return text.len(),
        }
    }
    let Some(line) = lines.next() else {
        return text.len();
    };
    let col_bytes = line
        .char_indices()
        .nth(col)
        .map(|(idx, _)| idx)
        .unwrap_or(line.len());
    (offset + col_bytes).min(text.len())
}

// ---------------------------------------------------------------------------
// scanning helpers — each takes the index of the opening delimiter and returns
// the index just past the construct.
// ---------------------------------------------------------------------------

/// `'...'`, `"..."` or `` `...` ``, where a doubled quote escapes itself.
fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

/// Block comments nest in Postgres, so track depth.
fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    let mut depth = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// Length of a `$tag$` opening delimiter at `start`, if there is one. The tag
/// must be a valid identifier (possibly empty, as in `$$`), which is what keeps
/// `$1` placeholders from being mistaken for dollar quotes.
fn dollar_tag_len(bytes: &[u8], start: usize) -> Option<usize> {
    debug_assert_eq!(bytes[start], b'$');
    let mut j = start + 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if bytes.get(j) != Some(&b'$') {
        return None;
    }
    if j > start + 1 && bytes[start + 1].is_ascii_digit() {
        return None; // `$1$` is not a tag
    }
    Some(j + 1 - start)
}

/// `$$...$$` / `$tag$...$tag$`. Returns `None` when `start` is not the opening
/// of a dollar-quoted string.
fn skip_dollar_quoted(bytes: &[u8], start: usize) -> Option<usize> {
    let tag_len = dollar_tag_len(bytes, start)?;
    let tag = &bytes[start..start + tag_len];
    let mut i = start + tag_len;
    while i + tag_len <= bytes.len() {
        if &bytes[i..i + tag_len] == tag {
            return Some(i + tag_len);
        }
        i += 1;
    }
    Some(bytes.len())
}

/// True when a statement body is only whitespace and comments.
fn is_effectively_empty(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line_comment(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block_comment(bytes, i),
            _ => return false,
        }
    }
    true
}

fn push_statement(text: &str, start: usize, end: usize, out: &mut Vec<Statement>) {
    if start >= end {
        return;
    }
    let raw = &text[start..end];
    let trimmed = raw.trim();
    if trimmed.is_empty() || is_effectively_empty(trimmed) {
        return;
    }
    let lead = raw.len() - raw.trim_start().len();
    let abs_start = start + lead;
    out.push(Statement {
        sql: trimmed.to_string(),
        start: abs_start,
        end: abs_start + trimmed.len(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every statement must still index correctly into the source buffer.
    fn assert_spans(text: &str, stmts: &[Statement]) {
        for s in stmts {
            assert_eq!(&text[s.start..s.end], s.sql, "span mismatch for {s:?}");
        }
    }

    #[test]
    fn splits_multiple_statements() {
        let text = "SELECT 1;\nSELECT 2;\n  SELECT 3";
        let stmts = split(text);
        assert_spans(text, &stmts);
        let sqls: Vec<_> = stmts.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(sqls, ["SELECT 1", "SELECT 2", "SELECT 3"]);
        assert_eq!(stmts[0].start, 0);
        assert_eq!(stmts[0].end, 8);
        assert_eq!(stmts[2].start, 22);
    }

    #[test]
    fn drops_empty_and_comment_only_statements() {
        let text = "SELECT 1;;   ;\n-- just a comment\n;/* block */;SELECT 2;";
        let stmts = split(text);
        assert_spans(text, &stmts);
        let sqls: Vec<_> = stmts.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(sqls, ["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn ignores_semicolon_in_string_literal() {
        let text = "SELECT 'a;b' AS x; SELECT 2";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "SELECT 'a;b' AS x");
    }

    #[test]
    fn handles_doubled_quote_escape() {
        let text = "SELECT 'it''s; fine' AS x; SELECT 2";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "SELECT 'it''s; fine' AS x");
    }

    #[test]
    fn ignores_semicolon_in_quoted_identifiers() {
        let text = "SELECT \"we;ird\" FROM `ta;ble`; SELECT 2";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "SELECT \"we;ird\" FROM `ta;ble`");
    }

    #[test]
    fn ignores_semicolon_in_line_comment() {
        let text = "SELECT 1 -- not a; separator\nFROM t; SELECT 2";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "SELECT 1 -- not a; separator\nFROM t");
    }

    #[test]
    fn ignores_semicolon_in_nested_block_comment() {
        let text = "SELECT /* outer /* inner ; still */ still ; */ 1; SELECT 2";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert_eq!(
            stmts[0].sql,
            "SELECT /* outer /* inner ; still */ still ; */ 1"
        );
    }

    #[test]
    fn ignores_semicolon_in_dollar_quoted_body() {
        let text = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN; RETURN 1; END; $$ LANGUAGE plpgsql; SELECT 2";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].sql.ends_with("LANGUAGE plpgsql"));
        assert_eq!(stmts[1].sql, "SELECT 2");
    }

    #[test]
    fn ignores_semicolon_in_tagged_dollar_quoted_body() {
        let text = "DO $body$ SELECT 1; SELECT 2; $body$; SELECT 3";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "DO $body$ SELECT 1; SELECT 2; $body$");
        assert_eq!(stmts[1].sql, "SELECT 3");
    }

    #[test]
    fn dollar_placeholders_are_not_dollar_quotes() {
        let text = "SELECT $1; SELECT $2";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "SELECT $1");
    }

    #[test]
    fn unterminated_string_does_not_panic() {
        let text = "SELECT 'oops";
        let stmts = split(text);
        assert_spans(text, &stmts);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn statement_at_inside() {
        let text = "SELECT 1;\nSELECT 2;";
        let s = statement_at(text, 3).unwrap();
        assert_eq!(s.sql, "SELECT 1");
        let s = statement_at(text, 13).unwrap();
        assert_eq!(s.sql, "SELECT 2");
    }

    #[test]
    fn statement_at_between_prefers_previous() {
        let text = "SELECT 1;\nSELECT 2;";
        // byte 9 is the `\n` between the two statements.
        let s = statement_at(text, 9).unwrap();
        assert_eq!(s.sql, "SELECT 1");
        // Right after the trailing `;` of the last statement.
        let s = statement_at(text, text.len()).unwrap();
        assert_eq!(s.sql, "SELECT 2");
    }

    #[test]
    fn statement_at_boundaries() {
        let text = "SELECT 1;\nSELECT 2;";
        // Cursor exactly at the end of the first statement text.
        assert_eq!(statement_at(text, 8).unwrap().sql, "SELECT 1");
        // Cursor exactly at the start of the second.
        assert_eq!(statement_at(text, 10).unwrap().sql, "SELECT 2");
    }

    #[test]
    fn statement_at_before_first_returns_first() {
        let text = "\n\n   SELECT 1";
        assert_eq!(statement_at(text, 0).unwrap().sql, "SELECT 1");
    }

    #[test]
    fn statement_at_empty_buffer() {
        assert!(statement_at("", 0).is_none());
        assert!(statement_at("   \n-- nothing\n", 3).is_none());
    }

    #[test]
    fn byte_offset_basic() {
        let text = "SELECT 1\nFROM t";
        assert_eq!(byte_offset(text, 0, 0), 0);
        assert_eq!(byte_offset(text, 0, 6), 6);
        assert_eq!(byte_offset(text, 1, 0), 9);
        assert_eq!(byte_offset(text, 1, 5), 14);
    }

    #[test]
    fn byte_offset_multibyte() {
        // `é` is 2 bytes, `🌵` is 4.
        let text = "SELECT 'café'\nSELECT '🌵🌵' AS c";
        // char 11 on line 0 is `é`; char 12 is the closing quote just past it.
        assert_eq!(byte_offset(text, 0, 11), 11);
        assert_eq!(byte_offset(text, 0, 12), 13);
        assert_eq!(
            &text[byte_offset(text, 0, 11)..byte_offset(text, 0, 12)],
            "é"
        );
        assert_eq!(
            &text[byte_offset(text, 0, 12)..byte_offset(text, 0, 13)],
            "'"
        );
        // Line 1 starts after the 14 bytes + newline of line 0.
        let line1 = "SELECT '".len();
        assert_eq!(byte_offset(text, 1, 0), 15);
        assert_eq!(byte_offset(text, 1, line1), 15 + line1);
        // Past both cacti.
        assert_eq!(byte_offset(text, 1, line1 + 2), 15 + line1 + 8);
    }

    #[test]
    fn byte_offset_saturates() {
        let text = "SELECT 1\nFROM t";
        assert_eq!(byte_offset(text, 0, 999), 8);
        assert_eq!(byte_offset(text, 99, 0), text.len());
        assert_eq!(byte_offset("", 0, 0), 0);
        assert_eq!(byte_offset("", 5, 5), 0);
    }

    #[test]
    fn byte_offset_round_trips_into_statement_at() {
        let text = "SELECT 'café';\nSELECT 2";
        let off = byte_offset(text, 1, 3);
        assert_eq!(statement_at(text, off).unwrap().sql, "SELECT 2");
    }
}
