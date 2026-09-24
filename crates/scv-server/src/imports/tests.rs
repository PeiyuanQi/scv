//! Unit tests for `src/imports.rs`.

use super::*;

#[test]
fn a_changed_source_is_reported_until_imported_again() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    let files = vec!["config.toml".to_owned()];
    std::fs::write(source.path().join("config.toml"), "a = 1\n").unwrap();
    assert!(check(&layout, "grok", || None).unwrap().is_none());
    let files_source = Source::Files {
        dir: source.path().to_owned(),
        files: files.clone(),
    };
    let digest = digest_files(source.path(), &files).unwrap();
    record(&layout, "grok", files_source.clone(), digest).unwrap();
    let status = check(&layout, "grok", || None).unwrap().unwrap();
    assert_eq!(status.freshness, Freshness::Current);
    assert!(status.describe("grok", now()).contains("up to date"));

    std::fs::write(source.path().join("config.toml"), "a = 2\n").unwrap();
    let status = check(&layout, "grok", || None).unwrap().unwrap();
    assert_eq!(status.freshness, Freshness::Changed);
    assert!(
        status
            .describe("grok", now())
            .contains("run `scv agents import grok`")
    );

    let digest = digest_files(source.path(), &files).unwrap();
    record(&layout, "grok", files_source, digest).unwrap();
    assert_eq!(
        check(&layout, "grok", || None).unwrap().unwrap().freshness,
        Freshness::Current
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(layout.imports()), 0o700);
    assert_eq!(mode(layout.imports().join("grok.json")), 0o600);
}

#[test]
fn provider_imports_compare_the_digest_the_caller_computes() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    let digest = digest_value(&("https://example.test/v1", "model", "key")).unwrap();
    record(&layout, "scv", Source::ScvProvider, digest.clone()).unwrap();
    let text = std::fs::read_to_string(layout.imports().join("scv.json")).unwrap();
    assert!(!text.contains("key\""), "{text}");
    for (current, expected) in [
        (Some(digest), Freshness::Current),
        (Some("other".into()), Freshness::Changed),
        (None, Freshness::Unknown),
    ] {
        let status = check(&layout, "scv", || current).unwrap().unwrap();
        assert_eq!(status.freshness, expected);
    }
}
