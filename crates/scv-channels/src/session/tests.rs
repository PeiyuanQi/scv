//! Unit tests for `src/session.rs`.

use super::*;

#[test]
fn accumulated_reply_is_cut_with_a_note_at_the_byte_limit() {
    let limit = 32;
    let mut answer = String::new();
    append_capped(&mut answer, "hello ", limit);
    assert_eq!(answer, "hello ");
    append_capped(&mut answer, &"é".repeat(40), limit);
    assert!(answer.len() <= limit, "{} bytes", answer.len());
    assert!(answer.starts_with("hello é"));
    assert!(answer.ends_with(TRUNCATED_NOTE));
    let cut = answer.clone();
    append_capped(&mut answer, "more", limit);
    assert_eq!(answer, cut, "nothing follows the truncation note");
}

#[test]
fn a_direct_report_names_the_error_and_quotes_each_reply() {
    let report = |job: &str| JobReport {
        job: job.into(),
        agent: "codex".into(),
        task: "Land it".into(),
        status: scv_protocol::JobStatus::Completed,
        session: None,
        reply: format!("{job} landed.\n"),
    };
    assert_eq!(
        direct_report("provider returned HTTP 503", 1, &[report("job-1")]),
        "A background job finished, but the model could not report it, so here is what the \
         agent replied, unedited.\nError: provider returned HTTP 503\n\n\
         job-1 (codex): completed\nTask: Land it\njob-1 landed."
    );
    let both = direct_report("timeout", 3, &[report("job-1"), report("job-2")]);
    assert!(
        both.starts_with(
            "2 background jobs finished, but the model could not report them (3 tries), so"
        ),
        "{both}"
    );
    assert!(
        both.ends_with("job-1 landed.\n\njob-2 (codex): completed\nTask: Land it\njob-2 landed.")
    );
}
