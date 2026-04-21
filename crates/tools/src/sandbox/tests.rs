use super::*;

use crate::shell::sh_command;
use std::path::Path;

#[test]
fn default_is_required() {
    assert_eq!(SandboxMode::default(), SandboxMode::Required);
}

#[test]
fn serde_lowercase_required() {
    let j: SandboxMode = serde_json::from_str(r#""required""#).unwrap();
    assert_eq!(j, SandboxMode::Required);
}

#[test]
fn serde_lowercase_preferred() {
    let j: SandboxMode = serde_json::from_str(r#""preferred""#).unwrap();
    assert_eq!(j, SandboxMode::Preferred);
}

#[test]
fn serde_lowercase_off() {
    let j: SandboxMode = serde_json::from_str(r#""off""#).unwrap();
    assert_eq!(j, SandboxMode::Off);
}

#[test]
fn serde_rejects_capitalized() {
    let err = serde_json::from_str::<SandboxMode>(r#""Required""#);
    assert!(err.is_err(), "capitalized variant must not deserialize");
}

#[test]
fn detect_bwrap_functional_does_not_panic() {
    let _ = detect_bwrap_functional();
}

#[test]
fn bwrap_command_wraps_sh_command() {
    let inner = sh_command("echo hi", Path::new("/tmp"));
    let wrapped = bwrap_command(inner, Path::new("/tmp"));
    let std_cmd = wrapped.as_std();
    assert_eq!(std_cmd.get_program(), "bwrap");

    let args: Vec<String> = std_cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert!(args.contains(&"--unshare-net".into()), "args: {args:?}");
    assert!(args.contains(&"--die-with-parent".into()), "args: {args:?}");
    assert!(args.contains(&"--ro-bind".into()), "args: {args:?}");
    assert!(args.contains(&"/tmp".into()), "args: {args:?}");
    // The original `sh`, `-c`, and `echo hi` must survive the wrap verbatim.
    let sep_idx = args
        .iter()
        .position(|a| a == "--")
        .expect("-- must separate bwrap args from inner program");
    assert_eq!(args[sep_idx + 1], "sh");
    assert_eq!(args[sep_idx + 2], "-c");
    assert_eq!(args[sep_idx + 3], "echo hi");
}

#[test]
fn bwrap_command_preserves_custom_program() {
    let mut inner = tokio::process::Command::new("grep");
    inner.arg("-rn").arg("pattern").arg("/tmp");
    inner.current_dir("/tmp");
    let wrapped = bwrap_command(inner, Path::new("/tmp"));
    let args: Vec<String> = wrapped
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let sep_idx = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(args[sep_idx + 1], "grep");
    assert_eq!(args[sep_idx + 2], "-rn");
    assert_eq!(args[sep_idx + 3], "pattern");
    assert_eq!(args[sep_idx + 4], "/tmp");
}

#[test]
fn bwrap_command_uses_inner_cwd_when_set() {
    let mut inner = tokio::process::Command::new("pwd");
    inner.current_dir("/tmp/foo");
    let wrapped = bwrap_command(inner, Path::new("/other"));
    let args: Vec<String> = wrapped
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let chdir_idx = args.iter().position(|a| a == "--chdir").unwrap();
    assert_eq!(args[chdir_idx + 1], "/tmp/foo");
}
