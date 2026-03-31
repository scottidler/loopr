/// The three execution lanes for tool subprocess isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// No network access. Sandboxed via bwrap --unshare-net.
    /// For filesystem-only tools: read, write, edit, glob, grep, find, list, tree.
    Local,
    /// Network access allowed. No sandbox wrapping.
    /// For tools that need HTTP: fetch, search, shell.
    Net,
    /// Resource-intensive. Network allowed. Slot-limited to 1.
    /// For builds, tests, linting: configured tools (cargo build, npm test, otto ci).
    Heavy,
}

/// Lane configuration - slots, timeouts, sandbox settings.
#[derive(Debug, Clone)]
pub struct LanePolicy {
    pub lane: Lane,
    pub max_slots: usize,
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub sandbox_net: bool,
}

impl LanePolicy {
    pub fn local() -> Self {
        Self {
            lane: Lane::Local,
            max_slots: 10,
            default_timeout_secs: 30,
            max_timeout_secs: 60,
            sandbox_net: true,
        }
    }

    pub fn net() -> Self {
        Self {
            lane: Lane::Net,
            max_slots: 5,
            default_timeout_secs: 60,
            max_timeout_secs: 120,
            sandbox_net: false,
        }
    }

    pub fn heavy() -> Self {
        Self {
            lane: Lane::Heavy,
            max_slots: 1,
            default_timeout_secs: 600,
            max_timeout_secs: 1800,
            sandbox_net: false,
        }
    }
}

/// Classify a tool into its execution lane.
pub fn classify(tool_name: &str) -> Lane {
    match tool_name {
        // Filesystem-only builtins - no network needed
        "read" | "write" | "edit" | "list" | "tree" | "glob" | "grep" | "find" => Lane::Local,

        // Network-required builtins
        "fetch" | "search" => Lane::Net,

        // In-process tools - no subprocess spawned, lane is irrelevant
        // (classification exists for completeness; these never hit the LaneRouter)
        "todo" | "plan" | "slash" | "delegate" => Lane::Local,

        // Shell tool - defaults to Net (conservative)
        "shell" => Lane::Net,

        // Configured project tools (test, build, lint) - always Heavy
        _ => Lane::Heavy,
    }
}

impl std::fmt::Display for Lane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lane::Local => write!(f, "local"),
            Lane::Net => write!(f, "net"),
            Lane::Heavy => write!(f, "heavy"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_filesystem_tools() {
        assert_eq!(classify("read"), Lane::Local);
        assert_eq!(classify("write"), Lane::Local);
        assert_eq!(classify("edit"), Lane::Local);
        assert_eq!(classify("glob"), Lane::Local);
        assert_eq!(classify("grep"), Lane::Local);
        assert_eq!(classify("find"), Lane::Local);
        assert_eq!(classify("list"), Lane::Local);
        assert_eq!(classify("tree"), Lane::Local);
    }

    #[test]
    fn test_classify_network_tools() {
        assert_eq!(classify("fetch"), Lane::Net);
        assert_eq!(classify("search"), Lane::Net);
        assert_eq!(classify("shell"), Lane::Net);
    }

    #[test]
    fn test_classify_in_process_tools() {
        assert_eq!(classify("todo"), Lane::Local);
        assert_eq!(classify("plan"), Lane::Local);
        assert_eq!(classify("slash"), Lane::Local);
        assert_eq!(classify("delegate"), Lane::Local);
    }

    #[test]
    fn test_classify_configured_tools() {
        assert_eq!(classify("test"), Lane::Heavy);
        assert_eq!(classify("build"), Lane::Heavy);
        assert_eq!(classify("lint"), Lane::Heavy);
        assert_eq!(classify("cargo-test"), Lane::Heavy);
        assert_eq!(classify("unknown-tool"), Lane::Heavy);
    }

    #[test]
    fn test_lane_policy_defaults() {
        let local = LanePolicy::local();
        assert_eq!(local.max_slots, 10);
        assert_eq!(local.default_timeout_secs, 30);
        assert!(local.sandbox_net);

        let net = LanePolicy::net();
        assert_eq!(net.max_slots, 5);
        assert_eq!(net.default_timeout_secs, 60);
        assert!(!net.sandbox_net);

        let heavy = LanePolicy::heavy();
        assert_eq!(heavy.max_slots, 1);
        assert_eq!(heavy.default_timeout_secs, 600);
        assert!(!heavy.sandbox_net);
    }

    #[test]
    fn test_lane_display() {
        assert_eq!(format!("{}", Lane::Local), "local");
        assert_eq!(format!("{}", Lane::Net), "net");
        assert_eq!(format!("{}", Lane::Heavy), "heavy");
    }
}
