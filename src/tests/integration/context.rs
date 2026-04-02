#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use crate::domain::learning::{Learning, LearningScope};
use crate::test_util::TestDir;

use super::fixtures::*;

#[test]
fn test_context_builder_role_filtering() {
    use crate::domain::role::Role;

    let mut learnings = HashMap::new();

    // Create learnings for specific roles
    let mut l1 = Learning::new("wi-1".into(), LearningScope::Work, "Impl insight".into());
    l1.applicable_roles = Some(vec![Role::Implementer]);
    l1.confidence = 0.8;
    learnings.insert(l1.id.clone(), l1);

    let mut l2 = Learning::new("wi-1".into(), LearningScope::Work, "Review insight".into());
    l2.applicable_roles = Some(vec![Role::Reviewer]);
    l2.confidence = 0.8;
    learnings.insert(l2.id.clone(), l2);

    let mut l3 = Learning::new("wi-1".into(), LearningScope::Global, "Global insight".into());
    l3.applicable_roles = None; // All roles
    l3.confidence = 0.8;
    learnings.insert(l3.id.clone(), l3);

    // Select learnings for Implementer
    let scope_ids = [("wi-1", LearningScope::Work)];
    let impl_learnings = crate::agents::context::select_learnings(&learnings, &scope_ids, Role::Implementer, 0.3, 100);

    // Should include implementer-specific and global, but NOT reviewer-specific
    assert!(
        impl_learnings.iter().any(|l| l.content == "Impl insight"),
        "should include implementer learning"
    );
    assert!(
        impl_learnings.iter().any(|l| l.content == "Global insight"),
        "should include global learning"
    );
    assert!(
        !impl_learnings.iter().any(|l| l.content == "Review insight"),
        "should NOT include reviewer learning"
    );

    // Select learnings for Reviewer
    let rev_learnings = crate::agents::context::select_learnings(&learnings, &scope_ids, Role::Reviewer, 0.3, 100);

    assert!(
        rev_learnings.iter().any(|l| l.content == "Review insight"),
        "should include reviewer learning"
    );
    assert!(
        rev_learnings.iter().any(|l| l.content == "Global insight"),
        "should include global learning"
    );
    assert!(
        !rev_learnings.iter().any(|l| l.content == "Impl insight"),
        "should NOT include implementer learning"
    );
}

#[test]
fn test_researcher_path_sandboxing() {
    use std::path::Path;

    let repo_root = Path::new("/tmp/test-repo");
    let _agent_dir = TestDir::new("loopr-intg-logger");
    let agent_log = test_agent_logger(&_agent_dir);

    // Valid relative path
    assert!(
        crate::agents::researcher::validate_path(repo_root, "src/main.rs", &agent_log).is_ok(),
        "relative path should be valid"
    );

    // Absolute path rejected
    assert!(
        crate::agents::researcher::validate_path(repo_root, "/etc/passwd", &agent_log).is_err(),
        "absolute path should be rejected"
    );

    // Path traversal rejected
    assert!(
        crate::agents::researcher::validate_path(repo_root, "../../../etc/passwd", &agent_log).is_err(),
        "traversal path should be rejected"
    );

    // Denied file patterns
    assert!(
        crate::agents::researcher::validate_path(repo_root, ".env", &agent_log).is_err(),
        ".env should be denied"
    );
    assert!(
        crate::agents::researcher::validate_path(repo_root, "keys/server.key", &agent_log).is_err(),
        "*.key should be denied"
    );
    assert!(
        crate::agents::researcher::validate_path(repo_root, "certs/server.pem", &agent_log).is_err(),
        "*.pem should be denied"
    );
    assert!(
        crate::agents::researcher::validate_path(repo_root, "credentials.json", &agent_log).is_err(),
        "credentials.* should be denied"
    );
}
