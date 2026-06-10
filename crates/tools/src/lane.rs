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
    /// Wrap this lane's subprocesses in bwrap for filesystem containment.
    /// Replaces the old `sandbox_net` flag, which conflated "wrap" with
    /// "unshare network" (Phase-5 finding 4).
    pub sandbox: bool,
    /// When sandboxed, allow network access inside the sandbox (omit
    /// `--unshare-net`). Ignored when `sandbox` is false. `Local` blocks
    /// network; `Net` (Bash) allows it but keeps filesystem containment.
    pub network: bool,
}

impl LanePolicy {
    pub const fn local() -> Self {
        Self {
            lane: Lane::Local,
            max_slots: 10,
            default_timeout_secs: 30,
            max_timeout_secs: 60,
            sandbox: true,
            network: false,
        }
    }

    pub const fn net() -> Self {
        Self {
            lane: Lane::Net,
            max_slots: 5,
            default_timeout_secs: 60,
            max_timeout_secs: 120,
            // Finding 4: Bash now runs filesystem-contained under bwrap WITH
            // network. The vision's "bwrap contains the Bash blast radius"
            // was previously false (bwrap wrapped only the Local lane); the
            // denylist remains as defense-in-depth.
            sandbox: true,
            network: true,
        }
    }

    pub const fn heavy() -> Self {
        Self {
            lane: Lane::Heavy,
            max_slots: 1,
            default_timeout_secs: 600,
            max_timeout_secs: 1800,
            // Builds need an unconfined filesystem (writes to ~/.cargo,
            // /tmp build dirs, toolchain caches outside the worktree), so
            // Heavy stays unsandboxed.
            sandbox: false,
            network: true,
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
