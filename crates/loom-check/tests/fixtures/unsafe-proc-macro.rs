extern crate proc_macro;

#[proc_macro]
pub fn hidden_unsafe(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    "unsafe { core::ptr::read_volatile(&1) }".parse().unwrap()
}
