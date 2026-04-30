use strum::Display;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display)]
#[strum(serialize_all = "kebab-case")]
pub enum Role {
    Reactor,
    Integrator,
    Implementer,
    Reviewer,
    Researcher,
    Decomposer,
    Director,
}
