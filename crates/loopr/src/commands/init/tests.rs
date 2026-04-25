#![allow(clippy::unwrap_used)]

use std::fs;

use super::seed_prompts;

#[test]
fn seed_into_empty_target_writes_all_files() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    let outcome = seed_prompts(dest.path(), baked, false).unwrap();
    assert!(outcome.written > 0, "expected to write at least one file");
    assert_eq!(outcome.preserved, 0);
    // The implementer system prompt is the load-bearing file: assert it landed.
    let implementer = dest.path().join("agents/implementer/system.pmt");
    assert!(implementer.exists(), "expected {implementer:?}");
    let content = fs::read_to_string(&implementer).unwrap();
    assert!(content.contains("You are an Implementer agent"));
}

#[test]
fn seed_skips_gitkeep_files_but_creates_their_parents() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    let _ = seed_prompts(dest.path(), baked, false).unwrap();
    let chat_dir = dest.path().join("chat");
    let plan_dir = dest.path().join("decompose/plan");
    assert!(chat_dir.is_dir(), "chat/ dir should exist");
    assert!(plan_dir.is_dir(), "decompose/plan/ dir should exist");
    assert!(!chat_dir.join(".gitkeep").exists(), ".gitkeep must NOT be written");
    assert!(!plan_dir.join(".gitkeep").exists(), ".gitkeep must NOT be written");
}

#[test]
fn seed_preserves_existing_edits_under_default_merge() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    // First seed populates everything.
    seed_prompts(dest.path(), baked, false).unwrap();
    // User edits one file.
    let target_file = dest.path().join("agents/implementer/system.pmt");
    fs::write(&target_file, "USER EDITED CONTENT").unwrap();
    // Re-seed without force.
    let outcome = seed_prompts(dest.path(), baked, false).unwrap();
    assert_eq!(outcome.written, 0, "merge mode must not overwrite anything");
    assert!(outcome.preserved > 0);
    let content = fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "USER EDITED CONTENT");
}

#[test]
fn seed_force_overwrites_existing_edits() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    seed_prompts(dest.path(), baked, false).unwrap();
    let target_file = dest.path().join("agents/implementer/system.pmt");
    fs::write(&target_file, "USER EDITED CONTENT").unwrap();
    let outcome = seed_prompts(dest.path(), baked, true).unwrap();
    assert!(
        outcome.written > 0,
        "force mode must overwrite at least the edited file"
    );
    let content = fs::read_to_string(&target_file).unwrap();
    assert!(content.contains("You are an Implementer agent"));
    assert!(!content.contains("USER EDITED CONTENT"));
}
