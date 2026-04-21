use super::*;

#[test]
fn default_is_required() {
    assert_eq!(SandboxMode::default(), SandboxMode::Required);
}

#[test]
fn serde_lowercase_required() {
    let m: SandboxMode = serde_yaml_required();
    assert_eq!(m, SandboxMode::Required);
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

fn serde_yaml_required() -> SandboxMode {
    serde_json::from_str(r#""required""#).unwrap()
}
