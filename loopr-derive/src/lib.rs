use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

/// Derive macro that generates:
/// 1. A case-insensitive `FromStr` impl (rejects underscores/hyphens)
/// 2. A `VARIANT_NAMES` const with PascalCase names
#[proc_macro_derive(FlexibleEnum)]
pub fn flexible_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let variants = match &input.data {
        Data::Enum(data) => &data.variants,
        _ => panic!("FlexibleEnum can only be derived for enums"),
    };

    // Ensure all variants are unit variants (no fields)
    for variant in variants {
        if !matches!(variant.fields, Fields::Unit) {
            panic!(
                "FlexibleEnum only supports unit variants, but `{}` has fields",
                variant.ident
            );
        }
    }

    let variant_idents: Vec<_> = variants.iter().map(|v| &v.ident).collect();
    let variant_strings: Vec<String> = variant_idents.iter().map(|id| id.to_string()).collect();
    let variant_lowercase: Vec<String> = variant_strings.iter().map(|s| s.to_lowercase()).collect();

    let match_arms = variant_idents
        .iter()
        .zip(variant_lowercase.iter())
        .map(|(ident, lower)| {
            quote! { #lower => Ok(Self::#ident) }
        });

    let valid_list = variant_strings.join(", ");
    let enum_name_str = name.to_string();

    let variant_name_literals: Vec<_> = variant_strings
        .iter()
        .map(|s| {
            quote! { #s }
        })
        .collect();

    let variant_count = variant_idents.len();

    let expanded = quote! {
        impl std::str::FromStr for #name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if s.contains('_') || s.contains('-') {
                    return Err(format!(
                        "invalid {}: '{}' - underscores and hyphens are not allowed (valid: {})",
                        #enum_name_str, s, #valid_list
                    ));
                }
                let normalized = s.to_lowercase();
                match normalized.as_str() {
                    #(#match_arms,)*
                    _ => Err(format!(
                        "invalid {}: '{}' (valid: {})",
                        #enum_name_str, s, #valid_list
                    )),
                }
            }
        }

        impl #name {
            /// All variant names as they should appear in prompts/docs.
            pub const VARIANT_NAMES: &'static [&'static str; #variant_count] = &[
                #(#variant_name_literals),*
            ];
        }
    };

    TokenStream::from(expanded)
}

#[cfg(test)]
mod tests {
    // Proc-macro crates can't have unit tests that use the macro directly.
    // Tests live in the consuming crate (loopr).
}
