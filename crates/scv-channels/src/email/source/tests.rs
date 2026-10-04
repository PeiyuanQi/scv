//! Unit tests for `src/email/source.rs`.

use super::*;
use crate::email::test_support::meta;

#[test]
fn cut_keeps_what_fits_and_marks_what_does_not() {
    let mut short = "short".to_owned();
    cut(&mut short, 5);
    assert_eq!(short, "short");
    let mut long = "éééééé".to_owned();
    cut(&mut long, 8);
    assert!(long.len() <= 8, "{long}");
    assert_eq!(long, "éé…");
}

#[test]
fn bounded_cuts_every_field_and_list_of_hostile_metadata() {
    let huge = |fill: &str| fill.repeat(100_000);
    let address = || Address {
        name: huge("n"),
        address: huge("a"),
    };
    let mut hostile = meta(1, "", "");
    hostile.subject = huge("s");
    hostile.from = Some(address());
    hostile.reply_to = Some(address());
    hostile.to = (0..1000).map(|_| address()).collect();
    hostile.cc = (0..1000).map(|_| address()).collect();
    hostile.signals.list_id = Some(huge("l"));
    hostile.signals.precedence = Some(huge("p"));
    hostile.signals.auto_submitted = Some(huge("x"));
    hostile.category = Some(huge("c"));
    let part = hostile.text.as_mut().unwrap();
    part.mime = huge("m");
    part.charset = Some(huge("u"));
    hostile.attachments = (0..100)
        .map(|_| AttachmentInfo {
            name: huge("f"),
            mime: huge("t"),
            size: 1,
        })
        .collect();

    let bounded = hostile.clone().bounded();
    assert_eq!(bounded.source, hostile.source);
    assert_eq!(bounded.identity, hostile.identity);
    assert!(bounded.subject.len() <= MAX_SUBJECT_BYTES && bounded.subject.ends_with(CUT));
    assert_eq!(bounded.to.len(), MAX_RECIPIENTS);
    assert_eq!(bounded.cc.len(), MAX_RECIPIENTS);
    let addresses = bounded
        .from
        .iter()
        .chain(bounded.reply_to.iter())
        .chain(bounded.to.iter())
        .chain(bounded.cc.iter());
    for address in addresses {
        assert!(address.name.len() <= MAX_NAME_BYTES);
        assert!(address.address.len() <= MAX_ADDRESS_BYTES);
    }
    let signals = &bounded.signals;
    assert!(signals.list_id.as_ref().unwrap().len() <= MAX_NAME_BYTES);
    for label in [
        signals.precedence.as_ref(),
        signals.auto_submitted.as_ref(),
        bounded.category.as_ref(),
        Some(&bounded.text.as_ref().unwrap().mime),
        bounded.text.as_ref().unwrap().charset.as_ref(),
        Some(&bounded.attachments[0].mime),
    ] {
        assert!(label.unwrap().len() <= MAX_LABEL_BYTES);
    }
    assert_eq!(bounded.attachments.len(), MAX_ATTACHMENTS);
    assert!(bounded.attachments[0].name.len() <= MAX_NAME_BYTES);

    // Metadata within its bounds is left as it is.
    let ordinary = meta(2, "alice@example.com", "Invoice");
    assert_eq!(ordinary.clone().bounded(), ordinary);
}

#[test]
fn api_ids_reject_dot_segments_and_accept_ordinary_ids() {
    assert!(SourceRef::valid_api_id("msg.1_A-b="));
    assert!(!SourceRef::valid_api_id("."));
    assert!(!SourceRef::valid_api_id(".."));
    assert!(!SourceRef::valid_api_id("a..b"));
    assert!(!SourceRef::valid_api_id(""));
    assert!(!SourceRef::valid_api_id("ab/cd"));
}
