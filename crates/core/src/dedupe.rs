//! Content identity (size + SHA-256 of the plaintext) for duplicate detection.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// How to treat files with identical content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dedupe {
    /// Keep every file.
    #[default]
    Off,
    /// Skip a file identical to an earlier one in the same chat.
    PerChat,
    /// Skip a file identical to an earlier one in any chat.
    AllChats,
}

/// Size and hash of a file's plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentId {
    size: u64,
    hash: [u8; 32],
}

/// A writer that hashes everything passing through it.
#[derive(Debug)]
pub struct HashWriter<W> {
    inner: W,
    hasher: Sha256,
    size: u64,
}

impl<W: Write> HashWriter<W> {
    /// Wrap `inner`.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            size: 0,
        }
    }

    /// Return the wrapped writer and the identity of what was written.
    pub fn finish(self) -> (W, ContentId) {
        let id = ContentId {
            size: self.size,
            hash: self.hasher.finalize().into(),
        };
        (self.inner, id)
    }
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(buf.get(..n).unwrap_or_default());
        self.size += u64::try_from(n).unwrap_or(0);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Identity of the file at `path`.
pub fn hash_file(path: &Path) -> io::Result<ContentId> {
    let mut w = HashWriter::new(io::sink());
    io::copy(&mut File::open(path)?, &mut w)?;
    Ok(w.finish().1)
}

/// Content seen so far, scoped according to the [`Dedupe`] mode.
#[derive(Debug)]
pub struct Index {
    mode: Dedupe,
    seen: HashSet<(i64, ContentId)>,
}

impl Index {
    /// An empty index.
    #[must_use]
    pub fn new(mode: Dedupe) -> Self {
        Self {
            mode,
            seen: HashSet::new(),
        }
    }

    /// Whether duplicates are being detected at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.mode != Dedupe::Off
    }

    /// Register content in chat `chat_pk`; false if it was already seen.
    pub fn insert(&mut self, chat_pk: i64, id: ContentId) -> bool {
        let scope = if self.mode == Dedupe::PerChat {
            chat_pk
        } else {
            0
        };
        self.seen.insert((scope, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(data: &[u8]) -> ContentId {
        let mut w = HashWriter::new(io::sink());
        w.write_all(data).unwrap();
        w.finish().1
    }

    #[test]
    fn scopes() {
        let mut per = Index::new(Dedupe::PerChat);
        assert!(per.insert(1, id(b"a")));
        assert!(!per.insert(1, id(b"a")));
        assert!(per.insert(2, id(b"a")));
        let mut all = Index::new(Dedupe::AllChats);
        assert!(all.insert(1, id(b"a")));
        assert!(!all.insert(2, id(b"a")));
        assert!(all.insert(2, id(b"b")));
        assert!(!Index::new(Dedupe::Off).enabled());
    }
}
