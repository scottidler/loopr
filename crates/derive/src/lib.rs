use proc_macro::TokenStream;

mod fsm;
mod record;

#[proc_macro_derive(Fsm, attributes(fsm))]
pub fn fsm_derive(input: TokenStream) -> TokenStream {
    fsm::expand(input.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

#[proc_macro_derive(Record, attributes(record))]
pub fn record_derive(input: TokenStream) -> TokenStream {
    record::expand(input.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
