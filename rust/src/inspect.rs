//! Describe an export from a small sample of it, in well under a second.
//!
//! The web page sends the first few megabytes of the file and the last few
//! hundred kilobytes (where the zip central directory lives), never the
//! whole thing. From those this reports the table, the schema and record
//! geometry, the shard count, an estimate of the record count, and the first
//! decoded rows; and if the rows do not decode, the record-size probe. The
//! CLI's `info` and `head` use the same code on the whole file.
//!
//! Everything here is tolerant of truncation: a sample ends mid-entry by
//! design, and whatever whole records it holds are used.

use std::io::{Cursor, Read};
use std::sync::Arc;

use serde::Serialize;

use crate::archive::{self, ShardFormat};
use crate::decode::{write_decimal, write_python_float, Block, Column, DecimalMode, Decoder};
use crate::error::{Error, Result};
use crate::probe::{probe, Candidate};
use crate::schema::{FieldType, Schema};
use crate::text::{decode_text_shard, TextOptions};
use crate::zip::{read_central_directory, Entries, StreamReader};

/// How much decompressed shard data to sample for the preview and probe.
const SAMPLE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct FieldReport {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub length: usize,
    pub decimals: u32,
    pub size: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaReport {
    pub name: Option<String>,
    pub fields: Vec<FieldReport>,
    pub payload_size: usize,
    pub record_size: usize,
    pub padding_size: usize,
    pub warnings: Vec<String>,
}

impl SchemaReport {
    pub fn new(schema: &Schema) -> Self {
        Self {
            name: schema.name.clone(),
            fields: schema
                .offsets()
                .map(|(f, offset)| FieldReport {
                    name: f.name.clone(),
                    kind: f.kind.as_str(),
                    length: f.length,
                    decimals: f.decimals,
                    size: f.size,
                    offset,
                })
                .collect(),
            payload_size: schema.payload_size(),
            record_size: schema.record_size(),
            padding_size: schema.padding_size(),
            warnings: schema.inconsistencies(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ShardsReport {
    pub count: usize,
    /// Bytes the data shards occupy inside the archive, as stored.
    pub stored_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorReport {
    pub message: String,
    pub record_index: Option<u64>,
    pub field: Option<String>,
}

impl From<&Error> for ErrorReport {
    fn from(error: &Error) -> Self {
        match error {
            Error::Record {
                message,
                record_index,
                field,
            } => Self {
                message: message.clone(),
                record_index: Some(*record_index),
                field: field.clone(),
            },
            other => Self {
                message: other.to_string(),
                record_index: None,
                field: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A delivered zip archive.
    Archive,
    /// A loose data file.
    Loose,
    /// Just the schema sidecar.
    Sidecar,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub kind: Kind,
    pub table: String,
    pub format: Option<ShardFormat>,
    pub schema: Option<SchemaReport>,
    pub shards: Option<ShardsReport>,
    pub first_shard: Option<String>,
    pub estimated_records: Option<u64>,
    pub estimate_is_exact: bool,
    pub preview: Option<Preview>,
    pub error: Option<ErrorReport>,
    pub probe: Vec<Candidate>,
    pub notes: Vec<String>,
}

/// What to inspect.
#[derive(Default)]
pub struct Sample<'a> {
    /// The first bytes of the file (or all of it).
    pub head: &'a [u8],
    /// The last bytes of the file, for the zip central directory.
    pub tail: Option<&'a [u8]>,
    /// The full file size, when only a head and tail are at hand.
    pub total_size: Option<u64>,
    pub schema: Option<Schema>,
    pub record_size: Option<usize>,
    pub text_encoding: Option<String>,
    pub name_hint: Option<String>,
    pub preview_rows: usize,
}

/// Read as much of an entry as is there; a truncated sample is not an error.
fn read_tolerant(reader: &mut impl Read, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while out.len() < limit {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n.min(limit - out.len())]),
        }
    }
    out
}

struct DataSample {
    name: String,
    format: ShardFormat,
    bytes: Vec<u8>,
    /// The shard's full decompressed size, if a header states it.
    size: Option<u64>,
    /// The shard's size as stored in the outer archive.
    stored: Option<u64>,
}

pub fn inspect(sample: Sample<'_>) -> Result<Report> {
    let preview_rows = if sample.preview_rows == 0 { 20 } else { sample.preview_rows };
    let mut report = Report {
        kind: Kind::Loose,
        table: archive::table_name(None, sample.name_hint.as_deref()),
        format: None,
        schema: None,
        shards: None,
        first_shard: None,
        estimated_records: None,
        estimate_is_exact: false,
        preview: None,
        error: None,
        probe: Vec::new(),
        notes: Vec::new(),
    };
    let mut schema = sample.schema.clone().map(Arc::new);
    let mut data: Option<DataSample> = None;
    let mut first_stored = None;

    if sample.head.starts_with(b"PK\x03\x04") {
        report.kind = Kind::Archive;
        let mut reader = StreamReader::new(Cursor::new(sample.head));
        let mut first_member = None;
        // A truncated head ends the walk; that is expected.
        while let Ok(Some(info)) = reader.next_entry() {
            first_member.get_or_insert_with(|| info.name.clone());
            if info.is_dir() {
                continue;
            }
            let (inner_name, bytes, size) = if archive::is_zip_member(&info.name) {
                let nested = read_tolerant(&mut reader, usize::MAX);
                let mut inner = StreamReader::new(Cursor::new(&nested[..]));
                let Ok(Some(inner_info)) = inner.next_entry() else {
                    continue;
                };
                let bytes = read_tolerant(&mut inner, SAMPLE_BYTES);
                (inner_info.name.clone(), bytes, inner_info.size)
            } else {
                let limit = if archive::classify(&info.name).is_some_and(|(i, _)| i == 0) {
                    usize::MAX
                } else {
                    SAMPLE_BYTES
                };
                (info.name.clone(), read_tolerant(&mut reader, limit), info.size)
            };
            let classified = archive::classify(&inner_name).or_else(|| archive::classify(&info.name));
            match classified {
                Some((0, _)) => {
                    if schema.is_none() {
                        let table = archive::table_name(first_member.as_deref(), sample.name_hint.as_deref());
                        schema = Some(Arc::new(Schema::parse_bytes(&bytes, Some(&table))?));
                    }
                }
                Some((_, format)) => {
                    data = Some(DataSample {
                        name: inner_name.rsplit('/').next().unwrap_or(&inner_name).to_owned(),
                        format,
                        bytes,
                        size,
                        stored: info.compressed_size.filter(|_| archive::is_zip_member(&info.name)),
                    });
                    break;
                }
                None => {}
            }
        }
        report.table = archive::table_name(first_member.as_deref(), sample.name_hint.as_deref());
        if let (Some(tail), Some(total)) = (sample.tail, sample.total_size) {
            first_stored = describe_shards(&mut report, tail, total);
        }
        if schema.is_none() {
            report.notes.push(
                "No DATA.0 schema sidecar was found at the start of the archive. \
                 Add the matching DATA.0.TXT to describe the fields."
                    .into(),
            );
        }
    } else if looks_like_sidecar(sample.head) {
        report.kind = Kind::Sidecar;
        let parsed = Schema::parse_bytes(sample.head, Some(&report.table))?;
        report.schema = Some(SchemaReport::new(&parsed));
        report.notes.push(
            "This is a schema sidecar (DATA.0.TXT). Add the matching DATA.N.BIN or the delivered \
             .zip to see and convert the records."
                .into(),
        );
        return Ok(report);
    } else {
        let format = match &schema {
            Some(s) if looks_like_text(sample.head, s) => ShardFormat::Text,
            _ => ShardFormat::Bin,
        };
        data = Some(DataSample {
            name: sample.name_hint.clone().unwrap_or_else(|| "DATA".into()),
            format,
            bytes: sample.head.to_vec(),
            size: sample.total_size.or(Some(sample.head.len() as u64)),
            stored: None,
        });
        if schema.is_none() {
            report.notes.push(
                "A loose data file carries no schema. Add its DATA.0.TXT sidecar, or drop the \
                 delivered .zip instead."
                    .into(),
            );
        }
    }

    let Some(schema) = schema else {
        return Ok(report);
    };
    report.schema = Some(SchemaReport::new(&schema));
    let Some(data) = data else {
        report
            .notes
            .push("No data shard was found in the part of the file that was read.".into());
        return Ok(report);
    };
    report.format = Some(data.format);
    report.first_shard = Some(data.name.clone());

    let record_size = sample.record_size.unwrap_or_else(|| schema.record_size());
    if report.kind == Kind::Archive && data.format == ShardFormat::Bin {
        estimate(&mut report, &data, first_stored, record_size);
    }
    if report.kind == Kind::Loose && data.format == ShardFormat::Bin {
        if let Some(size) = data.size {
            report.estimated_records = Some(size / record_size as u64);
            report.estimate_is_exact = size % record_size as u64 == 0;
            if size % record_size as u64 != 0 {
                report.notes.push(format!(
                    "The file is not a whole number of {record_size}-byte records ({} byte(s) left over).",
                    size % record_size as u64
                ));
            }
        }
    }

    match data.format {
        ShardFormat::Bin => preview_bin(&mut report, &schema, &data, record_size, preview_rows)?,
        ShardFormat::Text => preview_text(&mut report, &schema, &data, &sample, preview_rows)?,
    }
    Ok(report)
}

/// Count the data shards from the central directory in the tail. Returns
/// the stored size of the first data shard, for scaling an estimate.
fn describe_shards(report: &mut Report, tail: &[u8], total: u64) -> Option<u64> {
    let start = total.saturating_sub(tail.len() as u64);
    let entries = read_central_directory(total, tail.len(), &mut |offset, len| {
        if offset < start || offset + len as u64 > total {
            return Err(std::io::Error::other("outside the sample"));
        }
        let at = (offset - start) as usize;
        Ok(tail[at..at + len].to_vec())
    });
    let Ok(entries) = entries else {
        report
            .notes
            .push("The archive's central directory could not be read from the end of the file.".into());
        return None;
    };
    let data: Vec<_> = entries
        .iter()
        .filter(|e| !e.name.ends_with('/'))
        .filter(|e| archive::member_index(&e.name).is_some_and(|i| i > 0))
        .collect();
    report.shards = Some(ShardsReport {
        count: data.len(),
        stored_bytes: data.iter().map(|e| e.compressed_size).sum(),
    });
    data.first().map(|e| e.compressed_size)
}

/// Scale the first shard's record count by the stored size of all shards.
fn estimate(report: &mut Report, first: &DataSample, first_stored: Option<u64>, record_size: usize) {
    let (Some(shards), Some(size)) = (report.shards.as_ref(), first.size) else {
        return;
    };
    let Some(stored_first) = first.stored.or(first_stored).filter(|&s| s > 0) else {
        return;
    };
    let estimate = shards.stored_bytes as f64 * size as f64 / stored_first as f64 / record_size as f64;
    report.estimated_records = Some(estimate.round() as u64);
    report.estimate_is_exact = shards.count == 1;
}

fn preview_bin(
    report: &mut Report,
    schema: &Arc<Schema>,
    data: &DataSample,
    record_size: usize,
    rows: usize,
) -> Result<()> {
    let decoder = Decoder::new(schema.clone(), Some(record_size), DecimalMode::Exact, true)?;
    let available = data.bytes.len() / record_size;
    let take = available.min(rows);
    let (block, failure) = match decoder.decode(&data.bytes[..take * record_size], 0) {
        Ok(block) => (Some(block), None),
        Err(failure) => {
            let good = decoder
                .decode(&data.bytes[..failure.row * record_size], 0)
                .ok();
            (good, Some(failure.error))
        }
    };
    // Check further than the preview, so a misalignment that only shows up
    // after a few records is still caught here rather than mid-conversion.
    let failure = failure.or_else(|| {
        let check = available.min(2_000);
        decoder.decode(&data.bytes[..check * record_size], 0).err().map(|f| f.error)
    });
    if let Some(block) = block {
        report.preview = Some(preview(&block, rows));
    }
    if let Some(error) = failure {
        report.error = Some(ErrorReport::from(&error));
        report.probe = probe(&data.bytes, schema, data.size, 200);
    }
    Ok(())
}

fn preview_text(
    report: &mut Report,
    schema: &Arc<Schema>,
    data: &DataSample,
    sample: &Sample<'_>,
    rows: usize,
) -> Result<()> {
    let options = TextOptions::new(sample.text_encoding.as_deref(), DecimalMode::Exact, true)?;
    // Drop a partial last line.
    let end = data
        .bytes
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(data.bytes.len(), |i| i + 1);
    let mut first = None;
    let result = decode_text_shard(&data.bytes[..end], schema, &options, rows, Some(rows as u64), |block| {
        first.get_or_insert(block);
        Ok(())
    });
    if let Some(block) = first {
        report.preview = Some(preview(&block, rows));
    }
    if let Err(error) = result {
        report.error = Some(ErrorReport::from(&error));
    }
    Ok(())
}

/// Render the first rows of a block as display strings.
pub fn preview(block: &Block, rows: usize) -> Preview {
    let fields = block.schema.fields();
    Preview {
        columns: fields.iter().map(|f| f.name.clone()).collect(),
        rows: (0..block.rows.min(rows))
            .map(|row| {
                block
                    .columns
                    .iter()
                    .zip(fields)
                    .map(|(column, field)| {
                        if !column.is_valid(row) {
                            return None;
                        }
                        let mut out = String::new();
                        match column {
                            Column::Text { .. } => {
                                out.push_str(&String::from_utf8_lossy(column.text(row)))
                            }
                            Column::Decimal { values, .. } => {
                                write_decimal(&mut out, values[row], field.decimals)
                            }
                            Column::Float { values, .. } => write_python_float(&mut out, values[row]),
                        }
                        Some(out)
                    })
                    .collect()
            })
            .collect(),
    }
}

fn looks_like_sidecar(head: &[u8]) -> bool {
    let head = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let line = head.split(|&b| b == b'\n').next().unwrap_or(head);
    let upper = String::from_utf8_lossy(line).to_ascii_uppercase();
    let cells: Vec<&str> = upper.split('\t').map(str::trim).collect();
    ["NAME", "TYPE", "LENG", "SIZE"].iter().all(|c| cells.contains(c))
}

fn looks_like_text(head: &[u8], schema: &Schema) -> bool {
    let head = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let first = schema.fields()[0].name.as_bytes();
    head.starts_with(first) && head.get(first.len()) == Some(&b'\t')
}

/// True for a field type that is shown as text in previews.
pub fn is_textual(kind: FieldType) -> bool {
    kind != FieldType::P
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{encode_bsis_records, sample_archive, BSIS_SIDECAR};

    #[test]
    fn describes_an_archive_from_its_head_and_tail() {
        let archive = sample_archive(50_000, 3);
        let head = &archive[..archive.len().min(1 << 20)];
        let tail = &archive[archive.len() - 4096..];
        let report = inspect(Sample {
            head,
            tail: Some(tail),
            total_size: Some(archive.len() as u64),
            ..Sample::default()
        })
        .unwrap();
        assert_eq!(report.table, "BSIS");
        assert_eq!(report.schema.as_ref().unwrap().record_size, 126);
        assert_eq!(report.shards.as_ref().unwrap().count, 3);
        let estimate = report.estimated_records.unwrap() as f64;
        assert!((estimate - 150_000.0).abs() / 150_000.0 < 0.05, "{estimate}");
        let preview = report.preview.unwrap();
        assert_eq!(preview.rows.len(), 20);
        assert_eq!(preview.rows[0][4].as_deref(), Some("1000000000"));
        assert!(report.error.is_none());
    }

    #[test]
    fn a_wrong_record_size_comes_with_a_probe() {
        let schema = Schema::parse(BSIS_SIDECAR, None).unwrap();
        let mut data = Vec::new();
        encode_bsis_records(500, 1, &mut data);
        let report = inspect(Sample {
            head: &data,
            schema: Some(schema),
            record_size: Some(125),
            ..Sample::default()
        })
        .unwrap();
        assert!(report.error.is_some());
        assert_eq!(report.probe[0].record_size, 126);
    }

    #[test]
    fn recognises_a_lone_sidecar() {
        let report = inspect(Sample {
            head: BSIS_SIDECAR.as_bytes(),
            ..Sample::default()
        })
        .unwrap();
        assert!(matches!(report.kind, Kind::Sidecar));
        assert_eq!(report.schema.unwrap().fields.len(), 9);
    }
}
