#!/usr/bin/env python3
"""Build a Loom guest from the engine's skin-weld crate WITHOUT committing or editing any of its source:
read crates/skin-weld/src from the rgb checkout, inline its modules into one lib.rs, strip what guests
may not use (thiserror derive, the tests), hide the crate's own `pub fn`s (they take references and would
become entries), and append a packed-bytes entry `weld_blobs` (glue.rs).

  assemble.py [--rgb DIR] [--out FILE] [--keep-dyn]   write the guest (default out/guest.rs)
  assemble.py [--rgb DIR] --check FILE                exit 0 if FILE is byte-for-byte what assembling now gives,
                                                      exit 1 if it is stale (first differing line shown),
                                                      exit 2 if the crate can no longer be assembled

--rgb defaults to $RGB_DIR, then /Volumes/Projects/andrewgazelka/rgb. FILE is the `lib.rs` that
`loom export-dir` writes (loom/definitions/skin-weld/lib.rs): the store keeps submitted source byte for byte."""
import argparse, difflib, re, sys, pathlib, os

ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument("--rgb", default=os.environ.get("RGB_DIR", "/Volumes/Projects/andrewgazelka/rgb"))
ap.add_argument("--out")
ap.add_argument("--check", metavar="FILE")
ap.add_argument("--keep-dyn", action="store_true", help="leave the dyn Fn sites (daemon needs LOOM_ALLOW_UNRESOLVED_CALLS=1)")
args = ap.parse_args()
if args.out and args.check:
    ap.error("--out and --check are exclusive")
src = pathlib.Path(args.rgb) / "crates" / "skin-weld" / "src"


def cannot(message):
    print(f"assemble: cannot assemble {src}: {message}", file=sys.stderr)
    sys.exit(2)


if not src.is_dir():
    cannot("no such directory (set --rgb or RGB_DIR)")
lib = (src / "lib.rs").read_text()
for name in ("bridge", "check", "topology"):
    body = (src / f"{name}.rs").read_text()
    lib = lib.replace(f"mod {name};", f"mod {name} {{\n{body}\n}}", 1)
# Loom resolves every call statically (its effect rows are inferred), so a `dyn Fn` call is refused. Two
# exact, asserted rewrites make these calls static; the engine's source is otherwise untouched.
def rewrite(text, old, new):
    if text.count(old) != 1:
        cannot(f"expected exactly one {old!r} (the engine's source changed; update the rewrite)")
    return text.replace(old, new)
# `--keep-dyn` leaves the crate as the engine wrote it, for a daemon started with LOOM_ALLOW_UNRESOLVED_CALLS=1.
if not args.keep_dyn:
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
if args.check:
    have = pathlib.Path(args.check).read_text()
    if have == lib:
        print(f"{args.check}: up to date with {src}")
        sys.exit(0)
    a, b = have.splitlines(), lib.splitlines()
    first = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
    print(f"{args.check}: STALE relative to {src} ({len(a)} lines committed, {len(b)} assembled; first difference at line {first + 1})", file=sys.stderr)
    for line in list(difflib.unified_diff(a, b, "committed", "assembled", lineterm="", n=1))[:24]:
        print(line, file=sys.stderr)
    print("regenerate: assemble.py --out guest.rs, then `loom update skin-weld guest.rs`, then `loom export-dir loom/definitions skin-weld`", file=sys.stderr)
    sys.exit(1)
out = pathlib.Path(args.out) if args.out else pathlib.Path(__file__).with_name("out") / "guest.rs"
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(lib)
print(f"{out}: {lib.count(chr(10))} lines")
