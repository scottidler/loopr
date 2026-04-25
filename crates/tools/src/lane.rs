#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Lane {
    Local,
    Net,
    Heavy,
}

impl Lane {
    /// Stable lowercase string for span fields and log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Local => "local",
            Lane::Net => "net",
            Lane::Heavy => "heavy",
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct LanePolicy {
    pub lane: Lane,
    pub max_slots: usize,
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub sandbox_net: bool,
}

impl LanePolicy {
    pub const fn local() -> Self {
        Self {
            lane: Lane::Local,
            max_slots: 10,
            default_timeout_secs: 30,
            max_timeout_secs: 60,
            sandbox_net: true,
        }
    }

    pub const fn net() -> Self {
        Self {
            lane: Lane::Net,
            max_slots: 5,
            default_timeout_secs: 60,
            max_timeout_secs: 120,
            sandbox_net: false,
        }
    }

    pub const fn heavy() -> Self {
        Self {
            lane: Lane::Heavy,
            max_slots: 1,
            default_timeout_secs: 600,
            max_timeout_secs: 1800,
            sandbox_net: false,
        }
    }

    pub const fn for_lane(lane: Lane) -> Self {
        match lane {
            Lane::Local => Self::local(),
            Lane::Net => Self::net(),
            Lane::Heavy => Self::heavy(),
        }
    }
}

pub fn classify(tool_name: &str) -> Lane {
    match tool_name {
        "read" | "write" | "edit" | "grep" | "glob" => Lane::Local,
        "bash" => Lane::Net,
        _ => Lane::Heavy,
    }
}

#[cfg(test)]
mod tests;
