//! Output naming, sanitising, timestamps and optional exiftool tagging.

use crate::whatsapp::MediaRecord;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// Seconds between the Unix epoch and 2001-01-01 (Core Data epoch).
pub const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

/// Where (and under what name) one media file should be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// Index into the record slice given to [`plan`].
    pub record: usize,
    /// Chat folder name.
    pub dir: String,
    /// File name.
    pub name: String,
    /// Message time as Unix seconds.
    pub unix: i64,
}

/// Convert a Core Data timestamp to Unix seconds (rounded down).
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "timestamps are far inside i64 range"
)]
pub const fn apple_to_unix(apple: f64) -> i64 {
    apple.floor() as i64 + APPLE_EPOCH_OFFSET
}

/// Make a string safe as a single path component on common filesystems.
#[must_use]
pub fn sanitize_component(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let mut out = String::new();
    for c in cleaned.trim().chars().take(max_chars) {
        // Stay well below the 255-byte filename limit even for CJK/emoji.
        if out.len() + c.len_utf8() > 200 {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end_matches(['.', ' ']).trim_start();
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out.to_owned()
    }
}

/// The user part of a JID (`1234@s.whatsapp.net` -> `1234`).
#[must_use]
pub fn jid_user(jid: &str) -> &str {
    jid.split('@').next().unwrap_or(jid)
}

/// Lowercase file extension from the media path, falling back to the message type.
#[must_use]
pub fn extension_for(media_path: &str, msg_type: i64) -> String {
    let ext = Path::new(media_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()));
    ext.unwrap_or_else(|| {
        match msg_type {
            1 => "jpg",
            2 => "mp4",
            3 => "opus",
            11 => "gif",
            15 => "webp",
            _ => "bin",
        }
        .to_owned()
    })
}

fn sender_label(r: &MediaRecord) -> String {
    let raw = if r.from_me {
        "Me".to_owned()
    } else {
        [r.contact_name.as_deref(), r.push_name.as_deref()]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| r.member_jid.as_deref().map(|j| jid_user(j).to_owned()))
            .unwrap_or_else(|| "unknown".to_owned())
    };
    sanitize_component(&raw, 40)
}

/// Format a Unix time in UTC or local time.
#[must_use]
pub fn format_time(unix: i64, utc: bool, pattern: &str) -> String {
    let ts = Timestamp::from_second(unix).unwrap_or_default();
    let tz = if utc {
        TimeZone::UTC
    } else {
        TimeZone::system()
    };
    ts.to_zoned_datetime(tz).strftime(pattern).to_string()
}

/// Decide folder and file names for every record. `records` must already be
/// ordered by date then primary key (as returned by `whatsapp::load_media`).
#[must_use]
pub fn plan(records: &[MediaRecord], utc: bool) -> Vec<PlannedFile> {
    // Chat folders, assigned in chat primary-key order so results are stable.
    let mut chats: BTreeMap<i64, &MediaRecord> = BTreeMap::new();
    for r in records {
        chats.entry(r.chat_pk).or_insert(r);
    }
    let mut dirs: HashMap<i64, String> = HashMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    for (pk, r) in &chats {
        let jid_part = jid_user(&r.chat_jid);
        let base = sanitize_component(
            r.chat_name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(jid_part),
            100,
        );
        let mut name = base.clone();
        if taken.contains(&name.to_lowercase()) {
            name = sanitize_component(&format!("{base} ({jid_part})"), 120);
        }
        if taken.contains(&name.to_lowercase()) {
            name = format!("{name} [{pk}]");
        }
        taken.insert(name.to_lowercase());
        dirs.insert(*pk, name);
    }

    let mut seq: HashMap<(i64, i64), u32> = HashMap::new();
    records
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let unix = apple_to_unix(r.apple_date);
            let n = seq.entry((r.chat_pk, unix)).or_insert(0);
            *n += 1;
            let mut name = format!("{}_{:03}", format_time(unix, utc, "%Y-%m-%d_%H-%M-%S"), n);
            if r.is_group {
                name.push('_');
                name.push_str(&sender_label(r));
            }
            name.push('.');
            name.push_str(&extension_for(&r.media_path, r.msg_type));
            PlannedFile {
                record: i,
                dir: dirs.get(&r.chat_pk).cloned().unwrap_or_default(),
                name,
                unix,
            }
        })
        .collect()
}

/// Set mtime/atime (and the creation time on macOS) to `unix`.
pub fn set_file_times(path: &Path, unix: i64) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Best effort: birth time is not essential.
        let _ = set_birth_time(path, unix);
    }
    let t = u64::try_from(unix).map_or(std::time::UNIX_EPOCH, |s| {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(s)
    });
    let times = std::fs::FileTimes::new().set_accessed(t).set_modified(t);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_times(times)
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "setattrlist has no safe wrapper; needed for the creation time"
)]
fn set_birth_time(path: &Path, unix: i64) -> io::Result<()> {
    use std::ffi::{CString, c_void};
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_CRTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut ts = libc::timespec {
        tv_sec: unix,
        tv_nsec: 0,
    };
    // SAFETY: `c_path` is a valid NUL-terminated string, `attrs` and `ts` are
    // valid for the duration of the call and the buffer size matches `ts`.
    let rc = unsafe {
        libc::setattrlist(
            c_path.as_ptr(),
            std::ptr::from_mut(&mut attrs).cast::<c_void>(),
            std::ptr::from_mut(&mut ts).cast::<c_void>(),
            size_of::<libc::timespec>(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether `exiftool` can be run.
#[must_use]
pub fn exiftool_available() -> bool {
    crate::tools::find_tool("exiftool").is_some_and(|p| {
        Command::new(p)
            .arg("-ver")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Best-effort: write date tags with exiftool. Returns whether it ran successfully.
#[must_use]
pub fn write_exif_dates(path: &Path, ext: &str, unix: i64, utc: bool) -> bool {
    let Some(exiftool) = crate::tools::find_tool("exiftool") else {
        return false;
    };
    let mut cmd = Command::new(exiftool);
    cmd.args(["-q", "-overwrite_original"]);
    match ext {
        "jpg" | "jpeg" | "heic" | "png" => {
            cmd.arg(format!(
                "-AllDates={}",
                format_time(unix, utc, "%Y:%m:%d %H:%M:%S")
            ));
        }
        "mp4" | "mov" => {
            // QuickTime dates are defined as UTC.
            let d = format_time(unix, true, "%Y:%m:%d %H:%M:%S");
            for tag in [
                "QuickTime:CreateDate",
                "QuickTime:ModifyDate",
                "QuickTime:TrackCreateDate",
                "QuickTime:TrackModifyDate",
                "QuickTime:MediaCreateDate",
                "QuickTime:MediaModifyDate",
            ] {
                cmd.arg(format!("-{tag}={d}"));
            }
        }
        _ => return false,
    }
    cmd.arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pk: i64, chat: i64, name: Option<&str>, unix: i64, group: bool) -> MediaRecord {
        MediaRecord {
            chat_pk: chat,
            chat_jid: format!("{}@s.whatsapp.net", 1000 + chat),
            chat_name: name.map(str::to_owned),
            is_group: group,
            msg_pk: pk,
            apple_date: f64::from(i32::try_from(unix - APPLE_EPOCH_OFFSET).unwrap()),
            media_path: "Media/x/a/b/u.JPG".into(),
            msg_type: 1,
            from_me: false,
            push_name: None,
            contact_name: None,
            member_jid: None,
        }
    }

    #[test]
    fn sanitiser() {
        assert_eq!(
            sanitize_component("a/b\\c:d*e?f\"g<h>i|j", 100),
            "abcdefghij"
        );
        assert_eq!(sanitize_component("  Mum. . ", 100), "Mum");
        assert_eq!(sanitize_component("a\u{0}b\nc", 100), "abc");
        assert_eq!(sanitize_component("???", 100), "unknown");
        assert_eq!(sanitize_component(&"x".repeat(300), 100).len(), 100);
        assert!(sanitize_component(&"漢".repeat(300), 100).len() <= 200);
        assert_eq!(sanitize_component("Zoë 🎉", 100), "Zoë 🎉");
    }

    #[test]
    fn apple_epoch() {
        assert_eq!(apple_to_unix(0.0), 978_307_200);
        assert_eq!(apple_to_unix(699_753_599.9), 1_678_060_799);
        assert_eq!(apple_to_unix(-0.5), 978_307_199);
    }

    #[test]
    fn extensions() {
        assert_eq!(extension_for("Media/a/b/x.JPG", 2), "jpg");
        assert_eq!(extension_for("Media/a/b/x", 2), "mp4");
        assert_eq!(extension_for("Media/a/b/x.", 3), "opus");
        assert_eq!(extension_for("Media/a/b/x", 99), "bin");
    }

    #[test]
    fn names_sort_chronologically() {
        let t = 1_678_060_799; // 2023-03-05 23:59:59 UTC
        let recs = vec![
            rec(1, 1, Some("Alice"), t, false),
            rec(2, 1, Some("Alice"), t, false),
            rec(3, 1, Some("Alice"), t + 1, false), // crosses midnight
            rec(4, 1, Some("Alice"), t + 90_000, false),
        ];
        let p = plan(&recs, true);
        let names: Vec<_> = p.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "2023-03-05_23-59-59_001.jpg",
                "2023-03-05_23-59-59_002.jpg",
                "2023-03-06_00-00-00_001.jpg",
                "2023-03-07_00-59-59_001.jpg",
            ]
        );
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, names);
        assert!(p.iter().all(|f| f.dir == "Alice"));
    }

    #[test]
    fn group_sender_and_collisions() {
        let t = 1_700_000_000;
        let mut g = rec(1, 1, Some("Fam/ily"), t, true);
        g.contact_name = Some("Bob: B".into());
        let mut g2 = rec(2, 1, Some("Fam/ily"), t, true);
        g2.from_me = true;
        let mut g3 = rec(3, 1, Some("Fam/ily"), t + 1, true);
        g3.member_jid = Some("555@s.whatsapp.net".into());
        let other = rec(4, 2, Some("family"), t, false); // same name modulo case
        let p = plan(&[g, g2, g3, other], true);
        assert_eq!(p[0].dir, "Family");
        assert!(p[0].name.ends_with("_001_Bob B.jpg"), "{}", p[0].name);
        assert!(p[1].name.ends_with("_002_Me.jpg"));
        assert!(p[2].name.ends_with("_001_555.jpg"));
        assert_eq!(p[3].dir, "family (1002)");
    }

    #[test]
    fn chat_name_falls_back_to_jid() {
        let p = plan(&[rec(1, 7, None, 1_700_000_000, false)], true);
        assert_eq!(p[0].dir, "1007");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sets_birth_time_on_macos() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        std::fs::write(&f, b"x").unwrap();
        set_file_times(&f, 1_500_000_000).unwrap();
        let created = std::fs::metadata(&f).unwrap().created().unwrap();
        let secs = created
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, 1_500_000_000);
    }
}
