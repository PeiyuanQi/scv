//! The chat log: each direct conversation between a chat account's owner and
//! SCV, kept on disk so a new session can pick an unfinished conversation
//! back up and the model can look further back when asked.
//!
//! A conversation's log is a directory (see [`conversation_path`]) of
//! *episodes*, runs of messages without a long pause. Each episode is one
//! JSONL file, `<year>/<week>/<start>.jsonl`, all in the host's local time:
//! the calendar year of its first message, the Monday-to-Sunday week holding
//! that day as `<Monday>_<Sunday>` (`2026-09-21_2026-09-27`), and the time of
//! its first message (`2026-09-26T14-04-05`). A message starts a new episode
//! when the conversation was quiet for the episode gap, or when the owner
//! ended the last episode (`/new`); otherwise it joins the newest one.
//!
//! Files the owner asked to keep are moved to `<archive>/<conversation>/files/`
//! (see [`kept_dir`]), outside the media directory's retention.
//!
//! Only the chat bridge writes a conversation's log, through one [`Log`] per
//! account run, which holds the account's lock. The server reads it to reload
//! the open episode into a new session ([`open_episode`]) and to answer the
//! `chat_history` tool ([`episodes`], [`read_episode`], [`search`]).

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead as _, BufReader, Read as _, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Longest text one record keeps; the rest is cut with [`CUT_NOTE`].
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
const CUT_NOTE: &str = "\n[cut: longer than the chat log keeps]";
/// Longest line a reader accepts; longer ones are skipped.
const MAX_LINE_BYTES: usize = 2 * MAX_TEXT_BYTES;
/// Where kept files go inside a conversation's archive directory.
const KEPT_DIR: &str = "files";
/// How far a record may lie in the future and still count as recent: a
/// clock set back further than this closes the episode rather than keeping
/// it open until real time catches up.
const MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
/// Week directories inspected to find the newest episode, newest first. A
/// time zone change can put a newer episode in an older-named week, but
/// never by more than a day.
const NEWEST_WEEKS: usize = 3;

/// One line of an episode file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    /// Something said in the chat.
    Message(Entry),
    /// The owner ended the episode (`/new`); the next message starts another.
    End {
        at: u64,
        #[serde(default)]
        local: String,
    },
    /// A record a newer release wrote.
    #[serde(other)]
    Unknown,
}

/// A message in the chat, by whoever wrote it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// When SCV recorded it, in Unix milliseconds.
    pub at: u64,
    /// The same moment on the host's clock, as `2026-09-26 14:04:05 -07:00`.
    #[serde(default)]
    pub local: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// What the owner's message quoted or forwarded, as the model saw it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quote: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileRef>,
    /// Why files did not come in, such as `[image: download failed]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// A finished background job's report, which answers no message.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report: bool,
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The account's owner.
    #[default]
    Owner,
    /// The model's answer, sent as written.
    Scv,
    /// SCV's own words: a notice, a question, a fixed reply.
    System,
    /// A role a newer release wrote.
    #[serde(other)]
    Unknown,
}

/// A file that came with a message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    /// `image`, `audio`, `video`, or `file`.
    pub kind: String,
    pub name: String,
    /// Where a received file was saved; empty for a file SCV sent, whose copy
    /// is removed once it is delivered.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    /// What a voice message said, when the platform transcribed it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transcript: String,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A moment on the host's wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    /// Seconds east of UTC.
    offset: i32,
}

impl LocalTime {
    /// `unix` seconds on a clock `offset` seconds east of UTC.
    pub fn at(unix: i64, offset: i32) -> Self {
        let local = unix.saturating_add(i64::from(offset));
        let days = local.div_euclid(86_400);
        let seconds = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: (seconds / 3600) as u32,
            minute: (seconds / 60 % 60) as u32,
            second: (seconds % 60) as u32,
            offset,
        }
    }

    pub fn year(&self) -> i64 {
        self.year
    }

    /// `2026-09-26 14:04:05 -07:00`.
    pub fn stamp(&self) -> String {
        let sign = if self.offset < 0 { '-' } else { '+' };
        let offset = self.offset.unsigned_abs();
        format!(
            "{} {:02}:{:02}:{:02} {sign}{:02}:{:02}",
            self.date(),
            self.hour,
            self.minute,
            self.second,
            offset / 3600,
            offset / 60 % 60
        )
    }

    /// `2026-09-26`.
    pub fn date(&self) -> String {
        format_date(self.year, self.month, self.day)
    }

    /// The Monday-to-Sunday week holding this day, as `<Monday>_<Sunday>`.
    fn week(&self) -> String {
        let days = days_from_civil(self.year, self.month, self.day);
        // 1970-01-01 was a Thursday; count days since a Monday.
        let monday = days - (days + 3).rem_euclid(7);
        let (y1, m1, d1) = civil_from_days(monday);
        let (y2, m2, d2) = civil_from_days(monday + 6);
        format!("{}_{}", format_date(y1, m1, d1), format_date(y2, m2, d2))
    }

    /// `2026-09-26T14-04-05`, an episode file's name without its extension.
    fn file_stem(&self) -> String {
        format!(
            "{}T{:02}-{:02}-{:02}",
            self.date(),
            self.hour,
            self.minute,
            self.second
        )
    }
}

fn format_date(year: i64, month: u32, day: u32) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Whether `part` may name a directory under the history root: a channel,
/// an account, or a conversation digest.
pub fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 64
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// A conversation's directory relative to the history root (and to the
/// archive): `<channel>/<account>/<conversation>`; `None` when a part is not
/// a plain name.
pub fn conversation_path(channel: &str, account: &str, conversation: &str) -> Option<PathBuf> {
    [channel, account, conversation]
        .iter()
        .all(|part| valid_part(part))
        .then(|| Path::new(channel).join(account).join(conversation))
}

/// Where the files the owner kept from a conversation go:
/// `<archive>/<conversation>/files`, `conversation` being the path
/// [`conversation_path`] returns.
pub fn kept_dir(archive: &Path, conversation: &Path) -> PathBuf {
    archive.join(conversation).join(KEPT_DIR)
}

/// Appends to one conversation's log.
pub struct Log {
    dir: PathBuf,
    gap: Duration,
    /// The newest episode, once `scanned` found it.
    newest: Option<Newest>,
    scanned: bool,
}

#[derive(Debug, Clone)]
struct Newest {
    path: PathBuf,
    last_at: u64,
    ended: bool,
}

impl Log {
    /// The log in `dir`, whose episodes end after `gap` without a message.
    pub fn new(dir: PathBuf, gap: Duration) -> Self {
        Self {
            dir,
            gap,
            newest: None,
            scanned: false,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record `entry`, timed `local`, in the open episode, or in a new one
    /// when there is none: the first message, one after the gap, or one
    /// after the owner ended the last episode.
    pub fn append(&mut self, mut entry: Entry, local: &LocalTime) -> io::Result<()> {
        entry.local = local.stamp();
        cut(&mut entry.text);
        cut(&mut entry.quote);
        let at = entry.at;
        let path = match self.newest(at)? {
            Some(newest) if is_open(&newest, self.gap, at) => newest.path,
            _ => self.start(local)?,
        };
        write_record(&path, &Record::Message(entry))?;
        self.newest = Some(Newest {
            path,
            last_at: at,
            ended: false,
        });
        Ok(())
    }

    /// End the open episode, so the next message starts a new one. Returns
    /// whether an episode was open.
    pub fn end(&mut self, at: u64, local: &LocalTime) -> io::Result<bool> {
        let Some(newest) = self.newest(at)? else {
            return Ok(false);
        };
        if !is_open(&newest, self.gap, at) {
            return Ok(false);
        }
        write_record(
            &newest.path,
            &Record::End {
                at,
                local: local.stamp(),
            },
        )?;
        self.newest = Some(Newest {
            ended: true,
            ..newest
        });
        Ok(true)
    }

    fn newest(&mut self, now: u64) -> io::Result<Option<Newest>> {
        if self.scanned {
            return Ok(self.newest.clone());
        }
        let newest = match newest_episode(&self.dir, now)? {
            Some(path) => {
                let (last_at, ended) = tail(&path)?;
                Some(Newest {
                    path,
                    last_at,
                    ended,
                })
            }
            None => None,
        };
        self.newest.clone_from(&newest);
        self.scanned = true;
        Ok(newest)
    }

    /// Create a new episode file for a first message at `local`.
    fn start(&self, local: &LocalTime) -> io::Result<PathBuf> {
        let dir = self
            .dir
            .join(format!("{:04}", local.year))
            .join(local.week());
        create_private_dir(&dir)?;
        let stem = local.file_stem();
        for attempt in 1..1000 {
            let name = if attempt == 1 {
                format!("{stem}.jsonl")
            } else {
                format!("{stem}-{attempt}.jsonl")
            };
            let path = dir.join(name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            match options.open(&path) {
                Ok(_) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other(
            "too many episodes start in the same second",
        ))
    }
}

fn is_open(newest: &Newest, gap: Duration, now: u64) -> bool {
    !newest.ended
        && newest.last_at <= now.saturating_add(MAX_CLOCK_SKEW_MS)
        && u128::from(now.saturating_sub(newest.last_at)) < gap.as_millis()
}

fn cut(text: &mut String) {
    if text.len() > MAX_TEXT_BYTES {
        let kept = crate::text::utf8_prefix(text, MAX_TEXT_BYTES - CUT_NOTE.len()).len();
        text.truncate(kept);
        text.push_str(CUT_NOTE);
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

fn write_record(path: &Path, record: &Record) -> io::Result<()> {
    let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // One write per record, so a reader never sees half a line in between.
    options.open(path)?.write_all(&line)
}

/// Every record of an episode file, skipping lines that do not parse.
fn records(path: &Path) -> io::Result<Vec<Record>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut records = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(records);
        }
        if line.last() != Some(&b'\n') && read > MAX_LINE_BYTES {
            // Skip the rest of an overlong line.
            let mut rest = Vec::new();
            reader.read_until(b'\n', &mut rest)?;
            continue;
        }
        if let Ok(record) = serde_json::from_slice(&line) {
            records.push(record);
        }
    }
}

/// When the episode's last message was recorded, and whether it was ended.
fn tail(path: &Path) -> io::Result<(u64, bool)> {
    let mut last_at = 0;
    let mut ended = false;
    for record in records(path)? {
        match record {
            Record::Message(entry) => {
                last_at = last_at.max(entry.at);
                ended = false;
            }
            Record::End { .. } => ended = true,
            Record::Unknown => {}
        }
    }
    Ok((last_at, ended))
}

/// When an episode's first record was written: the order episodes started
/// in, whatever their local-time names say.
fn first_at(path: &Path) -> io::Result<u64> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    (&mut reader)
        .take(MAX_LINE_BYTES as u64)
        .read_until(b'\n', &mut line)?;
    Ok(match serde_json::from_slice(&line) {
        Ok(Record::Message(entry)) => entry.at,
        Ok(Record::End { at, .. }) => at,
        _ => 0,
    })
}

/// Names of the entries of `dir` that `keep` accepts, sorted; none when
/// `dir` does not exist.
fn names(dir: &Path, keep: impl Fn(&str) -> bool) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && keep(name)
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn is_year(name: &str) -> bool {
    name.len() == 4 && name.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_week(name: &str) -> bool {
    name.len() == 21 && name.as_bytes()[10] == b'_' && valid_part(name)
}

fn is_episode_file(name: &str) -> bool {
    name.strip_suffix(".jsonl").is_some_and(valid_part)
}

/// Every episode of the conversation at `dir` as `(id, path)`, the newest
/// name first. The ID is `<year>/<week>/<start>`.
fn all_episodes(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut episodes = Vec::new();
    for year in names(dir, is_year)? {
        for week in names(&dir.join(&year), is_week)? {
            let week_dir = dir.join(&year).join(&week);
            for file in names(&week_dir, is_episode_file)? {
                let stem = file.trim_end_matches(".jsonl");
                episodes.push((format!("{year}/{week}/{stem}"), week_dir.join(&file)));
            }
        }
    }
    episodes.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(episodes)
}

/// The episode file that started last, among the newest weeks, by the time
/// of its first record; episodes dated after `now` (a clock set back since)
/// count only when there is nothing else.
fn newest_episode(dir: &Path, now: u64) -> io::Result<Option<PathBuf>> {
    let mut weeks = Vec::new();
    for year in names(dir, is_year)?.into_iter().rev().take(2) {
        for week in names(&dir.join(&year), is_week)? {
            weeks.push((week, year.clone()));
        }
    }
    weeks.sort();
    // (dated in the past, first record's time): the largest wins.
    let mut newest: Option<((bool, u64), PathBuf)> = None;
    for (week, year) in weeks.into_iter().rev().take(NEWEST_WEEKS) {
        let week_dir = dir.join(year).join(week);
        for file in names(&week_dir, is_episode_file)? {
            let path = week_dir.join(file);
            let started = first_at(&path)?;
            let rank = (started <= now.saturating_add(MAX_CLOCK_SKEW_MS), started);
            if newest.as_ref().is_none_or(|(best, _)| rank >= *best) {
                newest = Some((rank, path));
            }
        }
    }
    Ok(newest.map(|(_, path)| path))
}

/// An episode's ID when it is well formed: `<year>/<week>/<start>`.
fn episode_path(dir: &Path, id: &str) -> Option<PathBuf> {
    let mut parts = id.split('/');
    let (Some(year), Some(week), Some(stem), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    (is_year(year) && is_week(week) && valid_part(stem))
        .then(|| dir.join(year).join(week).join(format!("{stem}.jsonl")))
}

/// An episode and its messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub id: String,
    pub messages: Vec<Entry>,
}

/// The conversation's open episode at `now` (Unix milliseconds): the newest
/// one, unless it was ended or its last message is `gap` or more ago.
pub fn open_episode(dir: &Path, gap: Duration, now: u64) -> io::Result<Option<Episode>> {
    let Some(path) = newest_episode(dir, now)? else {
        return Ok(None);
    };
    let mut messages = Vec::new();
    let mut ended = false;
    for record in records(&path)? {
        match record {
            Record::Message(entry) => {
                ended = false;
                messages.push(entry);
            }
            Record::End { .. } => ended = true,
            Record::Unknown => {}
        }
    }
    let last_at = messages.iter().map(|entry| entry.at).max().unwrap_or(0);
    let newest = Newest {
        path: path.clone(),
        last_at,
        ended,
    };
    if messages.is_empty() || !is_open(&newest, gap, now) {
        return Ok(None);
    }
    Ok(Some(Episode {
        id: episode_id(dir, &path),
        messages,
    }))
}

fn episode_id(dir: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(dir).unwrap_or(path);
    relative
        .with_extension("")
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// One episode as [`episodes`] lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub id: String,
    /// Local time of its first and last message.
    pub started: String,
    pub last: String,
    pub messages: usize,
    /// The start of the owner's first message.
    pub opening: String,
    /// Whether the owner ended it with `/new`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ended: bool,
}

/// Longest opening line or search excerpt, in characters.
const EXCERPT_CHARS: usize = 240;

/// Up to `limit` episodes, newest first, skipping those that started on or
/// after `before` or before `after` (dates as `YYYY-MM-DD`), with whether
/// more are left.
pub fn episodes(
    dir: &Path,
    before: Option<&str>,
    after: Option<&str>,
    limit: usize,
) -> io::Result<(Vec<Summary>, bool)> {
    let mut summaries = Vec::new();
    let mut more = false;
    for (id, path) in all_episodes(dir)? {
        let day = id.rsplit('/').next().unwrap_or("").get(..10).unwrap_or("");
        if before.is_some_and(|before| day >= before) || after.is_some_and(|after| day < after) {
            continue;
        }
        if summaries.len() == limit {
            more = true;
            break;
        }
        let mut summary = Summary {
            id,
            started: String::new(),
            last: String::new(),
            messages: 0,
            opening: String::new(),
            ended: false,
        };
        for record in records(&path)? {
            match record {
                Record::Message(entry) => {
                    if summary.started.is_empty() {
                        summary.started.clone_from(&entry.local);
                    }
                    if summary.opening.is_empty() && entry.role == Role::Owner {
                        summary.opening = excerpt(&entry.text, 0);
                    }
                    summary.last = entry.local;
                    summary.messages += 1;
                    summary.ended = false;
                }
                Record::End { .. } => summary.ended = true,
                Record::Unknown => {}
            }
        }
        summaries.push(summary);
    }
    Ok((summaries, more))
}

/// The messages of episode `id` from `offset`, at most `limit`, and how many
/// it has; `None` when there is no such episode.
pub fn read_episode(
    dir: &Path,
    id: &str,
    offset: usize,
    limit: usize,
) -> io::Result<Option<(Vec<Entry>, usize)>> {
    let Some(path) = episode_path(dir, id) else {
        return Ok(None);
    };
    let messages: Vec<Entry> = match records(&path) {
        Ok(records) => records
            .into_iter()
            .filter_map(|record| match record {
                Record::Message(entry) => Some(entry),
                _ => None,
            })
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let total = messages.len();
    Ok(Some((
        messages.into_iter().skip(offset).take(limit).collect(),
        total,
    )))
}

/// A message [`search`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub episode: String,
    /// Its position in the episode, for [`read_episode`].
    pub index: usize,
    pub local: String,
    pub role: Role,
    pub excerpt: String,
}

/// Messages that contain every word of `query` (ignoring case) in their
/// text, quote, or file names, newest first: at most `limit`, reading at
/// most `max_bytes` of the log. Also returns whether the search stopped
/// early.
pub fn search(
    dir: &Path,
    query: &str,
    limit: usize,
    max_bytes: u64,
) -> io::Result<(Vec<Hit>, bool)> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut hits = Vec::new();
    if terms.is_empty() {
        return Ok((hits, false));
    }
    let mut read = 0u64;
    for (id, path) in all_episodes(dir)? {
        if read >= max_bytes {
            return Ok((hits, true));
        }
        read = read.saturating_add(fs::metadata(&path).map_or(0, |metadata| metadata.len()));
        let messages: Vec<Entry> = records(&path)?
            .into_iter()
            .filter_map(|record| match record {
                Record::Message(entry) => Some(entry),
                _ => None,
            })
            .collect();
        for (index, entry) in messages.iter().enumerate().rev() {
            let mut haystack = entry.text.to_lowercase();
            let files = entry
                .files
                .iter()
                .flat_map(|file| [&file.name, &file.transcript]);
            for extra in std::iter::once(&entry.quote).chain(files) {
                haystack.push('\n');
                haystack.push_str(&extra.to_lowercase());
            }
            if !terms.iter().all(|term| haystack.contains(term.as_str())) {
                continue;
            }
            let lower = entry.text.to_lowercase();
            let from = lower
                .find(terms[0].as_str())
                .map_or(0, |byte| lower[..byte].chars().count());
            hits.push(Hit {
                episode: id.clone(),
                index,
                local: entry.local.clone(),
                role: entry.role,
                excerpt: excerpt(&entry.text, from),
            });
            if hits.len() == limit {
                return Ok((hits, true));
            }
        }
    }
    Ok((hits, false))
}

/// About [`EXCERPT_CHARS`] of `text` around character `from`.
fn excerpt(text: &str, from: usize) -> String {
    let start = from.saturating_sub(EXCERPT_CHARS / 4);
    let mut excerpt: String = text.chars().skip(start).take(EXCERPT_CHARS).collect();
    if start > 0 {
        excerpt.insert(0, '…');
    }
    if text.chars().count() > start + EXCERPT_CHARS {
        excerpt.push('…');
    }
    excerpt
}

/// Remove year directories older than `oldest_year` from every
/// conversation under `account_dir`; returns how many went.
pub fn prune_years(account_dir: &Path, oldest_year: i64) -> usize {
    let mut removed = 0;
    let Ok(conversations) = names(account_dir, valid_part) else {
        return 0;
    };
    for conversation in conversations {
        let dir = account_dir.join(conversation);
        let Ok(years) = names(&dir, is_year) else {
            continue;
        };
        for year in years {
            if year.parse::<i64>().is_ok_and(|year| year < oldest_year)
                && fs::remove_dir_all(dir.join(&year)).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests;
