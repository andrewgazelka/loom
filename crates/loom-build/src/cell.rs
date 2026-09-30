//! REPL cells: a bare block of statements and a final expression is a cell too.
use std::borrow::Cow;

/// The definition source for an `eval` cell.
///
/// A cell that already defines a root `pub fn` is a definition and is used as
/// written. Otherwise, when the text is a block body (statements, then an
/// optional final expression), it becomes the body of `pub fn eval()`, so
/// `let v = vec![1, 2, 3]; v.iter().sum::<i32>()` is a cell. The wrapper opens
/// on the text's first line and adds no line before it, so diagnostics keep
/// their line numbers. Text that is neither is returned unchanged, and the
/// compiler reports on what the caller wrote.
pub fn interactive_cell(source: &str) -> Cow<'_, str> {
    if let Ok(file) = syn::parse_file(source)
        && file.items.iter().any(|item| {
            matches!(item, syn::Item::Fn(function)
                if matches!(function.vis, syn::Visibility::Public(_)))
        })
    {
        return Cow::Borrowed(source);
    }
    let wrapped = format!("pub fn eval() -> impl ::loom::serde::Serialize {{ {source}\n}}");
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
    fn a_block_body_becomes_the_eval_entry_without_shifting_lines() {
        let cell = interactive_cell("let v = vec![1, 2, 3];\nv.iter().sum::<i32>()");
        assert!(cell.starts_with("pub fn eval() -> impl ::loom::serde::Serialize { let v"));
        assert_eq!(cell.lines().count(), 3, "one closing line, none before");
        assert!(interactive_cell("1 + 2").contains("pub fn eval()"));
    }

    #[test]
    fn text_that_is_neither_is_left_for_the_compiler_to_reject() {
        let broken = "pub fn broken( {";
        assert_eq!(interactive_cell(broken), broken);
        let items_only = "struct Point { x: i32 }";
        // Items without a pub fn wrap into a body that still parses (items are
        // statements), so a cell may define helpers and end in an expression.
        assert!(interactive_cell(items_only).contains("pub fn eval()"));
    }
}
