//! Unit tests for `src/intake.rs`.

use super::*;
use crate::Message;

fn text(id: &str, sender: &str, group: Option<&str>) -> Inbound {
    Inbound::Text(Message::text(id, sender, "hello", "context", group))
}

fn conversation(age: u64, watching: bool) -> Conversation {
    let (jobs, _queue) = mpsc::unbounded_channel();
    Conversation {
        jobs,
        last_used: Instant::now()
            .checked_sub(Duration::from_secs(age))
            .unwrap(),
        watching: Arc::new(AtomicBool::new(watching)),
    }
}

fn claim(id: &str, key: &str) -> state::InFlight {
    state::InFlight {
        message_id: id.into(),
        to_user_id: key.into(),
        context_token: "context".into(),
        key: key.into(),
    }
}

fn tool_owner(user_id: &str) -> ToolOwner {
    ToolOwner {
        user_id: user_id.into(),
        turn_timeout: Duration::from_secs(3600),
    }
}

/// What the bridge knows with no claims, no conversations, and no owner, on
/// an account that answers anyone.
fn quiet<'a>(
    state: &'a state::BridgeState,
    conversations: &'a HashMap<String, Conversation>,
) -> Intake<'a> {
    Intake {
        state,
        conversations,
        owner: None,
        tool_owner: None,
        senders: Senders::Anyone,
        question: None,
    }
}

/// What an owner-only account owned by `owner` knows.
fn owned<'a>(
    state: &'a state::BridgeState,
    conversations: &'a HashMap<String, Conversation>,
    owner: Option<&'a str>,
) -> Intake<'a> {
    Intake {
        owner,
        senders: Senders::Owner,
        ..quiet(state, conversations)
    }
}

#[test]
fn seen_and_unanswerable_messages_are_only_marked_seen() {
    let state = state::BridgeState {
        seen: vec!["old".into()],
        ..Default::default()
    };
    let conversations = HashMap::new();
    let intake = quiet(&state, &conversations);
    assert_eq!(
        classify(&text("old", "sender", None), &intake),
        Verdict::Ignore
    );
    assert_eq!(
        classify(
            &Inbound::Ignored {
                id: "system".into()
            },
            &intake
        ),
        Verdict::Ignore
    );
}

#[test]
fn a_claimed_or_undelivered_message_never_runs_a_second_turn() {
    let state = state::BridgeState {
        in_flight: vec![claim("running", "sender")],
        pending: vec![crate::new_pending(
            "answered", "sender", "context", "reply", 64,
        )],
        ..Default::default()
    };
    let conversations = HashMap::new();
    let intake = quiet(&state, &conversations);
    for id in ["running", "answered"] {
        assert_eq!(
            classify(&text(id, "sender", None), &intake),
            Verdict::Claimed
        );
    }
}

#[test]
fn direct_chats_and_each_sender_in_a_group_are_separate_conversations() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let intake = quiet(&state, &conversations);
    let direct = text("m1", "sender", None);
    let grouped = text("m2", "sender", Some("group"));
    for (inbound, key) in [(&direct, "sender"), (&grouped, "group\0sender")] {
        let Verdict::Turn {
            sender,
            owner,
            limit,
            evict,
        } = classify(inbound, &intake)
        else {
            panic!("a message to answer runs a turn");
        };
        assert_eq!(sender.key, key);
        assert!(!sender.owner_chat && !owner);
        assert_eq!((limit, evict), (TURN_TIMEOUT, None));
    }
}

#[test]
fn only_the_owners_direct_chat_gets_tools_and_the_owner_limit() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let granted = tool_owner("owner");
    let intake = Intake {
        owner: Some("owner"),
        tool_owner: Some(&granted),
        ..quiet(&state, &conversations)
    };
    let turn = |inbound: &Inbound| match classify(inbound, &intake) {
        Verdict::Turn {
            sender,
            owner,
            limit,
            ..
        } => (sender.owner_chat, owner, limit),
        other => panic!("expected a turn, got {other:?}"),
    };
    assert_eq!(
        turn(&text("m1", "owner", None)),
        (true, true, Duration::from_secs(3600))
    );
    // The owner's group messages never carry owner authority.
    assert_eq!(
        turn(&text("m2", "owner", Some("group"))),
        (false, false, TURN_TIMEOUT)
    );
    assert_eq!(
        turn(&text("m3", "other", None)),
        (false, false, TURN_TIMEOUT)
    );
}

#[test]
fn the_account_owner_is_known_without_the_tool_grant() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let intake = Intake {
        owner: Some("owner"),
        ..quiet(&state, &conversations)
    };
    let inbound = text("m1", "owner", None);
    let Verdict::Turn { sender, owner, .. } = classify(&inbound, &intake) else {
        panic!("a message to answer runs a turn");
    };
    assert!(sender.owner_chat);
    assert!(!owner, "tools need the grant");
}

#[test]
fn messages_beyond_the_claim_and_queue_limits_get_the_busy_reply() {
    let conversations = HashMap::new();
    let full = state::BridgeState {
        in_flight: (0..MAX_CLAIMS)
            .map(|index| claim(&index.to_string(), &format!("sender-{index}")))
            .collect(),
        ..Default::default()
    };
    let inbound = text("new", "sender", None);
    let Verdict::Busy(sender) = classify(&inbound, &quiet(&full, &conversations)) else {
        panic!("no claim is left");
    };
    assert_eq!(sender.key, "sender");
    let queued = state::BridgeState {
        in_flight: (0..MAX_QUEUED_PER_CONVERSATION)
            .map(|index| claim(&index.to_string(), "sender"))
            .collect(),
        ..Default::default()
    };
    assert!(matches!(
        classify(&inbound, &quiet(&queued, &conversations)),
        Verdict::Busy(_)
    ));
    // Another conversation still has room.
    assert!(matches!(
        classify(
            &text("other", "other", None),
            &quiet(&queued, &conversations)
        ),
        Verdict::Turn { .. }
    ));
}

#[test]
fn a_full_session_table_makes_room_only_by_closing_an_idle_conversation() {
    let state = state::BridgeState::default();
    let mut conversations: HashMap<String, Conversation> = (0..MAX_SESSIONS)
        .map(|index| (format!("busy-{index}"), conversation(10, true)))
        .collect();
    let inbound = text("new", "sender", None);
    assert!(matches!(
        classify(&inbound, &quiet(&state, &conversations)),
        Verdict::Busy(_)
    ));
    // A running conversation is never evicted for itself.
    assert!(matches!(
        classify(
            &text("again", "busy-0", None),
            &quiet(&state, &conversations)
        ),
        Verdict::Turn { evict: None, .. }
    ));
    conversations.insert("busy-1".into(), conversation(100, false));
    let Verdict::Turn { evict, .. } = classify(&inbound, &quiet(&state, &conversations)) else {
        panic!("an idle conversation makes room");
    };
    assert_eq!(evict.as_deref(), Some("busy-1"));
}

#[test]
fn a_full_session_table_never_closes_a_conversation_with_background_work() {
    let now = Instant::now();
    let conversation = |age: u64, watching: bool| {
        let (jobs, _queue) = mpsc::unbounded_channel();
        Conversation {
            jobs,
            last_used: now.checked_sub(Duration::from_secs(age)).unwrap(),
            watching: Arc::new(AtomicBool::new(watching)),
        }
    };
    let conversations = HashMap::from([
        // The oldest runs background jobs; the next has a message waiting.
        ("jobs".to_owned(), conversation(300, true)),
        ("queued".to_owned(), conversation(200, false)),
        ("idle".to_owned(), conversation(100, false)),
        ("recent".to_owned(), conversation(10, false)),
    ]);
    let waiting = |key: &str| usize::from(key == "queued");
    assert_eq!(evictable(&conversations, waiting).as_deref(), Some("idle"));
    // Its jobs finish: it is the least recently used idle one again.
    conversations["jobs"]
        .watching
        .store(false, Ordering::Release);
    assert_eq!(evictable(&conversations, waiting).as_deref(), Some("jobs"));
    // Nothing is closable while every conversation is busy.
    for conversation in conversations.values() {
        conversation.watching.store(true, Ordering::Release);
    }
    assert_eq!(evictable(&conversations, waiting), None);
}

#[test]
fn an_owner_only_account_answers_its_owner_and_drops_everyone_else() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let intake = owned(&state, &conversations, Some("owner"));
    let direct = text("m1", "owner", None);
    let Verdict::Turn { sender, owner, .. } = classify(&direct, &intake) else {
        panic!("the owner is answered");
    };
    assert!(sender.owner_chat && !owner);
    // In a group the owner is still answered, tool-free, on the group's
    // conversation.
    let grouped = text("m2", "owner", Some("group"));
    let Verdict::Turn { sender, owner, .. } = classify(&grouped, &intake) else {
        panic!("the owner's group message is answered");
    };
    assert_eq!(sender.key, "group\0owner");
    assert!(!sender.owner_chat && !owner);
    // Anyone else, alone or in the owner's group, is only marked seen.
    for inbound in [
        text("m3", "other", None),
        text("m4", "other", Some("group")),
    ] {
        assert_eq!(classify(&inbound, &intake), Verdict::Stranger);
    }
}

#[test]
fn an_owner_only_account_with_no_known_owner_answers_nobody() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let intake = owned(&state, &conversations, None);
    for inbound in [
        text("m1", "owner", None),
        text("m2", "other", Some("group")),
    ] {
        assert_eq!(classify(&inbound, &intake), Verdict::Stranger);
    }
}

#[test]
fn an_account_that_answers_anyone_keeps_owner_authority_for_the_owner_alone() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let granted = tool_owner("owner");
    let intake = Intake {
        owner: Some("owner"),
        tool_owner: Some(&granted),
        ..quiet(&state, &conversations)
    };
    let turn = |inbound: &Inbound| match classify(inbound, &intake) {
        Verdict::Turn { sender, owner, .. } => (sender.owner_chat, owner),
        other => panic!("expected a turn, got {other:?}"),
    };
    assert_eq!(turn(&text("m1", "owner", None)), (true, true));
    assert_eq!(turn(&text("m2", "other", None)), (false, false));
    assert_eq!(turn(&text("m3", "other", Some("group"))), (false, false));
}

#[test]
fn a_dropped_sender_never_gets_the_busy_reply_and_seen_ids_stay_ignored() {
    let conversations = HashMap::new();
    let full = state::BridgeState {
        seen: vec!["old".into()],
        in_flight: (0..MAX_CLAIMS)
            .map(|index| claim(&index.to_string(), &format!("sender-{index}")))
            .collect(),
        ..Default::default()
    };
    let intake = owned(&full, &conversations, Some("owner"));
    assert_eq!(
        classify(&text("new", "other", None), &intake),
        Verdict::Stranger
    );
    assert!(matches!(
        classify(&text("mine", "owner", None), &intake),
        Verdict::Busy(_)
    ));
    // Seen and claimed messages keep their own verdicts.
    assert_eq!(
        classify(&text("old", "other", None), &intake),
        Verdict::Ignore
    );
    assert_eq!(
        classify(&text("0", "other", None), &intake),
        Verdict::Claimed
    );
}

/// A voice message from `sender` with `transcript`, `text` beside it, and
/// `more` files.
fn voice(sender: &str, transcript: Option<&str>, text: &str, more: &[MediaKind]) -> Inbound {
    let file = |kind: MediaKind, transcript: Option<&str>| crate::Media {
        kind,
        name: String::new(),
        size: None,
        mime: None,
        transcript: transcript.map(str::to_owned),
        source: "source".into(),
    };
    let mut message = Message::text("v", sender, text, "context", None);
    message.media.push(file(MediaKind::Audio, transcript));
    message
        .media
        .extend(more.iter().map(|kind| file(*kind, None)));
    Inbound::Text(message)
}

#[test]
fn a_voice_message_nobody_transcribed_gets_the_voice_reply_instead_of_a_turn() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let anyone = quiet(&state, &conversations);
    for unheard in [
        voice("other", None, "", &[]),
        voice("other", Some(" "), " ", &[]),
    ] {
        let Verdict::Unheard(sender) = classify(&unheard, &anyone) else {
            panic!("an untranscribed voice message gets the voice reply");
        };
        assert_eq!(sender.key, "other");
    }
    // A transcript, text, or another file leaves something for a turn.
    for heard in [
        voice("other", Some("call me"), "", &[]),
        voice("other", None, "listen", &[]),
        voice("other", None, "", &[MediaKind::Image]),
    ] {
        assert!(
            matches!(classify(&heard, &anyone), Verdict::Turn { .. }),
            "a voice message with something else runs a turn"
        );
    }
    // Only those the account answers get it.
    let owned = owned(&state, &conversations, Some("owner"));
    let from_owner = voice("owner", None, "", &[]);
    let Verdict::Unheard(sender) = classify(&from_owner, &owned) else {
        panic!("the owner gets the voice reply");
    };
    assert!(sender.owner_chat);
    assert_eq!(
        classify(&voice("other", None, "", &[]), &owned),
        Verdict::Stranger
    );
}

#[test]
fn the_voice_reply_needs_no_turn_slot() {
    let conversations = HashMap::new();
    let full = state::BridgeState {
        in_flight: (0..MAX_CLAIMS)
            .map(|index| claim(&index.to_string(), &format!("sender-{index}")))
            .collect(),
        ..Default::default()
    };
    assert!(matches!(
        classify(
            &voice("other", None, "", &[]),
            &quiet(&full, &conversations)
        ),
        Verdict::Unheard(_)
    ));
}

#[test]
fn explicit_answers_are_recognized_after_normalizing() {
    for yes in [
        "yes",
        "Y",
        "Yes!",
        "yes!!",
        "YES .",
        " y ",
        "是",
        "是的",
        "是的。",
        "确认！",
        "同意",
    ] {
        assert_eq!(answer(yes), Some(true), "{yes:?}");
    }
    for no in [
        "no",
        "N",
        "No.",
        "否",
        "不",
        "不要",
        "取消",
        "算了。",
        "Stop!",
        " stop ",
    ] {
        assert_eq!(answer(no), Some(false), "{no:?}");
    }
    // Casual replies the owner may send about anything else are not answers.
    for other in [
        "",
        "ok",
        "OK",
        "okay.",
        "好",
        "好的",
        "好的。",
        "可以",
        "yes please",
        "no way",
        "maybe",
        "yes?",
        "not yet",
        "好的，发布吧",
        "[sticker]",
        "yes\nbut wait",
        "?",
        "!",
    ] {
        assert_eq!(answer(other), None, "{other:?}");
    }
}

/// When the question in the tests below reached the owner's chat.
const ASKED_MS: u64 = 1_790_000_000_000;

/// A direct or group text message the platform says was sent at `sent_ms`.
fn sent(id: &str, sender: &str, text: &str, group: Option<&str>, sent_ms: Option<u64>) -> Inbound {
    let mut message = Message::text(id, sender, text, "context", group);
    message.sent_ms = sent_ms;
    Inbound::Text(message)
}

#[test]
fn only_the_owner_answers_a_question_in_their_direct_chat() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let asking = Intake {
        owner: Some("owner"),
        question: Some(ASKED_MS),
        ..quiet(&state, &conversations)
    };
    let later = Some(ASKED_MS + 5_000);
    let said = |id: &str, sender: &str, text: &str, group: Option<&str>| {
        sent(id, sender, text, group, later)
    };
    let owner_yes = said("m1", "owner", "Yes!", None);
    let Verdict::Answer { sender, yes } = classify(&owner_yes, &asking) else {
        panic!("the owner's yes answers");
    };
    assert!(yes && sender.owner_chat);
    assert_eq!(sender.key, "owner");
    assert!(matches!(
        classify(&said("m2", "owner", "不要", None), &asking),
        Verdict::Answer { yes: false, .. }
    ));
    // Other senders, the owner's group messages, and other words, casual
    // ones included, run turns.
    for inbound in [
        said("m3", "other", "yes", None),
        said("m4", "owner", "yes", Some("group")),
        said("m5", "owner", "what is the question?", None),
        said("m8", "owner", "ok", None),
        said("m9", "owner", "好的", None),
    ] {
        assert!(
            matches!(classify(&inbound, &asking), Verdict::Turn { .. }),
            "{:?}",
            classify(&inbound, &asking)
        );
    }
    // A file with a yes caption is not a plain answer.
    let Inbound::Text(mut captioned) = said("m6", "owner", "yes", None) else {
        unreachable!()
    };
    captioned.media.push(crate::Media {
        kind: crate::MediaKind::Image,
        name: String::new(),
        size: None,
        mime: None,
        transcript: None,
        source: "img".into(),
    });
    assert!(matches!(
        classify(&Inbound::Text(captioned), &asking),
        Verdict::Turn { .. }
    ));
    // Without a question the owner has seen, a yes is an ordinary message.
    let idle = Intake {
        question: None,
        ..asking
    };
    assert!(matches!(classify(&owner_yes, &idle), Verdict::Turn { .. }));
    // An owner-only account answers its owner's yes and drops a stranger's.
    let owner_only = Intake {
        question: Some(ASKED_MS),
        ..owned(&state, &conversations, Some("owner"))
    };
    assert!(matches!(
        classify(&owner_yes, &owner_only),
        Verdict::Answer { yes: true, .. }
    ));
    assert_eq!(
        classify(&said("m7", "other", "yes", None), &owner_only),
        Verdict::Stranger
    );
}

#[test]
fn only_a_message_sent_after_the_question_reached_the_chat_answers_it() {
    let state = state::BridgeState::default();
    let conversations = HashMap::new();
    let asking = Intake {
        owner: Some("owner"),
        question: Some(ASKED_MS),
        ..quiet(&state, &conversations)
    };
    // Sent before the question arrived, such as a catch-up replay or a
    // message a poll in backoff picks up late: meant for something else.
    // A message whose platform gives no time cannot answer either.
    for sent_ms in [Some(ASKED_MS - 1), Some(ASKED_MS - 60_000), Some(0), None] {
        assert!(
            matches!(
                classify(&sent("m1", "owner", "yes", None, sent_ms), &asking),
                Verdict::Turn { .. }
            ),
            "{sent_ms:?}"
        );
    }
    for sent_ms in [ASKED_MS, ASKED_MS + 1] {
        assert!(
            matches!(
                classify(&sent("m1", "owner", "yes", None, Some(sent_ms)), &asking),
                Verdict::Answer { yes: true, .. }
            ),
            "{sent_ms}"
        );
    }
}

#[test]
fn an_answer_needs_no_room_but_is_never_taken_twice() {
    let full = state::BridgeState {
        in_flight: (0..MAX_CLAIMS)
            .map(|index| claim(&index.to_string(), &format!("sender-{index}")))
            .collect(),
        ..Default::default()
    };
    let conversations = HashMap::new();
    let asking = Intake {
        owner: Some("owner"),
        question: Some(ASKED_MS),
        ..quiet(&full, &conversations)
    };
    let answer = sent("m1", "owner", "yes", None, Some(ASKED_MS + 1));
    assert!(matches!(
        classify(&answer, &asking),
        Verdict::Answer { yes: true, .. }
    ));
    // A repeated delivery of an answer already acknowledged is dropped.
    let acknowledged = state::BridgeState {
        pending: vec![crate::new_pending(
            "m1",
            "owner",
            "context",
            crate::ANSWERED_YES,
            64,
        )],
        ..Default::default()
    };
    assert_eq!(
        classify(
            &answer,
            &Intake {
                state: &acknowledged,
                ..asking
            }
        ),
        Verdict::Claimed
    );
}
