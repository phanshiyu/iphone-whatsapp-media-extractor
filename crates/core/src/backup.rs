//! Reading an iOS (Finder/iTunes) backup: manifest, file lookup, decryption.

use crate::crypto::cbc_decrypt_stream;
use crate::error::access_error;
use crate::keybag::{ClassKeys, Keybag};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Cursor};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// One file stored in the backup, as described by `Manifest.db`.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Hashed file name (`<backup>/<id[0..2]>/<id>`).
    pub file_id: String,
    /// Path within the app domain.
    pub relative_path: String,
    /// Plaintext size from the MBFile record, if present.
    pub size: Option<u64>,
    /// `protection class (LE u32) || AES-wrapped file key`, for encrypted backups.
    pub key_blob: Option<Vec<u8>>,
}

/// An opened backup (read-only).
#[derive(Debug)]
pub struct Backup {
    dir: PathBuf,
    keys: Option<ClassKeys>,
    manifest: Connection,
    // Keeps the decrypted manifest alive for the lifetime of the connection.
    _manifest_tmp: Option<NamedTempFile>,
}

fn read_manifest_plist(dir: &Path) -> Result<plist::Dictionary> {
    let path = dir.join("Manifest.plist");
    let file = File::open(&path).map_err(|e| {
        let what = format!("cannot read {}", path.display());
        if e.kind() == io::ErrorKind::PermissionDenied {
            access_error(e, &what)
        } else {
            anyhow!("{what}: {e} (is this an iPhone backup folder?)")
        }
    })?;
    let value = plist::Value::from_reader(BufReader::new(file))
        .with_context(|| format!("cannot parse {}", path.display()))?;
    value
        .into_dictionary()
        .ok_or_else(|| anyhow!("Manifest.plist is not a dictionary"))
}

impl Backup {
    /// Whether the backup at `dir` is encrypted.
    pub fn is_encrypted(dir: &Path) -> Result<bool> {
        let plist = read_manifest_plist(dir)?;
        Ok(plist
            .get("IsEncrypted")
            .and_then(plist::Value::as_boolean)
            .unwrap_or(false))
    }

    /// Check `password` against the keybag of an encrypted backup without
    /// reading the manifest (fails fast on a typo).
    pub fn verify_password(dir: &Path, password: &str) -> Result<()> {
        unlock_keys(&read_manifest_plist(dir)?, password).map(|_| ())
    }

    /// Whether this backup is encrypted.
    #[must_use]
    pub const fn encrypted(&self) -> bool {
        self.keys.is_some()
    }

    /// Size of the stored (possibly encrypted) file in the backup folder.
    #[must_use]
    pub fn stored_len(&self, entry: &FileEntry) -> Option<u64> {
        fs::metadata(self.stored_path(entry)).ok().map(|m| m.len())
    }

    fn stored_path(&self, entry: &FileEntry) -> PathBuf {
        let prefix = entry.file_id.get(..2).unwrap_or("");
        self.dir.join(prefix).join(&entry.file_id)
    }

    /// Open the backup, decrypting the manifest if needed.
    pub fn open(dir: &Path, password: Option<&str>) -> Result<Self> {
        let plist = read_manifest_plist(dir)?;
        let encrypted = plist
            .get("IsEncrypted")
            .and_then(plist::Value::as_boolean)
            .unwrap_or(false);
        let manifest_path = dir.join("Manifest.db");
        if !encrypted {
            let manifest = open_sqlite(&manifest_path)?;
            return Ok(Self {
                dir: dir.to_owned(),
                keys: None,
                manifest,
                _manifest_tmp: None,
            });
        }
        let Some(password) = password else {
            bail!("the backup is encrypted; a password is required");
        };
        let keys = unlock_keys(&plist, password)?;
        let manifest_key = plist
            .get("ManifestKey")
            .and_then(plist::Value::as_data)
            .context("Manifest.plist has no ManifestKey")?;
        let key = keys
            .unwrap_blob(manifest_key)
            .context("cannot unwrap the Manifest.db key")?;
        let tmp = NamedTempFile::new().context("cannot create temp file for Manifest.db")?;
        let input = File::open(&manifest_path)
            .map_err(|e| access_error(e, &format!("cannot open {}", manifest_path.display())))?;
        cbc_decrypt_stream(
            &key,
            BufReader::new(input),
            BufWriter::new(tmp.as_file()),
            None,
        )
        .context("cannot decrypt Manifest.db")?;
        let manifest = open_sqlite(tmp.path())?;
        Ok(Self {
            dir: dir.to_owned(),
            keys: Some(keys),
            manifest,
            _manifest_tmp: Some(tmp),
        })
    }

    /// All regular files of `domain`, keyed by relative path.
    pub fn files_in_domain(&self, domain: &str) -> Result<HashMap<String, FileEntry>> {
        let mut stmt = self.manifest.prepare(
            "SELECT fileID, relativePath, file FROM Files WHERE domain = ?1 AND flags = 1",
        )?;
        let rows = stmt.query_map([domain], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (file_id, relative_path, blob) = row?;
            let (size, key_blob) = blob.as_deref().map(parse_mbfile).unwrap_or_default();
            map.insert(
                relative_path.clone(),
                FileEntry {
                    file_id,
                    relative_path,
                    size,
                    key_blob,
                },
            );
        }
        Ok(map)
    }

    /// Copy (and decrypt, if needed) a backup file to `dest`.
    /// Returns the number of bytes written.
    pub fn copy_file_to(&self, entry: &FileEntry, dest: &Path) -> Result<u64> {
        let src = self.stored_path(entry);
        let input = File::open(&src)
            .map_err(|e| access_error(e, &format!("cannot open {}", src.display())))?;
        let mut part = dest.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let result = self.copy_inner(entry, input, &part);
        match result {
            Ok(n) => {
                fs::rename(&part, dest)
                    .with_context(|| format!("cannot move into place: {}", dest.display()))?;
                Ok(n)
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                Err(e)
            }
        }
    }

    fn copy_inner(&self, entry: &FileEntry, input: File, part: &Path) -> Result<u64> {
        let out = BufWriter::new(
            File::create(part).with_context(|| format!("cannot create {}", part.display()))?,
        );
        let mut input = BufReader::new(input);
        if let Some(keys) = &self.keys {
            let blob = entry
                .key_blob
                .as_deref()
                .context("file has no encryption key in the manifest")?;
            let key = keys.unwrap_blob(blob)?;
            cbc_decrypt_stream(&key, input, out, entry.size)
        } else {
            let mut out = out;
            Ok(io::copy(&mut input, &mut out)?)
        }
    }
}

fn unlock_keys(plist: &plist::Dictionary, password: &str) -> Result<ClassKeys> {
    if !plist
        .get("IsEncrypted")
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false)
    {
        bail!("the backup is not encrypted");
    }
    let bag = plist
        .get("BackupKeyBag")
        .and_then(plist::Value::as_data)
        .context("Manifest.plist has no BackupKeyBag")?;
    Keybag::parse(bag)?.unlock(password)
}

fn open_sqlite(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open database {}", path.display()))?;
    // Force a read so that a wrongly decrypted / corrupt file fails early.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .with_context(|| format!("{} is not a valid SQLite database", path.display()))?;
    Ok(conn)
}

/// Extract `(Size, EncryptionKey bytes)` from an NSKeyedArchiver MBFile blob.
fn parse_mbfile(blob: &[u8]) -> (Option<u64>, Option<Vec<u8>>) {
    parse_mbfile_inner(blob).unwrap_or_default()
}

fn parse_mbfile_inner(blob: &[u8]) -> Option<(Option<u64>, Option<Vec<u8>>)> {
    let value = plist::Value::from_reader(Cursor::new(blob)).ok()?;
    let root = value.as_dictionary()?;
    let objects = root.get("$objects")?.as_array()?;
    let top = usize::try_from(
        root.get("$top")?
            .as_dictionary()?
            .get("root")?
            .as_uid()?
            .get(),
    )
    .ok()?;
    let file = objects.get(top)?.as_dictionary()?;
    let size = file.get("Size").and_then(|v| {
        v.as_unsigned_integer()
            .or_else(|| v.as_signed_integer().and_then(|s| u64::try_from(s).ok()))
    });
    let key = file.get("EncryptionKey").and_then(|v| match v.as_uid() {
        Some(uid) => {
            let idx = usize::try_from(uid.get()).ok()?;
            let d = objects.get(idx)?.as_dictionary()?;
            d.get("NS.data")?.as_data().map(<[u8]>::to_vec)
        }
        None => v.as_data().map(<[u8]>::to_vec),
    });
    Some((size, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::{Dictionary, Uid, Value};

    #[test]
    fn parses_keyed_archive_blob() {
        let mut file = Dictionary::new();
        file.insert("Size".into(), Value::Integer(1234u64.into()));
        file.insert("EncryptionKey".into(), Value::Uid(Uid::new(2)));
        let mut key = Dictionary::new();
        key.insert("NS.data".into(), Value::Data(vec![3, 0, 0, 0, 9, 9]));
        let mut top = Dictionary::new();
        top.insert("root".into(), Value::Uid(Uid::new(1)));
        let mut root = Dictionary::new();
        root.insert("$top".into(), Value::Dictionary(top));
        root.insert(
            "$objects".into(),
            Value::Array(vec![
                Value::String("$null".into()),
                Value::Dictionary(file),
                Value::Dictionary(key),
            ]),
        );
        let mut buf = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut buf).unwrap();
        let (size, key) = parse_mbfile(&buf);
        assert_eq!(size, Some(1234));
        assert_eq!(key, Some(vec![3, 0, 0, 0, 9, 9]));
        assert_eq!(parse_mbfile(b"garbage"), (None, None));
    }
}
