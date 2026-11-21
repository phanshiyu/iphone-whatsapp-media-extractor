//! Reading media records out of WhatsApp's `ChatStorage.sqlite`.

use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;

/// One media message.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaRecord {
    /// `ZWACHATSESSION.Z_PK`.
    pub chat_pk: i64,
    /// Chat JID, e.g. `1234@s.whatsapp.net`.
    pub chat_jid: String,
    /// Chat display name, if any.
    pub chat_name: Option<String>,
    /// Whether the chat is a group.
    pub is_group: bool,
    /// `ZWAMESSAGE.Z_PK`.
    pub msg_pk: i64,
    /// Core Data timestamp (seconds since 2001-01-01).
    pub apple_date: f64,
    /// `ZMEDIALOCALPATH`, relative to `Message/`.
    pub media_path: String,
    /// `ZMESSAGETYPE`.
    pub msg_type: i64,
    /// Sent by the phone's owner.
    pub from_me: bool,
    /// Sender's push name, if known.
    pub push_name: Option<String>,
    /// Sender's contact name (group members), if known.
    pub contact_name: Option<String>,
    /// Sender's JID (group members), if known.
    pub member_jid: Option<String>,
}

fn columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(names.collect::<rusqlite::Result<_>>()?)
}

/// Load all messages with a media file, ordered by date then primary key.
pub fn load_media(conn: &Connection) -> Result<Vec<MediaRecord>> {
    let msg_cols = columns(conn, "ZWAMESSAGE")?;
    let member_cols = columns(conn, "ZWAGROUPMEMBER")?;
    let push = if msg_cols.contains("ZPUSHNAME") {
        "m.ZPUSHNAME"
    } else {
        "NULL"
    };
    let has_member = member_cols.contains("Z_PK") && msg_cols.contains("ZGROUPMEMBER");
    let (contact, jid, join) = if has_member {
        (
            if member_cols.contains("ZCONTACTNAME") {
                "gm.ZCONTACTNAME"
            } else {
                "NULL"
            },
            if member_cols.contains("ZMEMBERJID") {
                "gm.ZMEMBERJID"
            } else {
                "NULL"
            },
            "LEFT JOIN ZWAGROUPMEMBER gm ON gm.Z_PK = m.ZGROUPMEMBER",
        )
    } else {
        ("NULL", "NULL", "")
    };
    let sql = format!(
        "SELECT s.Z_PK, s.ZCONTACTJID, s.ZPARTNERNAME, s.ZGROUPINFO IS NOT NULL,
                m.Z_PK, COALESCE(m.ZMESSAGEDATE, 0), mi.ZMEDIALOCALPATH,
                COALESCE(m.ZMESSAGETYPE, 0), COALESCE(m.ZISFROMME, 0),
                {push}, {contact}, {jid}
         FROM ZWAMESSAGE m
         JOIN ZWACHATSESSION s ON s.Z_PK = m.ZCHATSESSION
         JOIN ZWAMEDIAITEM mi ON mi.Z_PK = m.ZMEDIAITEM
         {join}
         WHERE mi.ZMEDIALOCALPATH IS NOT NULL AND mi.ZMEDIALOCALPATH != ''
         ORDER BY m.ZMESSAGEDATE, m.Z_PK"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(MediaRecord {
            chat_pk: r.get(0)?,
            chat_jid: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            chat_name: r.get(2)?,
            is_group: r.get(3)?,
            msg_pk: r.get(4)?,
            apple_date: r.get(5)?,
            media_path: r.get(6)?,
            msg_type: r.get(7)?,
            from_me: r.get::<_, i64>(8)? != 0,
            push_name: r.get(9)?,
            contact_name: r.get(10)?,
            member_jid: r.get(11)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
