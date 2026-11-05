//! `#[derive(Hello)]`: adds `const HELLO: &str`, the type's name.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{DeriveInput, LitStr, parse_macro_input};

/// Derive `HELLO`, a constant holding the type's name.
#[proc_macro_derive(Hello)]
pub fn derive_hello(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let text = LitStr::new(&name.to_string(), Span::call_site());
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    quote! {
        impl #impl_generics #name #ty_generics #where_clause {
            /// The type's name.
            pub const HELLO: &'static str = #text;
        }
    }
    .into()
}
