//! Files that travel with chat messages: what users send, saved privately
//! for their turn, and copies of files the model sends back, all under one
//! instance directory with a retention limit. Nothing saved here is ever
//! executed.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Largest file the model may send with one reply.
pub const MAX_REPLY_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// Files one reply may carry; the rest are dropped with a note.
pub const MAX_REPLY_FILES: usize = 8;
/// Files one received message may bring into a turn.
pub const MAX_MESSAGE_MEDIA: usize = 16;
/// Longest saved file name, in bytes.
const MAX_NAME_BYTES: usize = 96;

/// What kind of file a message carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    File,
}

impl MediaKind {
    /// The name `turn.start` attachments use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::File => "file",
        }
    }

    /// How the file is described to the model and the sender.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "voice message",
            Self::Video => "video",
            Self::File => "file",
        }
    }

    /// The kind a file of `mime` is sent as: images as pictures, video as
    /// video, anything else as a file.
    pub fn for_mime(mime: &str) -> Self {
        if SENDABLE_IMAGES.contains(&mime) {
            Self::Image
        } else if mime.starts_with("video/") {
            Self::Video
        } else {
            Self::File
        }
    }
}

/// Image types every channel shows as a picture.
const SENDABLE_IMAGES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
];

/// Media limits of one channel account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaSettings {
    /// Largest file downloaded from the account owner, in MiB. 0 turns
    /// media downloads off for every sender.
    pub owner_max_mib: u64,
    /// Largest image downloaded from any other sender, in MiB. Their other
    /// files are never downloaded; 0 turns their images off too.
    pub others_image_max_mib: u64,
    /// Days received files, and copies of files sent, are kept.
    pub keep_days: u64,
}

impl Default for MediaSettings {
    fn default() -> Self {
        Self {
            owner_max_mib: 50,
            others_image_max_mib: 5,
            keep_days: 7,
        }
    }
}

impl MediaSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The largest `kind` file downloaded from a sender, or `None` when it is
    /// not downloaded at all.
    pub fn limit(&self, kind: MediaKind, owner: bool) -> Option<u64> {
        let mib = if owner {
            self.owner_max_mib
        } else if kind == MediaKind::Image && self.owner_max_mib > 0 {
            self.others_image_max_mib
        } else {
            0
        };
        (mib > 0).then(|| mib.saturating_mul(1024 * 1024))
    }

    pub fn keep(&self) -> Duration {
        Duration::from_secs(self.keep_days.max(1).saturating_mul(24 * 60 * 60))
    }
}

/// Where one channel account keeps media.
#[derive(Debug, Clone)]
pub struct MediaOptions {
    /// Files this account's senders sent, by conversation.
    pub inbox: PathBuf,
    /// Copies of files the model attached, shared by every channel. The
    /// bridge sends only files inside it.
    pub outbox: PathBuf,
    pub settings: MediaSettings,
}

impl MediaOptions {
    /// `<root>/<channel>/<account>` for received files and `<root>/outbox`
    /// for files to send.
    pub fn new(root: &Path, channel: &str, account: &str, settings: MediaSettings) -> Self {
        Self {
            inbox: root.join(channel).join(account),
            outbox: outbox(root),
            settings,
        }
    }
}

/// Where copies of files the model attaches wait to be sent.
pub fn outbox(root: &Path) -> PathBuf {
    root.join("outbox")
}

/// The MIME type of a file named `name` that starts with `head`, preferring
/// a specific `declared` type.
pub fn mime_type(declared: Option<&str>, name: &str, head: &[u8]) -> String {
    if let Some(declared) = declared
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|value| {
            value.contains('/') && value != "application/octet-stream" && !value.ends_with("/*")
        })
    {
        return declared;
    }
    if let Some(sniffed) = sniff(head) {
        return sniffed.into();
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    EXTENSIONS
        .iter()
        .find(|(ext, _)| *ext == extension)
        .map_or("application/octet-stream", |(_, mime)| mime)
        .into()
}

/// A file extension for `mime`, for files saved without a name.
pub fn extension(mime: &str) -> &'static str {
    EXTENSIONS
        .iter()
        .find(|(_, candidate)| *candidate == mime)
        .map_or("bin", |(ext, _)| ext)
}

const EXTENSIONS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("heic", "image/heic"),
    ("svg", "image/svg+xml"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/opus"),
    ("amr", "audio/amr"),
    ("silk", "audio/silk"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("json", "application/json"),
    ("csv", "text/csv"),
    ("txt", "text/plain"),
    ("log", "text/plain"),
    ("md", "text/markdown"),
    ("html", "text/html"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
];

/// The type magic bytes identify.
fn sniff(head: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    Some(if at(0, b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if at(0, b"\xff\xd8\xff") {
        "image/jpeg"
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        "image/gif"
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        "image/webp"
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        "audio/wav"
    } else if at(0, b"BM") && head.len() > 14 {
        "image/bmp"
    } else if at(4, b"ftypheic") || at(4, b"ftypheix") || at(4, b"ftypmif1") {
        "image/heic"
    } else if at(4, b"ftypM4A") {
        "audio/mp4"
    } else if at(4, b"ftyp") {
        "video/mp4"
    } else if at(0, b"%PDF-") {
        "application/pdf"
    } else if at(0, b"OggS") {
        "audio/ogg"
    } else if at(0, b"#!SILK") || at(1, b"#!SILK") {
        "audio/silk"
    } else if at(0, b"#!AMR") {
        "audio/amr"
    } else if at(0, b"ID3") {
        "audio/mpeg"
    } else {
        return None;
    })
}

/// A file name safe to save and show: no directories, control characters,
/// or leading dots, and bounded in length.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let mut end = cleaned.len().min(MAX_NAME_BYTES);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].to_owned()
}

/// Save `bytes` as a new private file in `dir`, named after `name` behind a
/// unique prefix. Directories are made private too.
pub fn save(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    private_dir(dir)?;
    let name = match safe_name(name) {
        name if name.is_empty() => "file".to_owned(),
        name => name,
    };
    let path = dir.join(format!(
        "{}-{name}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(path)
}

/// Create `dir` and its missing parents with mode 0700.
pub fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Whether `path` is a regular file, not a symlink, that resolves to a
/// place inside `dir`. The bridge sends only such files from its outbox.
pub fn is_inside(dir: &Path, path: &Path) -> bool {
    let (Ok(dir), Ok(metadata)) = (std::fs::canonicalize(dir), std::fs::symlink_metadata(path))
    else {
        return false;
    };
    metadata.is_file()
        && std::fs::canonicalize(path).is_ok_and(|resolved| resolved.starts_with(&dir))
}

/// Remove files under `root` last changed more than `keep` ago, and the
/// directories they leave empty. Returns how many files went.
pub fn prune(root: &Path, keep: Duration) -> usize {
    let Some(cutoff) = SystemTime::now().checked_sub(keep) else {
        return 0;
    };
    prune_dir(root, cutoff, false)
}

fn prune_dir(dir: &Path, cutoff: SystemTime, remove_empty: bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            removed += prune_dir(&path, cutoff, true);
        } else if entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified < cutoff)
            && std::fs::remove_file(&path).is_ok()
        {
            removed += 1;
        }
    }
    if remove_empty {
        // Fails, harmlessly, while anything is left.
        let _ = std::fs::remove_dir(dir);
    }
    removed
}

/// The first bytes of `path`, for recognizing its type; empty when it
/// cannot be read.
pub fn head(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut head = Vec::with_capacity(64);
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(64).read_to_end(&mut head);
    }
    head
}

/// Read at most `max` bytes of `path`, failing when it is larger.
pub fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(anyhow!("not a regular file"));
    }
    if metadata.len() > max {
        return Err(anyhow!("file is over the {max} byte limit"));
    }
    Ok(std::fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn declared_types_win_then_magic_bytes_then_extensions() {
        assert_eq!(mime_type(Some("image/png"), "x.jpg", b""), "image/png");
        assert_eq!(
            mime_type(
                Some("application/octet-stream"),
                "x",
                b"\x89PNG\r\n\x1a\nrest"
            ),
            "image/png"
        );
        assert_eq!(
            mime_type(Some("image/*"), "x", b"\xff\xd8\xff\xe0"),
            "image/jpeg"
        );
        assert_eq!(mime_type(None, "clip", b"\0\0\0\x18ftypmp42"), "video/mp4");
        assert_eq!(mime_type(None, "voice", b"\x02#!SILK_V3"), "audio/silk");
        assert_eq!(mime_type(None, "report.PDF", b"plain"), "application/pdf");
        assert_eq!(mime_type(None, "blob", b"????"), "application/octet-stream");
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("application/x-unknown"), "bin");
        assert_eq!(MediaKind::for_mime("image/png"), MediaKind::Image);
        assert_eq!(MediaKind::for_mime("image/heic"), MediaKind::File);
        assert_eq!(MediaKind::for_mime("video/mp4"), MediaKind::Video);
    }

    #[test]
    fn names_lose_directories_control_characters_and_leading_dots() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Users\\a\\photo.jpg"), "photo.jpg");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("a\u{0}b\nc?.txt"), "a_b_c_.txt");
        assert!(safe_name(&"長".repeat(100)).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn saved_files_are_private_and_never_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("wechat/default/c1");
        let first = save(&dir, "../photo.jpg", b"one").unwrap();
        let second = save(&dir, "photo.jpg", b"two").unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(&dir));
        assert!(
            first
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-photo.jpg")
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&first), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert!(
            save(&dir, "", b"x")
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-file")
        );
    }

    #[test]
    fn prune_removes_old_files_and_their_empty_directories() {
        let root = tempfile::tempdir().unwrap();
        let old = save(&root.path().join("a/b"), "old.txt", b"x").unwrap();
        let fresh = save(&root.path().join("c"), "new.txt", b"x").unwrap();
        let past = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();
        assert_eq!(prune(root.path(), Duration::from_secs(7 * 24 * 3600)), 1);
        assert!(!old.exists());
        assert!(!root.path().join("a").exists());
        assert!(fresh.exists());
        assert!(root.path().exists());
    }

    #[test]
    fn only_regular_files_inside_the_outbox_count() {
        let root = tempfile::tempdir().unwrap();
        let outbox = root.path().join("outbox");
        let inside = save(&outbox, "a.txt", b"x").unwrap();
        let outside = save(&root.path().join("other"), "b.txt", b"x").unwrap();
        let link = outbox.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let linked_dir = outbox.join("dir");
        std::os::unix::fs::symlink(root.path().join("other"), &linked_dir).unwrap();
        assert!(!is_inside(
            &outbox,
            &linked_dir.join(outside.file_name().unwrap())
        ));
        assert!(is_inside(&outbox, &inside));
        assert!(!is_inside(&outbox, &outside));
        assert!(!is_inside(&outbox, &link));
        assert!(!is_inside(&outbox, &outbox.join("missing")));
        assert!(!is_inside(&outbox, &outbox.join("../other/b.txt")));
    }

    #[test]
    fn limits_follow_the_sender_and_kind() {
        let settings = MediaSettings::default();
        assert_eq!(
            settings.limit(MediaKind::File, true),
            Some(50 * 1024 * 1024)
        );
        assert_eq!(
            settings.limit(MediaKind::Image, false),
            Some(5 * 1024 * 1024)
        );
        assert_eq!(settings.limit(MediaKind::File, false), None);
        let off = MediaSettings {
            owner_max_mib: 0,
            ..MediaSettings::default()
        };
        assert_eq!(off.limit(MediaKind::Image, true), None);
        assert_eq!(off.limit(MediaKind::Image, false), None);
    }
}
