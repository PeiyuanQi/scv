//! Unit tests for `src/history.rs`.

use super::*;

const GAP: Duration = Duration::from_secs(2 * 60 * 60);
/// 2026-09-26 21:04:05 UTC, a Saturday.
const SATURDAY: i64 = 1_790_456_645;
const PDT: i32 = -7 * 3600;

fn owner(at_seconds: i64, text: &str) -> Entry {
    Entry {
        at: at_seconds as u64 * 1000,
        role: Role::Owner,
        text: text.into(),
        ..Entry::default()
    }
}

fn append(log: &mut Log, at_seconds: i64, role: Role, text: &str) {
    let mut entry = owner(at_seconds, text);
    entry.role = role;
    log.append(entry, &LocalTime::at(at_seconds, PDT)).unwrap();
}

#[test]
fn local_time_names_the_day_week_and_offset() {
    let local = LocalTime::at(SATURDAY, PDT);
    assert_eq!(local.stamp(), "2026-09-26 14:04:05 -07:00");
    assert_eq!(local.week(), "2026-09-21_2026-09-27");
    assert_eq!(local.file_stem(), "2026-09-26T14-04-05");
    // The same moment in UTC is already Saturday evening; east of UTC, Sunday.
    assert_eq!(
        LocalTime::at(SATURDAY, 0).stamp(),
        "2026-09-26 21:04:05 +00:00"
    );
    let tokyo = LocalTime::at(SATURDAY, 9 * 3600);
    assert_eq!(tokyo.stamp(), "2026-09-27 06:04:05 +09:00");
    assert_eq!(tokyo.week(), "2026-09-21_2026-09-27");
    // Monday starts the next week; a week can span two years.
    assert_eq!(
        LocalTime::at(SATURDAY + 2 * 86_400, PDT).week(),
        "2026-09-28_2026-10-04"
    );
    let new_year = LocalTime::at(1_798_761_600, 0); // 2027-01-01 00:00 UTC, a Friday
    assert_eq!(new_year.date(), "2027-01-01");
    assert_eq!(new_year.week(), "2026-12-28_2027-01-03");
    assert_eq!(
        LocalTime::at(0, 5 * 3600 + 1800).stamp(),
        "1970-01-01 05:30:00 +05:30"
    );
    assert_eq!(LocalTime::at(-1, 0).stamp(), "1969-12-31 23:59:59 +00:00");
}

#[test]
fn civil_day_arithmetic_round_trips() {
    for days in -800_000..800_000 {
        let (year, month, day) = civil_from_days(days);
        assert_eq!(days_from_civil(year, month, day), days);
    }
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(11_016), (2000, 2, 29));
}

#[test]
fn messages_join_the_open_episode_until_the_gap() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("wechat/default/abc");
    let mut log = Log::new(dir.clone(), GAP);
    append(&mut log, SATURDAY, Role::Owner, "first");
    append(&mut log, SATURDAY + 60, Role::Scv, "answer");
    append(&mut log, SATURDAY + 60 + 7199, Role::Owner, "still here");
    let (episodes, more) = episodes(&dir, None, None, 10).unwrap();
    assert!(!more);
    assert_eq!(episodes.len(), 1);
    assert_eq!(
        episodes[0].id,
        "2026/2026-09-21_2026-09-27/2026-09-26T14-04-05"
    );
    assert_eq!(episodes[0].messages, 3);
    assert_eq!(episodes[0].opening, "first");
    // Two hours of quiet start a new episode, in the next week's directory.
    append(&mut log, SATURDAY + 3 * 86_400, Role::Owner, "later");
    let (episodes, _) = super::episodes(&dir, None, None, 10).unwrap();
    assert_eq!(
        episodes.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        [
            "2026/2026-09-28_2026-10-04/2026-09-29T14-04-05",
            "2026/2026-09-21_2026-09-27/2026-09-26T14-04-05",
        ]
    );
    assert!(
        dir.join("2026/2026-09-28_2026-10-04/2026-09-29T14-04-05.jsonl")
            .is_file()
    );
}

#[cfg(unix)]
#[test]
fn log_files_are_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    let mut log = Log::new(dir.clone(), GAP);
    append(&mut log, SATURDAY, Role::Owner, "hi");
    let file = dir.join("2026/2026-09-21_2026-09-27/2026-09-26T14-04-05.jsonl");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("2026")), 0o700);
}

#[test]
fn a_new_writer_finds_the_open_episode_and_end_closes_it() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    append(
        &mut Log::new(dir.clone(), GAP),
        SATURDAY,
        Role::Owner,
        "before restart",
    );
    // A restarted bridge keeps writing to the same episode.
    let mut log = Log::new(dir.clone(), GAP);
    append(&mut log, SATURDAY + 600, Role::Owner, "after restart");
    let open = open_episode(&dir, GAP, (SATURDAY as u64 + 700) * 1000)
        .unwrap()
        .unwrap();
    assert_eq!(
        open.messages
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>(),
        ["before restart", "after restart"]
    );
    assert_eq!(open.messages[1].local, "2026-09-26 14:14:05 -07:00");
    // Nothing is open once the gap has passed.
    assert!(
        open_episode(&dir, GAP, (SATURDAY as u64 + 600 + 7200) * 1000)
            .unwrap()
            .is_none()
    );
    // `/new` ends it at once, and the next message starts another.
    let now = SATURDAY + 800;
    assert!(
        log.end(now as u64 * 1000, &LocalTime::at(now, PDT))
            .unwrap()
    );
    assert!(
        !log.end(now as u64 * 1000, &LocalTime::at(now, PDT))
            .unwrap()
    );
    assert!(
        open_episode(&dir, GAP, now as u64 * 1000)
            .unwrap()
            .is_none()
    );
    append(&mut log, now + 5, Role::Owner, "fresh start");
    let open = open_episode(&dir, GAP, (now as u64 + 6) * 1000)
        .unwrap()
        .unwrap();
    assert_eq!(open.id, "2026/2026-09-21_2026-09-27/2026-09-26T14-17-30");
    assert_eq!(open.messages.len(), 1);
    let (episodes, _) = episodes(&dir, None, None, 10).unwrap();
    assert!(episodes[1].ended);
    // A restarted writer sees the ended episode as ended too.
    let mut restarted = Log::new(dir.clone(), GAP);
    assert!(
        restarted
            .end((now as u64 + 10) * 1000, &LocalTime::at(now + 10, PDT))
            .unwrap()
    );
}

#[test]
fn the_newest_episode_is_the_one_that_started_last() {
    // After the clock moved to a zone west of the old one, a newer episode
    // can have an earlier local name.
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    let mut log = Log::new(dir.clone(), GAP);
    log.append(owner(SATURDAY, "in UTC"), &LocalTime::at(SATURDAY, 0))
        .unwrap();
    let mut log = Log::new(dir.clone(), GAP);
    let later = SATURDAY + 3 * 3600;
    log.append(owner(later, "in PDT"), &LocalTime::at(later, PDT))
        .unwrap();
    let open = open_episode(&dir, GAP, later as u64 * 1000)
        .unwrap()
        .unwrap();
    assert_eq!(open.messages[0].text, "in PDT");
    assert_eq!(open.id, "2026/2026-09-21_2026-09-27/2026-09-26T17-04-05");
}

#[test]
fn episodes_same_second_get_distinct_files_and_dates_filter_them() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    let mut log = Log::new(dir.clone(), GAP);
    append(&mut log, SATURDAY, Role::Owner, "one");
    log.end(SATURDAY as u64 * 1000, &LocalTime::at(SATURDAY, PDT))
        .unwrap();
    append(&mut log, SATURDAY, Role::Owner, "two");
    append(&mut log, SATURDAY + 86_400 * 10, Role::Owner, "three");
    let ids = |before, after| {
        episodes(&dir, before, after, 10)
            .unwrap()
            .0
            .into_iter()
            .map(|e| e.opening)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(None, None), ["three", "two", "one"]);
    assert_eq!(ids(Some("2026-10-06"), None), ["two", "one"]);
    assert_eq!(ids(None, Some("2026-09-27")), ["three"]);
    let (page, more) = episodes(&dir, None, None, 2).unwrap();
    assert_eq!(page.len(), 2);
    assert!(more);
}

#[test]
fn reading_an_episode_pages_through_it_and_refuses_odd_ids() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    let mut log = Log::new(dir.clone(), GAP);
    for index in 0..5 {
        append(
            &mut log,
            SATURDAY + index,
            Role::Owner,
            &format!("m{index}"),
        );
    }
    let id = "2026/2026-09-21_2026-09-27/2026-09-26T14-04-05";
    let (page, total) = read_episode(&dir, id, 3, 10).unwrap().unwrap();
    assert_eq!(total, 5);
    assert_eq!(
        page.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
        ["m3", "m4"]
    );
    for bad in [
        "../x",
        "2026/2026-09-21_2026-09-27/../../x",
        "2026/..",
        "/etc/passwd",
        "2026/2026-09-21_2026-09-27/a/b",
        "",
    ] {
        assert!(read_episode(&dir, bad, 0, 10).unwrap().is_none(), "{bad}");
    }
    assert!(
        read_episode(
            &dir,
            "2026/2026-09-21_2026-09-27/2026-09-26T00-00-00",
            0,
            10
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn search_finds_every_word_newest_first() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    let mut log = Log::new(dir.clone(), GAP);
    append(
        &mut log,
        SATURDAY,
        Role::Owner,
        "Please publish the Crates release",
    );
    append(
        &mut log,
        SATURDAY + 10,
        Role::Scv,
        "Published 0.3.1 to crates.io",
    );
    let mut with_file = owner(SATURDAY + 20, "here is the invoice");
    with_file.files.push(FileRef {
        kind: "file".into(),
        name: "Receipt-2026.pdf".into(),
        ..FileRef::default()
    });
    log.append(with_file, &LocalTime::at(SATURDAY + 20, PDT))
        .unwrap();
    let (hits, stopped) = search(&dir, "CRATES publish", 10, u64::MAX).unwrap();
    assert!(!stopped);
    assert_eq!(
        hits.iter().map(|hit| hit.index).collect::<Vec<_>>(),
        [1, 0],
        "{hits:?}"
    );
    assert_eq!(hits[0].role, Role::Scv);
    assert_eq!(hits[1].excerpt, "Please publish the Crates release");
    assert_eq!(search(&dir, "receipt", 10, u64::MAX).unwrap().0[0].index, 2);
    assert!(search(&dir, "   ", 10, u64::MAX).unwrap().0.is_empty());
    let (hits, stopped) = search(&dir, "crates", 1, u64::MAX).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(stopped);
    // Reading stops at the byte budget, between episodes.
    let (hits, stopped) = search(&dir, "nothing-matches", 10, 0).unwrap();
    assert!(hits.is_empty() && stopped);
}

#[test]
fn long_text_is_cut_and_unreadable_lines_skipped() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    let mut log = Log::new(dir.clone(), GAP);
    append(&mut log, SATURDAY, Role::Owner, &"é".repeat(MAX_TEXT_BYTES));
    let file = dir.join("2026/2026-09-21_2026-09-27/2026-09-26T14-04-05.jsonl");
    let mut raw = OpenOptions::new().append(true).open(&file).unwrap();
    raw.write_all(b"not json\n{\"type\":\"reaction\",\"at\":1}\n")
        .unwrap();
    raw.write_all(&vec![b'x'; MAX_LINE_BYTES + 10]).unwrap();
    raw.write_all(b"\n").unwrap();
    drop(raw);
    append(&mut log, SATURDAY + 1, Role::Scv, "after");
    let (messages, total) = read_episode(
        &dir,
        "2026/2026-09-21_2026-09-27/2026-09-26T14-04-05",
        0,
        10,
    )
    .unwrap()
    .unwrap();
    assert_eq!(total, 2);
    assert!(messages[0].text.len() <= MAX_TEXT_BYTES);
    assert!(messages[0].text.ends_with(CUT_NOTE));
    assert_eq!(messages[1].text, "after");
}

#[test]
fn records_of_newer_releases_still_parse() {
    let record: Record = serde_json::from_str(r#"{"type":"reaction","at":5}"#).unwrap();
    assert_eq!(record, Record::Unknown);
    let record: Record =
        serde_json::from_str(r#"{"type":"message","at":5,"role":"bot","text":"x","mood":"ok"}"#)
            .unwrap();
    let Record::Message(entry) = record else {
        panic!("not a message")
    };
    assert_eq!(entry.role, Role::Unknown);
}

#[test]
fn conversation_paths_are_plain_names() {
    assert_eq!(
        conversation_path("wechat", "default", "0a1b").unwrap(),
        Path::new("wechat/default/0a1b")
    );
    for bad in [
        ("..", "a", "b"),
        ("wechat", "", "b"),
        ("wechat", "a/b", "c"),
        ("w", "a", "."),
    ] {
        assert!(conversation_path(bad.0, bad.1, bad.2).is_none(), "{bad:?}");
    }
    assert!(!valid_part(&"a".repeat(65)));
    assert_eq!(
        kept_dir(Path::new("/archive"), Path::new("wechat/default/0a1b")),
        Path::new("/archive/wechat/default/0a1b/files")
    );
}

#[test]
fn pruning_removes_only_old_years() {
    let home = tempfile::tempdir().unwrap();
    let account = home.path().join("wechat/default");
    for year in ["1900", "1905", "2026"] {
        fs::create_dir_all(account.join("abc").join(year)).unwrap();
    }
    fs::create_dir_all(account.join("abc/files")).unwrap();
    assert_eq!(prune_years(&account, 1906), 2);
    assert_eq!(
        names(&account.join("abc"), |_| true).unwrap(),
        ["2026", "files"]
    );
    assert_eq!(prune_years(&home.path().join("missing"), 3000), 0);
}

#[test]
fn records_from_a_clock_set_back_close_their_episode() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    // Written while the clock ran an hour fast.
    let mut log = Log::new(dir.clone(), GAP);
    append(&mut log, SATURDAY + 3600, Role::Owner, "from the future");
    // Once corrected, that episode is no longer open, and a new message
    // starts another, which is then the newest.
    assert!(
        open_episode(&dir, GAP, SATURDAY as u64 * 1000)
            .unwrap()
            .is_none()
    );
    let mut log = Log::new(dir.clone(), GAP);
    append(&mut log, SATURDAY, Role::Owner, "now");
    let open = open_episode(&dir, GAP, (SATURDAY as u64 + 10) * 1000)
        .unwrap()
        .unwrap();
    assert_eq!(open.messages[0].text, "now");
    // A few seconds of skew is still the same episode.
    append(&mut log, SATURDAY + 20, Role::Owner, "same");
    assert_eq!(
        open_episode(&dir, GAP, (SATURDAY as u64 + 5) * 1000)
            .unwrap()
            .unwrap()
            .messages
            .len(),
        2
    );
}

#[test]
fn transcripts_are_searchable() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("c");
    let mut log = Log::new(dir.clone(), GAP);
    let mut voice = owner(SATURDAY, "");
    voice.files.push(FileRef {
        kind: "audio".into(),
        name: "voice.silk".into(),
        transcript: "Call the plumber tomorrow".into(),
        ..FileRef::default()
    });
    log.append(voice, &LocalTime::at(SATURDAY, PDT)).unwrap();
    assert_eq!(search(&dir, "plumber", 5, u64::MAX).unwrap().0.len(), 1);
}
