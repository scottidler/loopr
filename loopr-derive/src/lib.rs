use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Data, DeriveInput, Fields, Ident, Token, parse_macro_input};

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

// ============================================================================
// Fsm derive macro
// ============================================================================

/// A transition target: `Ready` (any role) or `Ready(Coordinator, Integrator)` (specific roles).
struct TransitionTarget {
    target: Ident,
    roles: Vec<Ident>,
}

impl Parse for TransitionTarget {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let target: Ident = input.parse()?;
        let roles = if input.peek(syn::token::Paren) {
            let content;
            syn::parenthesized!(content in input);
            let punctuated: syn::punctuated::Punctuated<Ident, Token![,]> =
                content.parse_terminated(Ident::parse, Token![,])?;
            punctuated.into_iter().collect()
        } else {
            Vec::new()
        };
        Ok(TransitionTarget { target, roles })
    }
}

/// Comma-separated list of transition targets.
struct TransitionList {
    targets: Vec<TransitionTarget>,
}

impl Parse for TransitionList {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let punctuated: syn::punctuated::Punctuated<TransitionTarget, Token![,]> =
            input.parse_terminated(TransitionTarget::parse, Token![,])?;
        Ok(TransitionList {
            targets: punctuated.into_iter().collect(),
        })
    }
}

fn target_to_match_arm(
    from: &Ident,
    target: &TransitionTarget,
) -> proc_macro2::TokenStream {
    let to = &target.target;
    if target.roles.is_empty() {
        quote! { (Self::#from, Self::#to) => true }
    } else {
        let roles: Vec<_> = target.roles.iter().map(|r| quote! { Role::#r }).collect();
        quote! { (Self::#from, Self::#to) => matches!(role, #(#roles)|*) }
    }
}

/// Derive macro that generates FSM validation methods from transition attributes.
///
/// # Attributes
///
/// - `#[transitions(Target, Target(Role1, Role2))]` - valid transitions from this variant
/// - `#[overrides(Target(Role1))]` - additional override-only transitions
///
/// # Generated Methods
///
/// - `validate_transition(self, target, role) -> Result<Transition>` - validate normal transitions
/// - `validate_override(self, target, role) -> Result<Transition>` - normal + override transitions
///   (only generated if any variant has `#[overrides]`)
/// - `is_terminal(self) -> bool` - true for variants with no `#[transitions]` attribute
#[proc_macro_derive(Fsm, attributes(transitions, overrides))]
pub fn fsm(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let variants = match &input.data {
        Data::Enum(data) => &data.variants,
        _ => panic!("Fsm can only be derived for enums"),
    };

    for variant in variants {
        if !matches!(variant.fields, Fields::Unit) {
            panic!(
                "Fsm only supports unit variants, but `{}` has fields",
                variant.ident
            );
        }
    }

    let mut normal_arms: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut override_arms: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut terminal_variants: Vec<&Ident> = Vec::new();
    let mut has_overrides = false;

    for variant in variants {
        let variant_ident = &variant.ident;
        let mut has_transitions = false;

        for attr in &variant.attrs {
            if attr.path().is_ident("transitions") {
                has_transitions = true;
                let targets: TransitionList = attr
                    .parse_args()
                    .unwrap_or_else(|e| panic!("invalid #[transitions] on `{}`: {}", variant_ident, e));
                for target in &targets.targets {
                    normal_arms.push(target_to_match_arm(variant_ident, target));
                }
            }

            if attr.path().is_ident("overrides") {
                has_overrides = true;
                let targets: TransitionList = attr
                    .parse_args()
                    .unwrap_or_else(|e| panic!("invalid #[overrides] on `{}`: {}", variant_ident, e));
                for target in &targets.targets {
                    override_arms.push(target_to_match_arm(variant_ident, target));
                }
            }
        }

        if !has_transitions {
            terminal_variants.push(variant_ident);
        }
    }

    let validate_override_method = if has_overrides {
        quote! {
            /// Validate an override transition. Includes all normal transitions
            /// plus override-only edges.
            pub fn validate_override(
                self,
                target: Self,
                role: crate::domain::role::Role,
            ) -> crate::error::Result<crate::domain::transition::Transition> {
                if let Ok(result) = self.validate_transition(target, role) {
                    return Ok(result);
                }
                use crate::domain::role::Role;
                use crate::domain::transition::Transition;
                let allowed = match (self, target) {
                    #(#override_arms,)*
                    _ => false,
                };
                if !allowed {
                    return Err(crate::error::LooprError::InvalidTransition {
                        from: format!("{:?}", self),
                        to: format!("{:?}", target),
                        role: role.to_string(),
                    });
                }
                Ok(Transition::Changed)
            }
        }
    } else {
        quote! {}
    };

    let is_terminal_body = if terminal_variants.is_empty() {
        quote! { false }
    } else {
        quote! { matches!(self, #(Self::#terminal_variants)|*) }
    };

    let expanded = quote! {
        impl #name {
            /// Validate a normal transition. Returns `Changed` if the transition
            /// is valid and moves to a new state, `Unchanged` if from == target
            /// (idempotent no-op), or `Err` if the transition is invalid.
            pub fn validate_transition(
                self,
                target: Self,
                role: crate::domain::role::Role,
            ) -> crate::error::Result<crate::domain::transition::Transition> {
                use crate::domain::role::Role;
                use crate::domain::transition::Transition;
                if self == target {
                    return Ok(Transition::Unchanged);
                }
                let allowed = match (self, target) {
                    #(#normal_arms,)*
                    _ => false,
                };
                if !allowed {
                    return Err(crate::error::LooprError::InvalidTransition {
                        from: format!("{:?}", self),
                        to: format!("{:?}", target),
                        role: role.to_string(),
                    });
                }
                Ok(Transition::Changed)
            }

            #validate_override_method

            /// True if this state has no outgoing transitions.
            pub fn is_terminal(self) -> bool {
                #is_terminal_body
            }
        }
    };

    TokenStream::from(expanded)
}

#[cfg(test)]
mod tests {
    // Proc-macro crates can't have unit tests that use the macro directly.
    // Tests live in the consuming crate (loopr).
}
