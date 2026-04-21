use super::*;

#[test]
fn sh_command_structure() {
    let cmd = sh_command("echo hi", Path::new("/tmp"));
    let inner = cmd.as_std();
    assert_eq!(inner.get_program(), "sh");
    let args: Vec<_> = inner.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(args, vec!["-c", "echo hi"]);
    assert_eq!(inner.get_current_dir(), Some(Path::new("/tmp")));
}
