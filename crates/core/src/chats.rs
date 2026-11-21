//! Listing chats and matching user-supplied chat patterns.

use anyhow::{Result, bail};
use std::collections::HashSet;

/// A chat that has at least one media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    /// Stable identifier: the chat JID.
    pub key: String,
    /// Folder name used for this chat in the output.
    pub name: String,
    /// Whether this is a group chat.
    pub is_group: bool,
    /// Number of media messages.
    pub media_count: usize,
    /// Unix time of the first media message.
    pub first: i64,
    /// Unix time of the last media message.
    pub last: i64,
}

/// Resolve patterns to chat keys. A pattern matches an exact JID, else a
/// folder name case-insensitively (exact, then substring).
pub fn resolve_patterns(chats: &[ChatInfo], patterns: &[String]) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    for pat in patterns {
        let lower = pat.to_lowercase();
        let by = |f: &dyn Fn(&ChatInfo) -> bool| -> Vec<&ChatInfo> {
            chats.iter().filter(|c| f(c)).collect()
        };
        let mut hits = by(&|c| c.key == *pat);
        if hits.is_empty() {
            hits = by(&|c| c.name.to_lowercase() == lower);
        }
        if hits.is_empty() {
            hits = by(&|c| c.name.to_lowercase().contains(&lower));
        }
        if hits.is_empty() {
            let names: Vec<&str> = chats.iter().map(|c| c.name.as_str()).collect();
            bail!(
                "no chat matches {pat:?}; chats with media: {} (see --list-chats)",
                names.join(", ")
            );
        }
        keys.extend(hits.into_iter().map(|c| c.key.clone()));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(key: &str, name: &str) -> ChatInfo {
        ChatInfo {
            key: key.into(),
            name: name.into(),
            is_group: false,
            media_count: 1,
            first: 0,
            last: 0,
        }
    }

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn matching() {
        let chats = [
            chat("1@s.whatsapp.net", "Alice"),
            chat("2@s.whatsapp.net", "Alice Smith"),
            chat("3@g.us", "Family"),
        ];
        let keys = |p: &[&str]| resolve_patterns(&chats, &pats(p)).unwrap();
        // Exact folder name beats substring.
        assert_eq!(keys(&["alice"]), HashSet::from(["1@s.whatsapp.net".into()]));
        // Substring falls back to all matches.
        assert_eq!(keys(&["smi"]), HashSet::from(["2@s.whatsapp.net".into()]));
        assert_eq!(keys(&["a"]).len(), 3);
        // JID is exact.
        assert_eq!(keys(&["3@g.us"]), HashSet::from(["3@g.us".into()]));
        // Union of several patterns.
        assert_eq!(keys(&["family", "Alice Smith"]).len(), 2);
        let err = resolve_patterns(&chats, &pats(&["nobody"])).unwrap_err();
        assert!(err.to_string().contains("Alice, Alice Smith, Family"));
    }
}
