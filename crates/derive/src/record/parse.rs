use proc_macro2::{Span, TokenStream};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{Attribute, Data, DeriveInput, Error, Fields, Ident, LitStr, Result, Token, Type, parenthesized};

pub(crate) struct RecordIr {
    pub struct_ident: Ident,
    pub collection_name: String,
    pub updated_at_ty: Type,
    pub indexed_fields: Vec<IndexedFieldIr>,
}

pub(crate) struct IndexedFieldIr {
    pub field_ident: Ident,
    pub map_key: String,
    pub map_key_span: Span,
    pub is_optional: bool,
}

pub(crate) fn parse(input: TokenStream) -> Result<RecordIr> {
    let derive_input: DeriveInput = syn::parse2(input)?;

    let data_struct = match &derive_input.data {
        Data::Struct(s) => s,
        Data::Enum(_) => {
            return Err(Error::new(
                derive_input.ident.span(),
                "#[derive(Record)] cannot be applied to enums",
            ));
        }
        Data::Union(_) => {
            return Err(Error::new(
                derive_input.ident.span(),
                "#[derive(Record)] cannot be applied to unions",
            ));
        }
    };

    if !derive_input.generics.params.is_empty() {
        return Err(Error::new(
            derive_input.generics.span(),
            "#[derive(Record)] does not support generic structs",
        ));
    }

    let named = match &data_struct.fields {
        Fields::Named(named) => named,
        Fields::Unnamed(_) => {
            return Err(Error::new(
                derive_input.ident.span(),
                "#[derive(Record)] requires named fields; tuple structs are not supported",
            ));
        }
        Fields::Unit => {
            return Err(Error::new(
                derive_input.ident.span(),
                "#[derive(Record)] requires named fields; unit structs are not supported",
            ));
        }
    };

    let record_attrs: Vec<&Attribute> = derive_input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("record"))
        .collect();
    if record_attrs.len() > 1 {
        return Err(Error::new(
            record_attrs[1].span(),
            "multiple `#[record(...)]` attributes on struct; merge into a single attribute",
        ));
    }
    let struct_args: StructArgs = match record_attrs.first() {
        Some(attr) => attr.parse_args()?,
        None => StructArgs::default(),
    };

    if !named
        .named
        .iter()
        .any(|f| f.ident.as_ref().map(|i| i == "id").unwrap_or(false))
    {
        return Err(Error::new(
            derive_input.ident.span(),
            "#[derive(Record)] requires a field named `id`",
        ));
    }

    let updated_at_field = match named
        .named
        .iter()
        .find(|f| f.ident.as_ref().map(|i| i == "updated_at").unwrap_or(false))
    {
        Some(f) => f,
        None => {
            return Err(Error::new(
                derive_input.ident.span(),
                "#[derive(Record)] requires a field named `updated_at`",
            ));
        }
    };

    let mut indexed_fields = Vec::new();
    for field in &named.named {
        let field_ident = field.ident.as_ref().expect("named field has ident").clone();

        let field_attrs: Vec<&Attribute> = field.attrs.iter().filter(|a| a.path().is_ident("record")).collect();
        if field_attrs.len() > 1 {
            return Err(Error::new(
                field_attrs[1].span(),
                "duplicate `#[record(...)]` attribute on field",
            ));
        }
        if let Some(attr) = field_attrs.first() {
            let field_arg: FieldArg = attr.parse_args()?;
            let map_key = field_arg.key.unwrap_or_else(|| field_ident.to_string());
            let map_key_span = field_arg.key_span.unwrap_or_else(|| field_ident.span());
            let is_optional = type_is_option(&field.ty);
            indexed_fields.push(IndexedFieldIr {
                field_ident,
                map_key,
                map_key_span,
                is_optional,
            });
        }
    }

    let collection_name = struct_args
        .collection
        .unwrap_or_else(|| default_collection_name(&derive_input.ident));

    Ok(RecordIr {
        struct_ident: derive_input.ident,
        collection_name,
        updated_at_ty: updated_at_field.ty.clone(),
        indexed_fields,
    })
}

fn default_collection_name(ident: &Ident) -> String {
    let mut s = ident.to_string().to_lowercase();
    s.push('s');
    s
}

fn type_is_option(ty: &Type) -> bool {
    if let Type::Path(type_path) = ty
        && let Some(last) = type_path.path.segments.last()
        && last.ident == "Option"
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
    {
        return args.args.len() == 1;
    }
    false
}

#[derive(Default)]
struct StructArgs {
    collection: Option<String>,
}

impl Parse for StructArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut args = StructArgs::default();
        while !input.is_empty() {
            let keyword: Ident = input.parse()?;
            let keyword_span = keyword.span();
            match keyword.to_string().as_str() {
                "collection" => {
                    if args.collection.is_some() {
                        return Err(Error::new(keyword_span, "duplicate `collection` key"));
                    }
                    input.parse::<Token![=]>()?;
                    let lit: LitStr = input.parse()?;
                    let value = lit.value();
                    if value.is_empty() {
                        return Err(Error::new(lit.span(), "`collection` must be a non-empty string"));
                    }
                    args.collection = Some(value);
                }
                other => {
                    return Err(Error::new(
                        keyword_span,
                        format!(
                            "unknown `#[record(...)]` key on struct: `{}`; expected `collection`",
                            other
                        ),
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(args)
    }
}

struct FieldArg {
    key: Option<String>,
    key_span: Option<Span>,
}

impl Parse for FieldArg {
    fn parse(input: ParseStream) -> Result<Self> {
        let keyword: Ident = input.parse()?;
        if keyword != "indexed" {
            return Err(Error::new(
                keyword.span(),
                format!(
                    "unknown `#[record(...)]` key on field: `{}`; expected `indexed` or `indexed(key = \"...\")`",
                    keyword
                ),
            ));
        }
        if input.is_empty() {
            return Ok(FieldArg {
                key: None,
                key_span: None,
            });
        }
        let content;
        parenthesized!(content in input);
        let inner_kw: Ident = content.parse()?;
        if inner_kw != "key" {
            return Err(Error::new(
                inner_kw.span(),
                format!(
                    "unknown key in `#[record(indexed(...))]`: `{}`; expected `key`",
                    inner_kw
                ),
            ));
        }
        content.parse::<Token![=]>()?;
        let lit: LitStr = content.parse()?;
        let value = lit.value();
        if value.is_empty() {
            return Err(Error::new(lit.span(), "`key` must be a non-empty string"));
        }
        if !content.is_empty() {
            return Err(Error::new(
                content.span(),
                "unexpected trailing tokens in `#[record(indexed(...))]`",
            ));
        }
        Ok(FieldArg {
            key: Some(value),
            key_span: Some(lit.span()),
        })
    }
}
