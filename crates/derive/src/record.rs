use proc_macro2::TokenStream;
use syn::Result;

mod emit;
mod parse;
mod validate;

pub(crate) fn expand(input: TokenStream) -> Result<TokenStream> {
    let ir = parse::parse(input)?;
    validate::validate(&ir)?;
    Ok(emit::emit(&ir))
}
