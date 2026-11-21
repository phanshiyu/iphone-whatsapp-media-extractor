//! End-to-end tests against synthetic unencrypted and encrypted backups.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; clippy.toml only exempts #[test] functions"
)]

use iwme_core::chats::ChatInfo;
use iwme_core::crypto::{aes_wrap, cbc_encrypt};
use iwme_core::{CancelToken, DOMAIN, Dedupe, Extractor, Options, Summary};
use plist::{Dictionary, Uid, Value};
use rusqlite::{Connection, params};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

const T: i64 = 1_678_060_799; // 2023-03-05 23:59:59 UTC
const APPLE: i64 = 978_307_200;
const PASSWORD: &str = "correct horse";
const CLASS_FILES: u32 = 3;
const CLASS_MANIFEST: u32 = 4;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut acc, b| {
        write!(acc, "{b:02x}").unwrap();
        acc
    })
}

fn tlv(tag: &[u8], val: &[u8]) -> Vec<u8> {
    let mut v = tag.to_vec();
    v.extend(u32::try_from(val.len()).unwrap().to_be_bytes());
    v.extend(val);
    v
}

/// Encryption material for the encrypted fixture.
struct Enc {
    class_files: [u8; 32],
    manifest_key: [u8; 32],
    keybag: Vec<u8>,
    manifest_key_blob: Vec<u8>,
}

fn make_enc() -> Enc {
    let (salt, dpsl) = ([5u8; 20], [6u8; 20]);
    let (iter, dpic) = (7u32, 9u32);
    let mut first = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(PASSWORD.as_bytes(), &dpsl, dpic, &mut first);
    let mut pass_key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha1>(&first, &salt, iter, &mut pass_key);

    let class_files = [0x33u8; 32];
    let class_manifest = [0x44u8; 32];
    let manifest_key = [0x55u8; 32];

    let mut kb = Vec::new();
    kb.extend(tlv(b"VERS", &4u32.to_be_bytes()));
    kb.extend(tlv(b"TYPE", &1u32.to_be_bytes()));
    kb.extend(tlv(b"UUID", &[1; 16]));
    kb.extend(tlv(b"HMCK", &[2; 40]));
    kb.extend(tlv(b"WRAP", &1u32.to_be_bytes()));
    kb.extend(tlv(b"SALT", &salt));
    kb.extend(tlv(b"ITER", &iter.to_be_bytes()));
    kb.extend(tlv(b"DPWT", &1u32.to_be_bytes()));
    kb.extend(tlv(b"DPIC", &dpic.to_be_bytes()));
    kb.extend(tlv(b"DPSL", &dpsl));
    for (class, key) in [(CLASS_FILES, class_files), (CLASS_MANIFEST, class_manifest)] {
        kb.extend(tlv(b"UUID", &[u8::try_from(class).unwrap(); 16]));
        kb.extend(tlv(b"CLAS", &class.to_be_bytes()));
        kb.extend(tlv(b"WRAP", &3u32.to_be_bytes()));
        kb.extend(tlv(b"KTYP", &0u32.to_be_bytes()));
        kb.extend(tlv(b"WPKY", &aes_wrap(&pass_key, &key).unwrap()));
    }
    let mut manifest_key_blob = CLASS_MANIFEST.to_le_bytes().to_vec();
    manifest_key_blob.extend(aes_wrap(&class_manifest, &manifest_key).unwrap());
    Enc {
        class_files,
        manifest_key,
        keybag: kb,
        manifest_key_blob,
    }
}

fn archive(size: usize, wrapped_key: Option<Vec<u8>>) -> Vec<u8> {
    let mut file = Dictionary::new();
    file.insert(
        "Size".into(),
        Value::Integer(u64::try_from(size).unwrap().into()),
    );
    let mut objects = vec![Value::String("$null".into())];
    if let Some(k) = wrapped_key {
        file.insert("EncryptionKey".into(), Value::Uid(Uid::new(2)));
        file.insert(
            "ProtectionClass".into(),
            Value::Integer(u64::from(CLASS_FILES).into()),
        );
        objects.push(Value::Dictionary(file));
        let mut d = Dictionary::new();
        d.insert("NS.data".into(), Value::Data(k));
        objects.push(Value::Dictionary(d));
    } else {
        objects.push(Value::Dictionary(file));
    }
    let mut top = Dictionary::new();
    top.insert("root".into(), Value::Uid(Uid::new(1)));
    let mut root = Dictionary::new();
    root.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
    root.insert("$top".into(), Value::Dictionary(top));
    root.insert("$objects".into(), Value::Array(objects));
    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out).unwrap();
    out
}

struct Builder<'a> {
    root: &'a Path,
    enc: Option<&'a Enc>,
    manifest: Connection,
    manifest_path: std::path::PathBuf,
}

impl<'a> Builder<'a> {
    fn new(root: &'a Path, enc: Option<&'a Enc>) -> Self {
        let manifest_path = root.join("Manifest.plain.db");
        let manifest = Connection::open(&manifest_path).unwrap();
        manifest
            .execute_batch(
                "CREATE TABLE Files (fileID TEXT PRIMARY KEY, domain TEXT, relativePath TEXT, flags INTEGER, file BLOB)",
            )
            .unwrap();
        Self {
            root,
            enc,
            manifest,
            manifest_path,
        }
    }

    fn store(&self, rel: &str, content: &[u8]) {
        let id = hex(&Sha1::digest(format!("{DOMAIN}-{rel}")));
        let (bytes, blob) = self.enc.map_or_else(
            || (content.to_vec(), archive(content.len(), None)),
            |enc| {
                let file_key: [u8; 32] = Sha256::digest(&id).into();
                let mut wrapped = CLASS_FILES.to_le_bytes().to_vec();
                wrapped.extend(aes_wrap(&enc.class_files, &file_key).unwrap());
                (
                    cbc_encrypt(&file_key, content).unwrap(),
                    archive(content.len(), Some(wrapped)),
                )
            },
        );
        let dir = self.root.join(&id[..2]);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(&id), bytes).unwrap();
        self.manifest
            .execute(
                "INSERT INTO Files VALUES (?1, ?2, ?3, 1, ?4)",
                params![id, DOMAIN, rel, blob],
            )
            .unwrap();
    }

    fn finish(self) {
        let Self {
            root,
            enc,
            manifest,
            manifest_path,
        } = self;
        drop(manifest);
        let plain = fs::read(&manifest_path).unwrap();
        fs::remove_file(&manifest_path).unwrap();
        let mut plist = Dictionary::new();
        if let Some(enc) = enc {
            fs::write(
                root.join("Manifest.db"),
                cbc_encrypt(&enc.manifest_key, &plain).unwrap(),
            )
            .unwrap();
            plist.insert("IsEncrypted".into(), Value::Boolean(true));
            plist.insert("BackupKeyBag".into(), Value::Data(enc.keybag.clone()));
            plist.insert(
                "ManifestKey".into(),
                Value::Data(enc.manifest_key_blob.clone()),
            );
        } else {
            fs::write(root.join("Manifest.db"), plain).unwrap();
            plist.insert("IsEncrypted".into(), Value::Boolean(false));
        }
        Value::Dictionary(plist)
            .to_file_binary(root.join("Manifest.plist"))
            .unwrap();
    }
}

fn big() -> Vec<u8> {
    (0..(5 << 19) + 5)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect()
}

// pk, chat, unix, type, from-me flag, member, path
type Row<'a> = (i64, i64, i64, i64, i64, Option<i64>, &'a str);

/// Build `ChatStorage.sqlite`. In WAL mode the first three messages are
/// checkpointed into the main file and the rest live only in the `-wal` file;
/// both are read while the writer is still open (as in a live app).
fn chat_db(path: &Path, wal: bool) -> (Vec<u8>, Option<Vec<u8>>) {
    let db = Connection::open(path).unwrap();
    if wal {
        db.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
            .unwrap();
        db.query_row("PRAGMA wal_autocheckpoint=0", [], |_| Ok(()))
            .ok();
    }
    db.execute_batch(
        "CREATE TABLE ZWACHATSESSION (Z_PK INTEGER PRIMARY KEY, ZCONTACTJID TEXT, ZPARTNERNAME TEXT, ZGROUPINFO INTEGER);
         CREATE TABLE ZWAGROUPMEMBER (Z_PK INTEGER PRIMARY KEY, ZMEMBERJID TEXT, ZCONTACTNAME TEXT);
         CREATE TABLE ZWAMEDIAITEM (Z_PK INTEGER PRIMARY KEY, ZMEDIALOCALPATH TEXT, ZTITLE TEXT);
         CREATE TABLE ZWAMESSAGE (Z_PK INTEGER PRIMARY KEY, ZCHATSESSION INTEGER, ZMESSAGEDATE REAL,
            ZMEDIAITEM INTEGER, ZMESSAGETYPE INTEGER, ZISFROMME INTEGER, ZGROUPMEMBER INTEGER, ZPUSHNAME TEXT);
         INSERT INTO ZWACHATSESSION VALUES (1, '111@s.whatsapp.net', 'Alice', NULL);
         INSERT INTO ZWACHATSESSION VALUES (2, '222-333@g.us', 'Fam/ily', 1);
         INSERT INTO ZWACHATSESSION VALUES (3, 'status@broadcast', NULL, NULL);
         INSERT INTO ZWAGROUPMEMBER VALUES (1, '444@s.whatsapp.net', 'Bob');",
    )
    .unwrap();
    // (pk, chat, unix, type, from_me, member, path)
    let rows: [Row<'_>; 7] = [
        (1, 1, T, 1, 0, None, "Media/111@s.whatsapp.net/a/b/p1.jpg"),
        (2, 1, T, 2, 0, None, "Media/111@s.whatsapp.net/a/b/p2.mp4"),
        (
            3,
            1,
            T + 1,
            2,
            1,
            None,
            "Media/111@s.whatsapp.net/a/b/big.MOV",
        ),
        (
            4,
            2,
            T + 100,
            1,
            0,
            Some(1),
            "Media/222-333@g.us/c/d/g1.jpg",
        ),
        (5, 2, T + 100, 1, 1, None, "Media/222-333@g.us/c/d/g2.jpg"),
        (
            6,
            3,
            T + 200,
            1,
            0,
            None,
            "Media/status@broadcast/e/f/s1.jpg",
        ),
        (
            7,
            1,
            T + 5,
            1,
            0,
            None,
            "Media/111@s.whatsapp.net/a/b/gone.jpg",
        ),
    ];
    for (i, (pk, chat, unix, ty, me, member, path)) in rows.into_iter().enumerate() {
        if wal && i == 3 {
            db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .unwrap();
        }
        db.execute(
            "INSERT INTO ZWAMEDIAITEM VALUES (?1, ?2, NULL)",
            params![pk, path],
        )
        .unwrap();
        db.execute(
            "INSERT INTO ZWAMESSAGE VALUES (?1, ?2, ?3, ?1, ?4, ?5, ?6, NULL)",
            params![
                pk,
                chat,
                f64::from(i32::try_from(unix - APPLE).unwrap()),
                ty,
                me,
                member
            ],
        )
        .unwrap();
    }
    // A text message without media must be ignored.
    db.execute(
        "INSERT INTO ZWAMESSAGE VALUES (8, 1, 1.0, NULL, 0, 0, NULL, NULL)",
        [],
    )
    .unwrap();
    let wal_bytes = wal.then(|| {
        let bytes = fs::read(path.with_file_name("chat.tmp-wal")).unwrap();
        assert!(!bytes.is_empty(), "fixture must have WAL content");
        bytes
    });
    (fs::read(path).unwrap(), wal_bytes)
}

fn build_backup(root: &Path, enc: Option<&Enc>, wal: bool) {
    let b = Builder::new(root, enc);
    let db_path = root.join("chat.tmp");
    let (main, wal_bytes) = chat_db(&db_path, wal);
    b.store("ChatStorage.sqlite", &main);
    if let Some(w) = wal_bytes {
        b.store("ChatStorage.sqlite-wal", &w);
    }
    b.store("Message/Media/111@s.whatsapp.net/a/b/p1.jpg", b"alice-1");
    b.store("Message/Media/111@s.whatsapp.net/a/b/p2.mp4", b"alice-2");
    b.store("Message/Media/111@s.whatsapp.net/a/b/big.MOV", &big());
    b.store("Message/Media/222-333@g.us/c/d/g1.jpg", b"group-1");
    b.store("Message/Media/222-333@g.us/c/d/g2.jpg", b"group-2");
    b.store("Message/Media/status@broadcast/e/f/s1.jpg", b"status");
    b.finish();
}

fn run(opts: &Options) -> anyhow::Result<Summary> {
    iwme_core::run(opts, &CancelToken::new(), &|_| {})
}

fn options(backup: &Path, out: &Path, password: Option<&str>) -> Options {
    Options {
        backup_dir: backup.to_owned(),
        output_dir: out.to_owned(),
        password: password.map(str::to_owned),
        utc: true,
        ..Options::default()
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

fn mtime(p: &Path) -> i64 {
    let d = fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    i64::try_from(d.as_secs()).unwrap()
}

fn check_output(out: &Path) {
    assert_eq!(listing(out), ["Alice", "Family"]);
    assert_eq!(
        listing(&out.join("Alice")),
        [
            "2023-03-05_23-59-59_001.jpg",
            "2023-03-05_23-59-59_002.mp4",
            "2023-03-06_00-00-00_001.mov",
        ]
    );
    assert_eq!(
        listing(&out.join("Family")),
        [
            "2023-03-06_00-01-39_001_Bob.jpg",
            "2023-03-06_00-01-39_002_Me.jpg"
        ]
    );
    let alice = out.join("Alice");
    assert_eq!(
        fs::read(alice.join("2023-03-05_23-59-59_001.jpg")).unwrap(),
        b"alice-1"
    );
    assert_eq!(
        fs::read(alice.join("2023-03-05_23-59-59_002.mp4")).unwrap(),
        b"alice-2"
    );
    assert_eq!(
        fs::read(alice.join("2023-03-06_00-00-00_001.mov")).unwrap(),
        big()
    );
    assert_eq!(
        fs::read(out.join("Family/2023-03-06_00-01-39_002_Me.jpg")).unwrap(),
        b"group-2"
    );
    assert_eq!(mtime(&alice.join("2023-03-05_23-59-59_001.jpg")), T);
    assert_eq!(mtime(&alice.join("2023-03-06_00-00-00_001.mov")), T + 1);
    assert_eq!(
        mtime(&out.join("Family/2023-03-06_00-01-39_001_Bob.jpg")),
        T + 100
    );
    assert!(
        !fs::read_dir(out.join("Alice")).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".part"))
    );
}

fn check_summary(s: &Summary, extracted: usize, up_to_date: usize) {
    assert_eq!(
        *s,
        Summary {
            chats: 2,
            extracted,
            up_to_date,
            missing: 1,
            skipped: 1,
            failed: 0,
            duplicates: 0
        }
    );
}

fn full_flow(enc: Option<&Enc>, password: Option<&str>, wal: bool) {
    let backup = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    build_backup(backup.path(), enc, wal);
    let opts = options(backup.path(), out.path(), password);

    let dry = run(&Options {
        dry_run: true,
        ..opts.clone()
    })
    .unwrap();
    check_summary(&dry, 5, 0);
    assert_eq!(listing(out.path()), Vec::<String>::new());

    let first = run(&opts).unwrap();
    check_summary(&first, 5, 0);
    check_output(out.path());

    // Re-run is idempotent and repairs timestamps.
    let stale = out.path().join("Alice/2023-03-05_23-59-59_001.jpg");
    filetime_reset(&stale);
    let second = run(&opts).unwrap();
    check_summary(&second, 0, 5);
    check_output(out.path());

    // Status chats are only extracted on request.
    let with_status = run(&Options {
        include_status: true,
        ..opts
    })
    .unwrap();
    assert_eq!(with_status.skipped, 0);
    assert_eq!(listing(out.path()), ["Alice", "Family", "status"]);
}

fn filetime_reset(p: &Path) {
    let t = UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let f = fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
}

#[test]
fn unencrypted_backup() {
    full_flow(None, None, false);
}

#[test]
fn encrypted_backup() {
    let enc = make_enc();
    full_flow(Some(&enc), Some(PASSWORD), false);
}

#[test]
fn wal_only_rows_are_extracted() {
    full_flow(None, None, true);
}

#[test]
fn wal_only_rows_are_extracted_encrypted() {
    let enc = make_enc();
    full_flow(Some(&enc), Some(PASSWORD), true);
}

#[test]
fn encrypted_wrong_password() {
    let enc = make_enc();
    let backup = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    build_backup(backup.path(), Some(&enc), false);
    let err = run(&options(backup.path(), out.path(), Some("nope"))).unwrap_err();
    assert!(
        err.to_string().contains("incorrect backup password"),
        "{err:#}"
    );
    let err = run(&options(backup.path(), out.path(), None)).unwrap_err();
    assert!(err.to_string().contains("password is required"), "{err:#}");
}

#[test]
fn not_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let err = run(&options(dir.path(), out.path(), None)).unwrap_err();
    assert!(format!("{err:#}").contains("Manifest.plist"), "{err:#}");
}

#[test]
fn list_chats_contents() {
    let backup = tempfile::tempdir().unwrap();
    build_backup(backup.path(), None, false);
    let opts = options(backup.path(), Path::new("unused"), None);
    let ex = Extractor::open(&opts, &CancelToken::new(), &|_| {}).unwrap();
    let chat = |key: &str, name: &str, is_group, media_count, first, last| ChatInfo {
        key: key.into(),
        name: name.into(),
        is_group,
        media_count,
        first,
        last,
    };
    // Most recent activity first; status excluded.
    assert_eq!(
        ex.list_chats(false),
        [
            chat("222-333@g.us", "Family", true, 2, T + 100, T + 100),
            chat("111@s.whatsapp.net", "Alice", false, 4, T, T + 5),
        ]
    );
    assert_eq!(ex.list_chats(true).len(), 3);
}

#[test]
fn only_selected_chats_are_extracted() {
    let backup = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    build_backup(backup.path(), None, false);
    let opts = Options {
        chats: Some(HashSet::from(["222-333@g.us".to_owned()])),
        ..options(backup.path(), out.path(), None)
    };
    let ex = Extractor::open(&opts, &CancelToken::new(), &|_| {}).unwrap();
    let s = ex.run(&opts, &CancelToken::new(), &|_| {}).unwrap();
    assert_eq!((s.chats, s.extracted, s.missing), (1, 2, 0));
    assert_eq!(listing(out.path()), ["Family"]);
    // The same opened backup serves a second selection.
    let opts = Options {
        chats: Some(HashSet::from(["111@s.whatsapp.net".to_owned()])),
        ..opts
    };
    let s = ex.run(&opts, &CancelToken::new(), &|_| {}).unwrap();
    assert_eq!((s.chats, s.extracted, s.missing), (1, 3, 1));
    assert_eq!(listing(out.path()), ["Alice", "Family"]);
}

/// Alice sends the same bytes twice (plus one other file); Bob sends them too.
fn build_dup_backup(root: &Path) {
    let b = Builder::new(root, None);
    let db_path = root.join("chat.tmp");
    let db = Connection::open(&db_path).unwrap();
    db.execute_batch(
        "CREATE TABLE ZWACHATSESSION (Z_PK INTEGER PRIMARY KEY, ZCONTACTJID TEXT, ZPARTNERNAME TEXT, ZGROUPINFO INTEGER);
         CREATE TABLE ZWAGROUPMEMBER (Z_PK INTEGER PRIMARY KEY, ZMEMBERJID TEXT, ZCONTACTNAME TEXT);
         CREATE TABLE ZWAMEDIAITEM (Z_PK INTEGER PRIMARY KEY, ZMEDIALOCALPATH TEXT, ZTITLE TEXT);
         CREATE TABLE ZWAMESSAGE (Z_PK INTEGER PRIMARY KEY, ZCHATSESSION INTEGER, ZMESSAGEDATE REAL,
            ZMEDIAITEM INTEGER, ZMESSAGETYPE INTEGER, ZISFROMME INTEGER, ZGROUPMEMBER INTEGER, ZPUSHNAME TEXT);
         INSERT INTO ZWACHATSESSION VALUES (1, '111@s.whatsapp.net', 'Alice', NULL);
         INSERT INTO ZWACHATSESSION VALUES (2, '222@s.whatsapp.net', 'Bob', NULL);",
    )
    .unwrap();
    let files: [(i64, i64, &str, &[u8]); 4] = [
        (1, 1, "x.jpg", b"same"),
        (2, 1, "y.jpg", b"same"),
        (3, 1, "z.jpg", b"other"),
        (4, 2, "w.jpg", b"same"),
    ];
    for (pk, chat, name, content) in files {
        let path = format!("Media/{chat}/{name}");
        db.execute(
            "INSERT INTO ZWAMEDIAITEM VALUES (?1, ?2, NULL)",
            params![pk, path],
        )
        .unwrap();
        db.execute(
            "INSERT INTO ZWAMESSAGE VALUES (?1, ?2, ?3, ?1, 1, 0, NULL, NULL)",
            params![pk, chat, f64::from(i32::try_from(T - APPLE + pk).unwrap())],
        )
        .unwrap();
        b.store(&format!("Message/{path}"), content);
    }
    drop(db);
    b.store("ChatStorage.sqlite", &fs::read(&db_path).unwrap());
    b.finish();
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut v = Vec::new();
    for chat in listing(dir) {
        for f in listing(&dir.join(&chat)) {
            v.push(format!("{chat}/{f}"));
        }
    }
    v
}

#[test]
fn dedupe_modes() {
    let backup = tempfile::tempdir().unwrap();
    build_dup_backup(backup.path());
    // (mode, files kept, extracted, duplicates)
    let cases: [(Dedupe, usize, usize, usize); 3] = [
        (Dedupe::Off, 4, 4, 0),
        (Dedupe::PerChat, 3, 3, 1),
        (Dedupe::AllChats, 2, 2, 2),
    ];
    for (mode, kept, extracted, duplicates) in cases {
        let out = tempfile::tempdir().unwrap();
        let opts = Options {
            dedupe: mode,
            ..options(backup.path(), out.path(), None)
        };
        // Dry run reports the same counts without writing.
        let dry = run(&Options {
            dry_run: true,
            ..opts.clone()
        })
        .unwrap();
        assert_eq!((dry.extracted, dry.duplicates), (extracted, duplicates));
        assert_eq!(listing(out.path()), Vec::<String>::new());

        let first = run(&opts).unwrap();
        assert_eq!(
            (first.extracted, first.duplicates, first.up_to_date),
            (extracted, duplicates, 0),
            "{mode:?}"
        );
        let files = files_under(out.path());
        assert_eq!(files.len(), kept, "{mode:?}: {files:?}");
        // The earliest occurrence keeps its name; later copies are dropped.
        let has = |f: &str| files.contains(&f.to_owned());
        assert!(has("Alice/2023-03-06_00-00-00_001.jpg"));
        assert_eq!(
            has("Alice/2023-03-06_00-00-01_001.jpg"),
            mode == Dedupe::Off
        );
        // No leftover .part files.
        assert!(files.iter().all(|f| !f.contains(".part")));

        // Re-run: present files are registered, so the outcome is identical.
        let second = run(&opts).unwrap();
        assert_eq!(
            (second.extracted, second.duplicates, second.up_to_date),
            (0, duplicates, extracted),
            "{mode:?}"
        );
        assert_eq!(files_under(out.path()), files);
    }
}
