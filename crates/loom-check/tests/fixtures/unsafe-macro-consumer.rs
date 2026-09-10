#[loom::def(effects=[])]
pub fn main() -> i32 {
    untrusted_macro::hidden_unsafe!()
}
