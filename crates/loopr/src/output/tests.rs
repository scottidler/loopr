use super::*;

use serde::Serialize;

#[derive(Serialize)]
struct Sample {
    id: String,
    status: String,
}

fn sample() -> Sample {
    Sample {
        id: "pl-abc".into(),
        status: "draft".into(),
    }
}

#[test]
fn explicit_json_wins_over_tty_default() {
    assert_eq!(resolve_inner(Some(Format::Json), true), Format::Json);
    assert_eq!(resolve_inner(Some(Format::Json), false), Format::Json);
}

#[test]
fn explicit_yaml_wins_over_pipe_default() {
    assert_eq!(resolve_inner(Some(Format::Yaml), true), Format::Yaml);
    assert_eq!(resolve_inner(Some(Format::Yaml), false), Format::Yaml);
}

#[test]
fn tty_default_is_yaml() {
    assert_eq!(resolve_inner(None, true), Format::Yaml);
}

#[test]
fn pipe_default_is_json() {
    assert_eq!(resolve_inner(None, false), Format::Json);
}

#[test]
fn render_json_emits_pretty_parseable() {
    let out = render(&sample(), Format::Json).unwrap();
    assert!(out.contains("\"id\""));
    assert!(out.contains("\"pl-abc\""));
    let _: serde_json::Value = serde_json::from_str(&out).expect("json round-trip");
}

#[test]
fn render_yaml_emits_readable_parseable() {
    let out = render(&sample(), Format::Yaml).unwrap();
    assert!(out.contains("id: pl-abc"));
    assert!(out.contains("status: draft"));
    let _: serde_yaml::Value = serde_yaml::from_str(&out).expect("yaml round-trip");
}
