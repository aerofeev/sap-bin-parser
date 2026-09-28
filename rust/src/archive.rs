//! The shape of a delivered export archive.
//!
//! An export arrives as a zip of zips:
//!
//! ```text
//! BSIS.QUERY.zip
//!   BSIS.QUERY/DATA.0.zip   -> DATA.0.TXT    the schema sidecar
//!   BSIS.QUERY/DATA.1.zip   -> DATA.1.BIN    a shard of records
//!   BSIS.QUERY/DATA.2.zip   -> DATA.2.BIN
//! ```
//!
//! Shard 0 is always the sidecar. Shards from 1 up are data, either
//! fixed-width `.BIN` or tab-separated `.TXT`.

use std::io::{Cursor, Read};

use crate::error::{Error, Result};
use crate::zip::{EntryInfo, Entries, StreamReader};

/// The two data formats a shard can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShardFormat {
    Bin,
    Text,
}

impl ShardFormat {
    pub fn describe(self) -> &'static str {
        match self {
            ShardFormat::Bin => "fixed-width binary (.BIN)",
            ShardFormat::Text => "tab-separated text (.TXT)",
        }
    }
}

/// `DATA.N.BIN` / `DATA.N.TXT` at the end of a path, case-insensitive.
pub fn classify(name: &str) -> Option<(u32, ShardFormat)> {
    let base = name.rsplit('/').next().unwrap_or(name);
    let upper = base.to_ascii_uppercase();
    let rest = upper.strip_prefix("DATA.")?;
    let (index, suffix) = rest.split_once('.')?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let format = match suffix {
        "BIN" => ShardFormat::Bin,
        "TXT" => ShardFormat::Text,
        _ => return None,
    };
    Some((index.parse().ok()?, format))
}

/// The `N` in a member name like `BSIS.QUERY/DATA.N.zip`, for ordering.
pub fn member_index(name: &str) -> Option<u32> {
    let base = name.rsplit('/').next().unwrap_or(name);
    let upper = base.to_ascii_uppercase();
    let rest = upper.strip_prefix("DATA.")?;
    rest.split('.').next()?.parse().ok()
}

pub fn is_zip_member(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".zip")
}

/// The table name, from the archive's top-level directory (`BSIS.QUERY/...`
/// gives `BSIS`), else from a file name.
pub fn table_name(first_member: Option<&str>, file_name: Option<&str>) -> String {
    if let Some(head) = first_member
        .and_then(|m| m.split('/').next())
        .filter(|h| !h.is_empty())
    {
        if first_member.is_some_and(|m| m.contains('/')) {
            return head.replace(".QUERY", "").trim().to_owned();
        }
    }
    if let Some(name) = file_name {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let stem = base.split('.').next().unwrap_or(base);
        if !stem.is_empty() {
            return stem.to_owned();
        }
    }
    "export".to_owned()
}

/// The first file inside an in-memory nested zip: its header, and a reader
/// positioned at its data.
pub fn open_inner(bytes: &[u8]) -> Result<(EntryInfo, StreamReader<Cursor<&[u8]>>)> {
    let mut reader = StreamReader::new(Cursor::new(bytes));
    loop {
        match reader.next_entry()? {
            Some(info) if info.is_dir() => continue,
            Some(info) => return Ok((info, reader)),
            None => return Err(Error::Archive("a nested shard zip is empty".into())),
        }
    }
}

/// Read a whole entry, refusing more than `limit` bytes.
pub fn read_bounded(reader: &mut (impl Read + ?Sized), limit: u64, what: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    (&mut *reader).take(limit + 1).read_to_end(&mut out)?;
    if out.len() as u64 > limit {
        return Err(Error::Limit(format!(
            "{what} is larger than the {} MiB per-shard limit",
            limit >> 20
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_shards() {
        assert_eq!(classify("DATA.1.BIN"), Some((1, ShardFormat::Bin)));
        assert_eq!(classify("x/data.12.txt"), Some((12, ShardFormat::Text)));
        assert_eq!(classify("DATA.0.TXT"), Some((0, ShardFormat::Text)));
        assert_eq!(classify("DATA.1.zip"), None);
        assert_eq!(classify("README"), None);
        assert_eq!(member_index("BSIS.QUERY/DATA.10.zip"), Some(10));
    }

    #[test]
    fn names_tables() {
        assert_eq!(table_name(Some("BSIS.QUERY/"), None), "BSIS");
        assert_eq!(table_name(Some("DATA.1.BIN"), Some("BSIM.QUERY.zip")), "BSIM");
        assert_eq!(table_name(None, None), "export");
    }
}
