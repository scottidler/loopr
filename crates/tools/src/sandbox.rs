use serde::Deserialize;

#[derive(Debug, Default, Deserialize, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    #[default]
    Required,
    Preferred,
    Off,
}

#[cfg(test)]
mod tests;
