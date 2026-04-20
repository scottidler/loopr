use proc_macro2::TokenStream;
use quote::quote;

use super::parse::{IndexedFieldIr, RecordIr};

pub(crate) fn emit(ir: &RecordIr) -> TokenStream {
    let struct_ident = &ir.struct_ident;
    let collection = &ir.collection_name;

    let inserts: Vec<TokenStream> = ir.indexed_fields.iter().map(emit_insert).collect();

    quote! {
        impl ::taskstore_traits::Record for #struct_ident {
            fn id(&self) -> &str {
                ::std::convert::AsRef::<str>::as_ref(&self.id)
            }

            fn updated_at(&self) -> i64 {
                self.updated_at
            }

            fn collection_name() -> &'static str {
                #collection
            }

            fn indexed_fields(&self) -> ::std::collections::HashMap<::std::string::String, ::taskstore_traits::IndexValue> {
                let mut m: ::std::collections::HashMap<::std::string::String, ::taskstore_traits::IndexValue> =
                    ::std::collections::HashMap::new();
                #(#inserts)*
                m
            }
        }
    }
}

fn emit_insert(f: &IndexedFieldIr) -> TokenStream {
    let ident = &f.field_ident;
    let key = &f.map_key;
    if f.is_optional {
        quote! {
            if let ::std::option::Option::Some(ref v) = self.#ident {
                m.insert(
                    ::std::string::String::from(#key),
                    ::taskstore_traits::IndexValue::String(::std::string::ToString::to_string(v)),
                );
            }
        }
    } else {
        quote! {
            m.insert(
                ::std::string::String::from(#key),
                ::taskstore_traits::IndexValue::String(::std::string::ToString::to_string(&self.#ident)),
            );
        }
    }
}
