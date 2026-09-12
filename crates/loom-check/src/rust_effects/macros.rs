//! Guest source contains ordinary Rust items; executable expansion is unavailable.
use super::*;

pub(crate) fn macro_diagnostics(file: &syn::File) -> Vec<loom_proto::Diagnostic> {
    struct Macros {
        messages: BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for Macros {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            self.messages.insert("Macro invocations and definitions are unavailable in guest Rust; use ordinary functions and types.".into());
            visit::visit_macro(self, node);
        }
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            let path = attribute.path();
            let inert = path.segments.len() == 1
                && path.get_ident().is_some_and(|name| {
                    [
                        "doc",
                        "allow",
                        "warn",
                        "deny",
                        "forbid",
                        "expect",
                        "inline",
                        "cold",
                        "must_use",
                        "repr",
                        "non_exhaustive",
                        "track_caller",
                        "deprecated",
                    ]
                    .contains(&name.to_string().as_str())
                });
            if !inert {
                self.messages.insert("Only inert built-in attributes are available in guest Rust; export ordinary crate-root pub fn items.".into());
            }
            visit::visit_attribute(self, attribute);
        }
    }
    let mut visitor = Macros {
        messages: BTreeSet::new(),
    };
    visitor.visit_file(file);
    visitor
        .messages
        .into_iter()
        .map(|message| crate::diagnostic(loom_proto::Lang::Rust, "LOOM_MACRO", &message))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_macros_and_active_attributes() {
        for source in [
            "pub fn main() { println!(\"hello\"); }",
            "macro_rules! helper { () => {} }",
            "#[custom::expand] pub fn main() {}",
            "#[derive(Clone)] struct Value;",
            "#[cfg_attr(all(), derive(Clone))] struct Value;",
        ] {
            assert!(
                !macro_diagnostics(&syn::parse_file(source).unwrap()).is_empty(),
                "{source}"
            );
        }
        assert!(
            macro_diagnostics(
                &syn::parse_file("use loom::sleep; #[inline] pub fn main() { sleep(1); }").unwrap()
            )
            .is_empty()
        );
    }
}
