use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, Path};

use super::parse::{EdgeIr, FsmIr};

pub(crate) fn emit(ir: &FsmIr) -> TokenStream {
    let enum_ident = &ir.enum_ident;
    let role_path = &ir.role_path;

    let all_states_body = emit_variant_slice(&ir.variants);
    let terminal_states_body = emit_variant_slice(&ir.terminal);

    let validate_transition = emit_validate_transition(ir);
    let validate_override = emit_validate_override(ir);
    let valid_targets = emit_valid_targets(ir);
    let normal_targets_from = emit_targets_from(&ir.transitions, &ir.variants, "_normal");
    let override_targets_from = emit_targets_from(&ir.overrides, &ir.variants, "_override");

    quote! {
        impl #enum_ident {
            pub const fn all_states() -> &'static [Self] {
                #all_states_body
            }

            pub const fn terminal_states() -> &'static [Self] {
                #terminal_states_body
            }

            pub fn is_terminal(self) -> bool {
                Self::terminal_states().contains(&self)
            }

            pub fn validate_transition(
                from: Self,
                to: Self,
                role: #role_path,
            ) -> ::std::result::Result<::domain::Transition, ::domain::FsmError<Self>> {
                #validate_transition
            }

            pub fn validate_override(
                from: Self,
                to: Self,
                role: #role_path,
            ) -> ::std::result::Result<::domain::Transition, ::domain::FsmError<Self>> {
                #validate_override
            }

            pub fn valid_targets(
                from: Self,
                role: #role_path,
            ) -> ::std::vec::Vec<(Self, ::domain::TargetKind)> {
                #valid_targets
            }

            #[doc(hidden)]
            pub const fn _normal_targets_from(from: Self) -> &'static [(Self, &'static [&'static str])] {
                #normal_targets_from
            }

            #[doc(hidden)]
            pub const fn _override_targets_from(from: Self) -> &'static [(Self, &'static [&'static str])] {
                #override_targets_from
            }
        }
    }
}

fn emit_variant_slice(variants: &[Ident]) -> TokenStream {
    if variants.is_empty() {
        quote! { &[] }
    } else {
        let entries = variants.iter().map(|v| quote! { Self::#v });
        quote! { &[#(#entries),*] }
    }
}

fn emit_validate_transition(ir: &FsmIr) -> TokenStream {
    let role_path = &ir.role_path;
    if ir.transitions.is_empty() {
        return quote! {
            if from == to {
                return ::std::result::Result::Ok(::domain::Transition::Unchanged);
            }
            ::std::result::Result::Err(::domain::FsmError {
                from,
                to,
                role: ::std::string::ToString::to_string(&role),
                kind: ::domain::FsmErrorKind::NoTransition,
                valid_normal: Self::_normal_targets_from(from),
                valid_override: Self::_override_targets_from(from),
                context: ::std::option::Option::None,
            })
        };
    }
    let arms = ir.transitions.iter().map(|e| edge_match_arm(e, role_path));
    quote! {
        if from == to {
            return ::std::result::Result::Ok(::domain::Transition::Unchanged);
        }
        let authorized: &[#role_path] = match (from, to) {
            #(#arms)*
            _ => {
                return ::std::result::Result::Err(::domain::FsmError {
                    from,
                    to,
                    role: ::std::string::ToString::to_string(&role),
                    kind: ::domain::FsmErrorKind::NoTransition,
                    valid_normal: Self::_normal_targets_from(from),
                    valid_override: Self::_override_targets_from(from),
                    context: ::std::option::Option::None,
                });
            }
        };
        if authorized.is_empty() || authorized.contains(&role) {
            ::std::result::Result::Ok(::domain::Transition::Changed)
        } else {
            ::std::result::Result::Err(::domain::FsmError {
                from,
                to,
                role: ::std::string::ToString::to_string(&role),
                kind: ::domain::FsmErrorKind::RoleNotAuthorized,
                valid_normal: Self::_normal_targets_from(from),
                valid_override: Self::_override_targets_from(from),
                context: ::std::option::Option::None,
            })
        }
    }
}

fn emit_validate_override(ir: &FsmIr) -> TokenStream {
    let role_path = &ir.role_path;
    if ir.overrides.is_empty() {
        return quote! {
            match Self::validate_transition(from, to, role) {
                ::std::result::Result::Ok(t) => ::std::result::Result::Ok(t),
                ::std::result::Result::Err(normal_err) => {
                    ::std::result::Result::Err(::domain::FsmError {
                        from,
                        to,
                        role: ::std::string::ToString::to_string(&role),
                        kind: ::domain::FsmErrorKind::NoTransition,
                        valid_normal: Self::_normal_targets_from(from),
                        valid_override: Self::_override_targets_from(from),
                        context: ::std::option::Option::Some(::std::boxed::Box::new(normal_err)),
                    })
                }
            }
        };
    }
    let arms = ir.overrides.iter().map(|e| edge_match_arm(e, role_path));
    quote! {
        match Self::validate_transition(from, to, role) {
            ::std::result::Result::Ok(t) => ::std::result::Result::Ok(t),
            ::std::result::Result::Err(normal_err) => {
                let authorized: &[#role_path] = match (from, to) {
                    #(#arms)*
                    _ => {
                        return ::std::result::Result::Err(::domain::FsmError {
                            from,
                            to,
                            role: ::std::string::ToString::to_string(&role),
                            kind: ::domain::FsmErrorKind::NoTransition,
                            valid_normal: Self::_normal_targets_from(from),
                            valid_override: Self::_override_targets_from(from),
                            context: ::std::option::Option::Some(::std::boxed::Box::new(normal_err)),
                        });
                    }
                };
                if authorized.is_empty() || authorized.contains(&role) {
                    ::std::result::Result::Ok(::domain::Transition::Override)
                } else {
                    ::std::result::Result::Err(::domain::FsmError {
                        from,
                        to,
                        role: ::std::string::ToString::to_string(&role),
                        kind: ::domain::FsmErrorKind::RoleNotAuthorized,
                        valid_normal: Self::_normal_targets_from(from),
                        valid_override: Self::_override_targets_from(from),
                        context: ::std::option::Option::Some(::std::boxed::Box::new(normal_err)),
                    })
                }
            }
        }
    }
}

fn emit_valid_targets(ir: &FsmIr) -> TokenStream {
    let role_path = &ir.role_path;
    let normal_checks = ir.transitions.iter().map(|e| {
        let from = &e.from;
        let to = &e.to;
        let cond = role_condition(&e.by, role_path);
        quote! {
            if from == Self::#from && (#cond) {
                out.push((Self::#to, ::domain::TargetKind::Normal));
            }
        }
    });
    let override_checks = ir.overrides.iter().map(|e| {
        let from = &e.from;
        let to = &e.to;
        let cond = role_condition(&e.by, role_path);
        quote! {
            if from == Self::#from && (#cond) {
                out.push((Self::#to, ::domain::TargetKind::Override));
            }
        }
    });
    quote! {
        let _ = role;
        let mut out: ::std::vec::Vec<(Self, ::domain::TargetKind)> = ::std::vec::Vec::new();
        #(#normal_checks)*
        #(#override_checks)*
        out
    }
}

fn edge_match_arm(edge: &EdgeIr, role_path: &Path) -> TokenStream {
    let from = &edge.from;
    let to = &edge.to;
    if edge.by.is_empty() {
        quote! { (Self::#from, Self::#to) => &[], }
    } else {
        let roles = edge.by.iter().map(|r| quote! { #role_path::#r });
        quote! { (Self::#from, Self::#to) => &[#(#roles),*], }
    }
}

fn role_condition(by: &[Ident], role_path: &Path) -> TokenStream {
    if by.is_empty() {
        return quote! { true };
    }
    let patterns = by.iter().map(|r| quote! { #role_path::#r });
    quote! { ::std::matches!(role, #(#patterns)|*) }
}

fn emit_targets_from(edges: &[EdgeIr], variants: &[Ident], _label: &str) -> TokenStream {
    use std::collections::BTreeMap;
    let mut by_from: BTreeMap<String, (Ident, Vec<&EdgeIr>)> = BTreeMap::new();
    for edge in edges {
        let key = edge.from.to_string();
        by_from
            .entry(key)
            .or_insert_with(|| (edge.from.clone(), Vec::new()))
            .1
            .push(edge);
    }
    if by_from.is_empty() {
        let _ = variants;
        let _ = format_ident!("unused");
        return quote! {
            let _ = from;
            &[]
        };
    }
    let arms = by_from.values().map(|(from_ident, edges)| {
        let entries = edges.iter().map(|e| {
            let to = &e.to;
            if e.by.is_empty() {
                quote! { (Self::#to, &[]) }
            } else {
                let role_strs = e.by.iter().map(ident_to_kebab);
                let role_lits = role_strs.map(|s| quote! { #s });
                quote! { (Self::#to, &[#(#role_lits),*]) }
            }
        });
        quote! {
            Self::#from_ident => &[#(#entries),*],
        }
    });
    quote! {
        match from {
            #(#arms)*
            _ => &[],
        }
    }
}

fn ident_to_kebab(ident: &Ident) -> String {
    let s = ident.to_string();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.char_indices() {
        if c.is_uppercase() && i > 0 {
            out.push('-');
        }
        out.extend(c.to_lowercase());
    }
    out
}
