//! Unit tests for `src/cli/config/overview.rs`.

use super::*;

#[test]
fn modes_and_missing_files_are_described() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(describe(&file), "0600");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(describe(&file), "0644, readable by others");
    assert_eq!(describe(&dir.path().join("missing")), "missing");
}

#[test]
fn media_limits_show_where_each_was_set() {
    let defaults = scv_channels::media::MediaSettings::default();
    assert_eq!(
        media_line(&defaults, &|_| false),
        "media owner_max_mib = 50 [default], others_image_max_mib = 5 [default], keep_days = 7 \
         [default]"
    );
    let raised = scv_channels::media::MediaSettings {
        owner_max_mib: 100,
        ..defaults
    };
    assert_eq!(
        media_line(&raised, &|key| key == "owner_max_mib"),
        "media owner_max_mib = 100 [config.toml], others_image_max_mib = 5 [default], \
         keep_days = 7 [default]"
    );
}

#[test]
fn an_owner_only_account_without_an_owner_answers_nobody() {
    use scv_channels::state::Senders;
    assert_eq!(
        answers(Senders::Owner, Some(true)),
        "answers only its owner"
    );
    assert_eq!(answers(Senders::Owner, None), "answers only its owner");
    assert_eq!(
        answers(Senders::Owner, Some(false)),
        "answers nobody (only its owner, and no owner is recorded)"
    );
    assert_eq!(answers(Senders::Anyone, Some(false)), "answers anyone");
}
