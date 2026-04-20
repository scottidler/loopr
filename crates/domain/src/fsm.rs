use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Unchanged,
    Changed,
    Override,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Normal,
    Override,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsmErrorKind {
    NoTransition,
    RoleNotAuthorized,
}

impl FsmErrorKind {
    fn reason(self) -> &'static str {
        match self {
            FsmErrorKind::NoTransition => "no transition exists",
            FsmErrorKind::RoleNotAuthorized => "role not authorized",
        }
    }
}

#[derive(Debug)]
pub struct FsmError<S: 'static + fmt::Debug + Copy> {
    pub from: S,
    pub to: S,
    pub role: String,
    pub kind: FsmErrorKind,
    pub valid_normal: &'static [(S, &'static [&'static str])],
    pub valid_override: &'static [(S, &'static [&'static str])],
    pub context: Option<Box<FsmError<S>>>,
}

impl<S: 'static + fmt::Debug + Copy> fmt::Display for FsmError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid transition: {:?} -> {:?} (role: {}): {}",
            self.from,
            self.to,
            self.role,
            self.kind.reason()
        )?;
        if let Some(inner) = &self.context {
            write!(f, " (normal path: {})", inner.kind.reason())?;
        }
        writeln!(f)?;
        write!(f, "  valid from {:?} (normal): ", self.from)?;
        fmt_targets(f, self.valid_normal)?;
        writeln!(f)?;
        write!(f, "  valid from {:?} (override): ", self.from)?;
        fmt_targets(f, self.valid_override)?;
        Ok(())
    }
}

fn fmt_targets<S: fmt::Debug>(f: &mut fmt::Formatter<'_>, targets: &[(S, &[&str])]) -> fmt::Result {
    if targets.is_empty() {
        return write!(f, "(none)");
    }
    for (i, (target, roles)) in targets.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{:?} (", target)?;
        if roles.is_empty() {
            write!(f, "any")?;
        } else {
            for (j, role) in roles.iter().enumerate() {
                if j > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", role)?;
            }
        }
        write!(f, ")")?;
    }
    Ok(())
}

impl<S> std::error::Error for FsmError<S> where S: 'static + fmt::Debug + Copy {}
