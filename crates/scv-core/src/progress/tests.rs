//! Unit tests for `src/progress.rs`.

use super::*;

#[test]
fn progress_lines_are_single_bounded_lines() {
    assert_eq!(
        progress_line("  run\n\tcargo \u{7}test  "),
        "run cargo test"
    );
    let long = progress_line(&"x".repeat(1000));
    assert!(long.len() <= MAX_PROGRESS_LINE_BYTES && long.ends_with(PROGRESS_ELIDED));
    let wide = progress_line(&"é".repeat(300));
    assert!(wide.len() <= MAX_PROGRESS_LINE_BYTES);
    let discard = ProgressSink::default();
    discard.report("ignored");
    assert!(!discard.is_enabled() && discard.take().is_none());
}

#[test]
fn progress_events_keep_the_newest_lines_within_the_limit() {
    let progress = ProgressSink::buffered();
    assert!(progress.take().is_none());
    progress.report("first");
    progress.report("second");
    assert_eq!(progress.take().as_deref(), Some("first\nsecond"));
    assert!(progress.take().is_none());
    for index in 0..50 {
        progress.report(&format!("{index:03} {}", "y".repeat(96)));
    }
    let text = progress.take().unwrap();
    assert!(text.len() <= MAX_PROGRESS_EVENT_BYTES, "{}", text.len());
    assert!(text.starts_with(&format!("{PROGRESS_ELIDED}\n")));
    assert!(text.lines().last().unwrap().starts_with("049 "));
    progress.report("after");
    assert_eq!(progress.take().as_deref(), Some("after"));
}
