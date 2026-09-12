//! Source admission; effect inference belongs to the rustc driver.
use std::collections::BTreeSet;
use syn::visit::{self, Visit};
mod admission;
pub(crate) use admission::unsafe_source_diagnostics;
mod macros;
pub(crate) use macros::macro_diagnostics;
