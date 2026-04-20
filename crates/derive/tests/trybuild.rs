#[test]
fn compile_fail_cases_reject_with_spanned_errors() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile-fail/*.rs");
}
