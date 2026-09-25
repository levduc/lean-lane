//! SPEC.md Appendix A vectors V1-V8 against the ENGINE.

#[test]
fn spec_appendix_a_vectors_hold_for_the_engine() {
    match lean_diff::golden::check_all() {
        Ok(n) => eprintln!("golden: {n} assertions"),
        Err(e) => panic!("golden vector failed: {e}"),
    }
}
