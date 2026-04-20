use proc_macro2::TokenStream;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Data, DeriveInput, Error, Fields, Ident, Path, Result, Token, bracketed, parenthesized};

pub(crate) struct FsmIr {
    pub enum_ident: Ident,
    pub variants: Vec<Ident>,
    pub role_path: Path,
    pub terminal: Vec<Ident>,
    pub transitions: Vec<EdgeIr>,
    pub overrides: Vec<EdgeIr>,
}

pub(crate) struct EdgeIr {
    pub from: Ident,
    pub to: Ident,
    pub by: Vec<Ident>,
}

pub(crate) fn parse(input: TokenStream) -> Result<FsmIr> {
    let derive_input: DeriveInput = syn::parse2(input)?;

    let Data::Enum(data_enum) = &derive_input.data else {
        return Err(Error::new(
            derive_input.ident.span(),
            "#[derive(Fsm)] can only be applied to enums",
        ));
    };

    if !derive_input.generics.params.is_empty() {
        return Err(Error::new(
            derive_input.generics.span(),
            "#[derive(Fsm)] does not support generic enums",
        ));
    }

    if data_enum.variants.is_empty() {
        return Err(Error::new(
            derive_input.ident.span(),
            "#[derive(Fsm)] requires at least one variant",
        ));
    }

    let mut variants = Vec::with_capacity(data_enum.variants.len());
    for v in &data_enum.variants {
        match v.fields {
            Fields::Unit => variants.push(v.ident.clone()),
            _ => {
                return Err(Error::new(v.ident.span(), "#[derive(Fsm)] only supports unit variants"));
            }
        }
    }

    let fsm_attrs: Vec<&Attribute> = derive_input.attrs.iter().filter(|a| a.path().is_ident("fsm")).collect();
    if fsm_attrs.is_empty() {
        return Err(Error::new(
            derive_input.ident.span(),
            "#[derive(Fsm)] requires a `#[fsm(...)]` attribute",
        ));
    }
    if fsm_attrs.len() > 1 {
        return Err(Error::new(
            fsm_attrs[1].span(),
            "multiple `#[fsm(...)]` attributes; merge into a single attribute",
        ));
    }

    let args: FsmArgs = fsm_attrs[0].parse_args()?;

    let role_path = args
        .role
        .ok_or_else(|| Error::new(fsm_attrs[0].span(), "`#[fsm(...)]` requires `role = <path>`"))?;

    Ok(FsmIr {
        enum_ident: derive_input.ident,
        variants,
        role_path,
        terminal: args.terminal.unwrap_or_default(),
        transitions: args.transitions.unwrap_or_default(),
        overrides: args.overrides.unwrap_or_default(),
    })
}

struct FsmArgs {
    role: Option<Path>,
    terminal: Option<Vec<Ident>>,
    transitions: Option<Vec<EdgeIr>>,
    overrides: Option<Vec<EdgeIr>>,
}

impl Parse for FsmArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut args = FsmArgs {
            role: None,
            terminal: None,
            transitions: None,
            overrides: None,
        };
        while !input.is_empty() {
            let keyword: Ident = input.parse()?;
            let keyword_span = keyword.span();
            match keyword.to_string().as_str() {
                "role" => {
                    if args.role.is_some() {
                        return Err(Error::new(keyword_span, "duplicate `role` key"));
                    }
                    input.parse::<Token![=]>()?;
                    args.role = Some(input.parse::<Path>()?);
                }
                "terminal" => {
                    if args.terminal.is_some() {
                        return Err(Error::new(keyword_span, "duplicate `terminal` key"));
                    }
                    input.parse::<Token![=]>()?;
                    let content;
                    bracketed!(content in input);
                    let idents: Punctuated<Ident, Token![,]> = content.parse_terminated(Ident::parse, Token![,])?;
                    args.terminal = Some(idents.into_iter().collect());
                }
                "transitions" => {
                    if args.transitions.is_some() {
                        return Err(Error::new(keyword_span, "duplicate `transitions` key"));
                    }
                    let content;
                    parenthesized!(content in input);
                    let edges: Punctuated<EdgeIr, Token![,]> = content.parse_terminated(EdgeIr::parse, Token![,])?;
                    args.transitions = Some(edges.into_iter().collect());
                }
                "overrides" => {
                    if args.overrides.is_some() {
                        return Err(Error::new(keyword_span, "duplicate `overrides` key"));
                    }
                    let content;
                    parenthesized!(content in input);
                    let edges: Punctuated<EdgeIr, Token![,]> = content.parse_terminated(EdgeIr::parse, Token![,])?;
                    args.overrides = Some(edges.into_iter().collect());
                }
                other => {
                    return Err(Error::new(
                        keyword_span,
                        format!(
                            "unknown `#[fsm(...)]` key: `{}`; expected `role`, `terminal`, `transitions`, or `overrides`",
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

impl Parse for EdgeIr {
    fn parse(input: ParseStream) -> Result<Self> {
        let from: Ident = input.parse()?;
        input.parse::<Token![=>]>()?;
        let to: Ident = input.parse()?;
        let by_kw: Ident = input.parse()?;
        if by_kw != "by" {
            return Err(Error::new(by_kw.span(), "expected keyword `by`"));
        }
        let content;
        parenthesized!(content in input);
        let roles: Punctuated<Ident, Token![,]> = content.parse_terminated(Ident::parse, Token![,])?;
        let by_idents: Vec<Ident> = roles.into_iter().collect();
        if by_idents.is_empty() {
            return Err(Error::new(
                by_kw.span(),
                "`by (...)` must list at least one role; use `by (Any)` for no restriction",
            ));
        }
        let contains_any = by_idents.iter().any(|r| r == "Any");
        let is_pure_any = by_idents.len() == 1 && by_idents[0] == "Any";
        if contains_any && !is_pure_any {
            return Err(Error::new(
                by_idents.iter().find(|r| *r == "Any").unwrap().span(),
                "`Any` cannot be combined with concrete roles",
            ));
        }
        let by = if is_pure_any { Vec::new() } else { by_idents };
        Ok(EdgeIr { from, to, by })
    }
}
