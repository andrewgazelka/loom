#!/usr/bin/env python3
"""Build a Loom guest from the engine's skin-weld crate WITHOUT committing or editing any of its source:
read crates/skin-weld/src from the rgb checkout, inline its modules into one lib.rs, strip what guests
may not use (thiserror derive, the tests), hide the crate's own `pub fn`s (they take references and would
become entries), and append a packed-bytes entry `weld_blobs`."""
import re, sys, pathlib
src = pathlib.Path("/Volumes/Projects/andrewgazelka/rgb/crates/skin-weld/src")
lib = (src / "lib.rs").read_text()
for name in ("bridge", "check", "topology"):
    body = (src / f"{name}.rs").read_text()
    lib = lib.replace(f"mod {name};", f"mod {name} {{\n{body}\n}}", 1)
# Loom resolves every call statically (its effect rows are inferred), so a `dyn Fn` call is refused. Two
# exact, asserted rewrites make these calls static; the engine's source is otherwise untouched.
def rewrite(text, old, new):
    assert text.count(old) == 1, old
    return text.replace(old, new)
lib = rewrite(lib, "(head, &head_nodes, head_s, &head_share as &dyn Fn(f64) -> f64),", "(head, &head_nodes, head_s, true),")
lib = rewrite(lib, "(body, &body_nodes, body_s, &body_share),", "(body, &body_nodes, body_s, false),")
lib = rewrite(lib, "let k = share(s[v]);", "let k = if share { head_share(s[v]) } else { body_share(s[v]) };")
lib = rewrite(lib, "positions: &dyn Fn(u32) -> DVec3,", "positions: &impl Fn(u32) -> DVec3,")
lib = lib.replace("#[cfg(test)]\nmod tests;\n", "").replace("#[cfg(test)]\nmod bridge_tests;\n", "")
lib = re.sub(r"^pub use ", "use ", lib, flags=re.M)
lib = re.sub(r"^pub fn ", "pub(crate) fn ", lib, flags=re.M)
lib = lib.replace("#[derive(Debug, thiserror::Error, PartialEq, Eq)]", "#[derive(Debug, PartialEq, Eq)]")
lib = re.sub(r"^    #\[error\((?:.|\n)*?\)\]\n", "", lib, flags=re.M)
lib += '''
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
'''
glue = pathlib.Path(__file__).with_name("glue.rs")
lib += glue.read_text()
out = pathlib.Path(__file__).with_name("out") / "guest.rs"
out.parent.mkdir(exist_ok=True)
out.write_text(lib)
print(f"{out}: {lib.count(chr(10))} lines")
