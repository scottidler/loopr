use super::*;

#[tokio::test]
async fn current_is_default_outside_any_scope() {
    let cc = CallContext::current();
    assert_eq!(cc.plan_id, None);
    assert_eq!(cc.work_id, None);
    assert_eq!(cc.role, None);
}

#[tokio::test]
async fn scope_installs_context_for_the_future() {
    let ctx = CallContext {
        plan_id: Some("p-1".to_string()),
        work_id: Some("wk-1".to_string()),
        role: Some("implementer".to_string()),
    };
    let observed = CallContext::scope(ctx, async { CallContext::current() }).await;
    assert_eq!(observed.plan_id.as_deref(), Some("p-1"));
    assert_eq!(observed.work_id.as_deref(), Some("wk-1"));
    assert_eq!(observed.role.as_deref(), Some("implementer"));
    // The scope is gone once the future completes.
    assert_eq!(CallContext::current().plan_id, None);
}

#[tokio::test]
async fn nested_scope_shadows_outer() {
    let outer = CallContext {
        role: Some("director".to_string()),
        ..CallContext::default()
    };
    let inner = CallContext {
        role: Some("implementer".to_string()),
        ..CallContext::default()
    };
    let role = CallContext::scope(outer, async {
        CallContext::scope(inner, async { CallContext::current().role }).await
    })
    .await;
    assert_eq!(role.as_deref(), Some("implementer"));
}
