//! Unit tests for `src/lib.rs`.

#[test]
fn clients_and_tools_agree_on_the_depth_variable() {
    assert_eq!(
        scv_client::DELEGATION_DEPTH_VARIABLE,
        scv_tools::delegation::DEPTH_VARIABLE
    );
}
