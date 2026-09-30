//! REPL cells: a bare block of statements and a final expression is a cell too.
use std::borrow::Cow;

/// The line `interactive_cell` puts before a bare block's text.
pub const CELL_HEADER: &str = "pub fn eval() -> impl ::loom::serde::Serialize {";

/// The definition source for an `eval` cell.
///
/// A cell that parses as a file (items, with or without a `pub fn`) is a
/// definition and is used as written. Otherwise, when the text is a block body
/// (statements, then an optional final expression), it becomes the body of
/// `pub fn eval()`, so
/// `let v = vec![1, 2, 3]; v.iter().sum::<i32>()` is a cell. The wrapper adds
/// exactly one line before the text (`CELL_HEADER`), which `eval` subtracts from
/// the lines of a compile failure. Text that is neither is returned unchanged,
/// and the compiler reports on what the caller wrote.
pub fn interactive_cell(source: &str) -> Cow<'_, str> {
    // Text that parses as a file is a set of items and is used as written, with
    // or without a `pub fn`: wrapping items into a block would hide them (a
    // helper `fn add(..)` would become a private function inside `eval` and the
    // cell would answer `null`). The compiler then says the cell has no entry.
    if syn::parse_file(source).is_ok() {
        return Cow::Borrowed(source);
    }
    let wrapped = format!("{CELL_HEADER}\n{source}\n}}");
    if syn::parse_file(&wrapped).is_ok() {
        Cow::Owned(wrapped)
    } else {
        Cow::Borrowed(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_definition_is_used_as_written() {
        let source = "pub fn double(x: i64) -> i64 { x * 2 }";
        assert!(matches!(interactive_cell(source), Cow::Borrowed(_)));
        let with_helper = "fn helper() -> i64 { 1 }\npub fn one() -> i64 { helper() }";
        assert!(matches!(interactive_cell(with_helper), Cow::Borrowed(_)));
    }

    #[test]
    fn a_block_body_becomes_the_eval_entry_under_one_header_line() {
        let cell = interactive_cell("let v = vec![1, 2, 3];\nv.iter().sum::<i32>()");
        let mut lines = cell.lines();
        assert_eq!(lines.next(), Some(CELL_HEADER));
        assert_eq!(lines.next(), Some("let v = vec![1, 2, 3];"));
        assert_eq!(
            cell.lines().count(),
            4,
            "header, two lines of text, closing brace"
        );
        assert!(interactive_cell("1 + 2").contains("pub fn eval()"));
    }

    #[test]
    fn text_that_is_neither_is_left_for_the_compiler_to_reject() {
        let broken = "pub fn broken( {";
        assert_eq!(interactive_cell(broken), broken);
    }

    #[test]
    fn items_without_an_entry_are_not_hidden_inside_a_block() {
        for items in [
            "struct Point { x: i32 }",
            "fn add(a: i32, b: i32) -> i32 { a + b }",
        ] {
            assert!(
                matches!(interactive_cell(items), Cow::Borrowed(_)),
                "{items}"
            );
        }
        // Helpers followed by an expression are a block body, which keeps them.
        let mixed = interactive_cell("fn add(a: i32, b: i32) -> i32 { a + b }\nadd(1, 2)");
        assert!(mixed.starts_with(CELL_HEADER), "{mixed}");
    }
}
