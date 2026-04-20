use std::collections::HashMap;

use syn::spanned::Spanned;
use syn::{Error, Result, Type};

use super::parse::RecordIr;

pub(crate) fn validate(ir: &RecordIr) -> Result<()> {
    check_updated_at_is_i64(&ir.updated_at_ty)?;
    check_no_duplicate_map_keys(ir)?;
    Ok(())
}

fn check_updated_at_is_i64(ty: &Type) -> Result<()> {
    if let Type::Path(type_path) = ty
        && let Some(last) = type_path.path.segments.last()
        && last.ident == "i64"
        && last.arguments.is_empty()
    {
        return Ok(());
    }
    Err(Error::new(ty.span(), "`updated_at` field must be of type `i64`"))
}

fn check_no_duplicate_map_keys(ir: &RecordIr) -> Result<()> {
    let mut seen: HashMap<String, ()> = HashMap::new();
    for field in &ir.indexed_fields {
        if seen.insert(field.map_key.clone(), ()).is_some() {
            return Err(Error::new(
                field.map_key_span,
                format!(
                    "duplicate indexed map key `{}`; each indexed field must produce a unique map key",
                    field.map_key
                ),
            ));
        }
    }
    Ok(())
}
