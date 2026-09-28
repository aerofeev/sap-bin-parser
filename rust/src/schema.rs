//! Schema handling for SAP flat-file exports.
//!
//! Every export ships its own schema as a tab-separated sidecar
//! (`DATA.0.TXT`) with the columns `NAME TABLE TYPE LENG DEC SIZE ROLL KEY`:
//!
//! ```text
//! NAME    TABLE   TYPE    LENG    DEC     SIZE    ROLL    KEY
//! BUKRS           C       4       0       8       BUKRS
//! DMBTR           P       7       2       7       DMBTR
//! ```
//!
//! `SIZE` is the field's width in bytes and is authoritative for every type:
//! for the character-ish types it is always `LENG * 2` (UTF-16), and for
//! packed decimals it is the raw byte count. So field widths never need to be
//! derived.

use serde::Serialize;

use crate::error::{Error, Result};

/// The five SAP dictionary types these exports use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum FieldType {
    /// Character text, UTF-16BE, space padded.
    C,
    /// Numeric text (digits stored as characters), UTF-16BE.
    N,
    /// Date as `YYYYMMDD` text.
    D,
    /// Time as `HHMMSS` text.
    T,
    /// Packed decimal (COMP-3 / BCD).
    P,
}

impl FieldType {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "C" => Some(Self::C),
            "N" => Some(Self::N),
            "D" => Some(Self::D),
            "T" => Some(Self::T),
            "P" => Some(Self::P),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::N => "N",
            Self::D => "D",
            Self::T => "T",
            Self::P => "P",
        }
    }

    /// True when the field is stored as UTF-16BE text.
    pub fn is_wide_text(self) -> bool {
        !matches!(self, Self::P)
    }
}

/// One column of a fixed-width SAP record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: FieldType,
    pub length: usize,
    pub decimals: u32,
    pub size: usize,
}

impl Field {
    pub fn new(name: &str, kind: &str, length: usize, decimals: u32, size: usize) -> Result<Self> {
        let Some(parsed) = FieldType::parse(kind) else {
            return Err(Error::Schema(format!(
                "field '{name}' has unsupported type '{kind}' (known: C, D, N, P, T)"
            )));
        };
        if size < 1 {
            return Err(Error::Schema(format!(
                "field '{name}' has non-positive size {size}"
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            kind: parsed,
            length,
            decimals,
            size,
        })
    }

    /// The width `SIZE` should have, given `TYPE` and `LENG`.
    pub fn implied_size(&self) -> usize {
        if self.kind.is_wide_text() {
            self.length * 2
        } else {
            self.size
        }
    }
}

/// An ordered set of fields, plus the record geometry they imply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
    offsets: Vec<usize>,
    payload_size: usize,
    pub name: Option<String>,
}

impl Schema {
    pub fn new(fields: Vec<Field>, name: Option<String>) -> Result<Self> {
        if fields.is_empty() {
            return Err(Error::Schema("schema has no fields".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for field in &fields {
            if !seen.insert(field.name.as_str()) {
                return Err(Error::Schema(format!(
                    "duplicate field name '{}'",
                    field.name
                )));
            }
        }
        let mut offsets = Vec::with_capacity(fields.len());
        let mut offset = 0;
        for field in &fields {
            offsets.push(offset);
            offset += field.size;
        }
        Ok(Self {
            fields,
            offsets,
            payload_size: offset,
            name,
        })
    }

    /// Build a schema from `(name, type, length, decimals, size)` tuples.
    pub fn from_tuples<'a>(
        rows: impl IntoIterator<Item = (&'a str, &'a str, usize, u32, usize)>,
        name: Option<&str>,
    ) -> Result<Self> {
        let fields = rows
            .into_iter()
            .map(|(n, t, l, d, s)| Field::new(n, t, l, d, s))
            .collect::<Result<Vec<_>>>()?;
        Self::new(fields, name.map(str::to_owned))
    }

    /// Parse the text of a `DATA.0.TXT` schema sidecar.
    pub fn parse(text: &str, name: Option<&str>) -> Result<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let rows: Vec<Vec<&str>> = text
            .lines()
            .map(|line| line.split('\t').map(str::trim).collect::<Vec<_>>())
            .filter(|cells| cells.iter().any(|c| !c.is_empty()))
            .collect();
        if rows.is_empty() {
            return Err(Error::Schema("schema sidecar is empty".into()));
        }

        let header: Vec<String> = rows[0].iter().map(|c| c.to_ascii_uppercase()).collect();
        if !header.iter().any(|h| h == "NAME") || !header.iter().any(|h| h == "TYPE") {
            return Err(Error::Schema(format!(
                "schema sidecar has no recognisable header; got {:?}. Expected tab-separated \
                 columns including NAME, TYPE, LENG, DEC, SIZE.",
                rows[0]
            )));
        }
        let expected = ["NAME", "TYPE", "LENG", "DEC", "SIZE"];
        let mut index = [0usize; 5];
        let mut missing = Vec::new();
        for (slot, key) in expected.iter().enumerate() {
            match header.iter().position(|h| h == key) {
                Some(position) => index[slot] = position,
                None => missing.push(*key),
            }
        }
        if !missing.is_empty() {
            return Err(Error::Schema(format!(
                "schema sidecar is missing column(s): {}",
                missing.join(", ")
            )));
        }
        let widest = *index.iter().max().unwrap();

        let mut fields = Vec::new();
        for (line_number, row) in rows.iter().enumerate().skip(1) {
            let line_number = line_number + 1;
            if row.len() <= widest {
                return Err(Error::Schema(format!(
                    "line {line_number}: expected {} columns, got {}",
                    header.len(),
                    row.len()
                )));
            }
            let field_name = row[index[0]];
            if field_name.is_empty() {
                continue;
            }
            let number = |cell: &str| -> std::result::Result<usize, ()> {
                if cell.is_empty() {
                    Ok(0)
                } else {
                    cell.parse::<usize>().map_err(|_| ())
                }
            };
            let (length, decimals, size) = match (
                number(row[index[2]]),
                number(row[index[3]]),
                number(row[index[4]]),
            ) {
                (Ok(l), Ok(d), Ok(s)) => (l, d, s),
                _ => {
                    return Err(Error::Schema(format!(
                        "line {line_number}: non-numeric LENG/DEC/SIZE in {row:?}"
                    )))
                }
            };
            let kind = row[index[1]].to_ascii_uppercase();
            let field = Field::new(field_name, &kind, length, decimals as u32, size)
                .map_err(|e| Error::Schema(format!("line {line_number}: {e}")))?;
            fields.push(field);
        }

        Self::new(fields, name.map(str::to_owned))
    }

    /// Parse a sidecar delivered as bytes: UTF-8 with an optional BOM, with
    /// undecodable bytes replaced rather than rejected.
    pub fn parse_bytes(bytes: &[u8], name: Option<&str>) -> Result<Self> {
        let text = String::from_utf8_lossy(bytes);
        Self::parse(&text, name)
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(|f| f.name.as_str())
    }

    /// Each field with its byte offset into a record.
    pub fn offsets(&self) -> impl Iterator<Item = (&Field, usize)> {
        self.fields.iter().zip(self.offsets.iter().copied())
    }

    /// Total bytes the declared fields occupy, before record padding.
    pub fn payload_size(&self) -> usize {
        self.payload_size
    }

    /// Bytes per record on disk.
    ///
    /// Records are padded to an even boundary so that each one begins on a
    /// two-byte boundary and its UTF-16 fields stay aligned. A schema whose
    /// fields sum to an odd width therefore carries one trailing pad byte,
    /// whose content is unspecified.
    pub fn record_size(&self) -> usize {
        self.payload_size + (self.payload_size % 2)
    }

    /// Trailing pad bytes per record: 1 for an odd payload, else 0.
    pub fn padding_size(&self) -> usize {
        self.record_size() - self.payload_size
    }

    /// Fields whose `SIZE` disagrees with `TYPE`/`LENG`.
    ///
    /// Empty for every export seen so far; a non-empty result means the
    /// sidecar is unusual and the geometry deserves a second look.
    pub fn inconsistencies(&self) -> Vec<String> {
        self.fields
            .iter()
            .filter(|f| f.kind.is_wide_text() && f.size != f.implied_size())
            .map(|f| {
                format!(
                    "{}: SIZE={} but TYPE={} LENG={} implies {}",
                    f.name,
                    f.size,
                    f.kind.as_str(),
                    f.length,
                    f.implied_size()
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::BSIS_SIDECAR;

    #[test]
    fn reads_every_field() {
        let schema = Schema::parse(BSIS_SIDECAR, Some("BSIS")).unwrap();
        assert_eq!(schema.len(), 9);
        assert_eq!(schema.fields()[0].name, "BUKRS");
        let dmbtr = &schema.fields()[8];
        assert_eq!(
            (dmbtr.kind, dmbtr.length, dmbtr.decimals, dmbtr.size),
            (FieldType::P, 7, 2, 7)
        );
    }

    #[test]
    fn odd_payload_pads_to_even_record() {
        let schema = Schema::parse(BSIS_SIDECAR, None).unwrap();
        assert_eq!(schema.payload_size(), 125);
        assert_eq!(schema.record_size(), 126);
        assert_eq!(schema.padding_size(), 1);
        let offsets: Vec<usize> = schema.offsets().map(|(_, o)| o).collect();
        assert_eq!(&offsets[..3], &[0, 8, 28]);
        assert_eq!(offsets[8], 118);
    }

    #[test]
    fn tolerates_a_bom() {
        let schema = Schema::parse(&format!("\u{feff}{BSIS_SIDECAR}"), None).unwrap();
        assert_eq!(schema.len(), 9);
    }

    #[test]
    fn rejects_bad_sidecars() {
        assert!(matches!(Schema::parse("", None), Err(Error::Schema(m)) if m.contains("empty")));
        assert!(matches!(
            Schema::parse("BUKRS\tC\t4\t0\t8\n", None),
            Err(Error::Schema(m)) if m.contains("header")
        ));
        let bad = "NAME\tTABLE\tTYPE\tLENG\tDEC\tSIZE\tROLL\tKEY\nFOO\t\tX\t4\t0\t8\tFOO\t\n";
        assert!(matches!(
            Schema::parse(bad, None),
            Err(Error::Schema(m)) if m.contains("unsupported type")
        ));
        let bad = BSIS_SIDECAR.replace("GJAHR\t\tN\t4 \t0 \t8", "GJAHR\t\tN\tx \t0 \t8");
        assert!(matches!(
            Schema::parse(&bad, None),
            Err(Error::Schema(m)) if m.starts_with("line 5")
        ));
        assert!(matches!(
            Schema::from_tuples([("A", "C", 1, 0, 2), ("A", "C", 1, 0, 2)], None),
            Err(Error::Schema(m)) if m.contains("duplicate")
        ));
    }

    #[test]
    fn flags_a_sidecar_whose_size_disagrees() {
        let schema = Schema::from_tuples([("A", "C", 4, 0, 9)], None).unwrap();
        assert!(schema.inconsistencies()[0].starts_with("A: SIZE=9"));
        assert!(Schema::parse(BSIS_SIDECAR, None)
            .unwrap()
            .inconsistencies()
            .is_empty());
    }
}
