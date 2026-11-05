use proc_macro::{TokenStream, TokenTree};

#[cfg(not(hello_bd))]
compile_error!("the build-dependency's cfg is missing");

#[proc_macro_derive(Hello)]
pub fn derive_hello(input: TokenStream) -> TokenStream {
    let mut tokens = input.into_iter();
    let mut name = None;
    while let Some(t) = tokens.next() {
        if matches!(&t, TokenTree::Ident(i) if i.to_string() == "struct") {
            if let Some(TokenTree::Ident(n)) = tokens.next() {
                name = Some(n.to_string());
            }
            break;
        }
    }
    let name = name.expect("derive(Hello) takes a struct");
    let value = pm2::VALUE + qt::VALUE;
    format!("impl Hello for {name} {{ fn hello() -> u32 {{ {value} }} }}")
        .parse()
        .unwrap()
}
