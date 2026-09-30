//! Where the compiler's lines came from in the client's source.
//!
//! A Rust definition is stored as `prettyplease::unparse` of what the client wrote: no comments, no
//! blank lines, and long statements re-wrapped. The compiler's DWARF names lines of that text, so
//! a line number from a backtrace means nothing in the client's editor. Both texts parse to the
//! same syntax tree (unparse only changes layout), so walking the two trees in step pairs every
//! item, statement and expression with the line where it starts in each text.
use syn::spanned::Spanned;
use syn::visit::Visit;

/// One node start: what kind of node, and its 1-based start line.
struct Starts(Vec<(std::mem::Discriminant<Node>, u32)>);

/// The node kinds that begin a line of code. Only their order and kind matter.
enum Node {
    Item,
    Stmt,
    Expr,
    Pat,
}

impl<'ast> Visit<'ast> for Starts {
    fn visit_item(&mut self, node: &'ast syn::Item) {
        self.0.push((std::mem::discriminant(&Node::Item), line(node)));
        syn::visit::visit_item(self, node);
    }
    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        self.0.push((std::mem::discriminant(&Node::Stmt), line(node)));
        syn::visit::visit_stmt(self, node);
    }
    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        self.0.push((std::mem::discriminant(&Node::Expr), line(node)));
        syn::visit::visit_expr(self, node);
    }
    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        self.0.push((std::mem::discriminant(&Node::Pat), line(node)));
        syn::visit::visit_pat(self, node);
    }
}

fn line(node: &impl Spanned) -> u32 {
    node.span().start().line as u32
}

fn starts(source: &str) -> Option<Starts> {
    let file = syn::parse_file(source).ok()?;
    let mut starts = Starts(Vec::new());
    starts.visit_file(&file);
    Some(starts)
}

/// `original` as the compiler sees it: the unparsed text, or `None` if it does not parse.
pub fn normalized(original: &str) -> Option<String> {
    syn::parse_file(original).ok().map(|file| prettyplease::unparse(&file))
}

/// For each line of [`normalized`]`(original)` (index 0 is line 1), the line of `original` where
/// the code on it starts. A line no node starts on (the continuation of a re-wrapped statement)
/// takes the mapping of the nearest line above it. `None` when `original` does not parse or the
/// two trees do not line up node for node.
pub fn original_lines(original: &str) -> Option<Vec<u32>> {
    let normalized = normalized(original)?;
    let (before, after) = (starts(original)?, starts(&normalized)?);
    if before.0.len() != after.0.len()
        || before.0.iter().zip(&after.0).any(|(a, b)| a.0 != b.0)
    {
        return None;
    }
    let lines = normalized.lines().count();
    let mut map = vec![0u32; lines + 1];
    // Several nodes start on one line (a statement and its expressions); the outermost comes first
    // in pre-order and names the line.
    for ((_, from), (_, to)) in before.0.iter().zip(&after.0) {
        let slot = &mut map[*to as usize];
        if *slot == 0 {
            *slot = *from;
        }
    }
    let mut last = 1;
    let mut out = Vec::with_capacity(lines);
    for line in 1..=lines {
        if map[line] != 0 {
            last = map[line];
        }
        out.push(last);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_survive_dropped_comments_blank_lines_and_rewrapping() {
        let original = "use a::b;\n\n// note\nfn f() {\n    let x = g(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23);\n\n    h(x);\n}\n";
        let text = normalized(original).unwrap();
        let map = original_lines(original).unwrap();
        let find = |needle: &str| text.lines().position(|l| l.contains(needle)).unwrap();
        assert_eq!(map[find("fn f")], 4);
        assert_eq!(map[find("let x")], 5);
        assert_eq!(map[find("h(x)")], 7);
        // The wrapped call's argument lines still point into the statement they came from.
        assert!(text.lines().count() > 6, "{text}");
    }

    #[test]
    fn source_that_does_not_parse_has_no_map() {
        assert!(original_lines("fn f( {").is_none());
    }
}
