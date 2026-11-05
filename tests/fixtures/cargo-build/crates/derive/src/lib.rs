extern crate proc_macro;

use proc_macro::TokenStream;

/// Emits `const MARKER: u32`, computed on the host from `util`, so the
/// host copy of `util` is linked into this proc macro.
#[proc_macro_derive(Marker)]
pub fn marker(_input: TokenStream) -> TokenStream {
    format!("const MARKER: u32 = {};", util::base())
        .parse()
        .unwrap()
}
