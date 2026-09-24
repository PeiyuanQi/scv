//! Unit tests for `src/daemon.rs`.

use super::*;

#[test]
fn delegation_records_live_where_the_layout_says() {
    let home = std::path::Path::new("/tmp/scv-layout-check");
    let registry = DelegationRegistry::new(home);
    let layout = scv_client::Layout::new(home);
    assert_eq!(registry.record_dir(), layout.delegations());
    assert_eq!(registry.conversation_dir(), layout.conversations());
}
