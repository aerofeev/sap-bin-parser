//! Encoding decoded blocks as CSV, JSON Lines and Arrow, and assembling the
//! encoded chunks into an output: one file, a zip of one file per shard, or
//! a directory of files.
//!
//! CSV follows Python's `csv` module with `QUOTE_MINIMAL` exactly: a field
//! is quoted only when it contains the delimiter, a quote, `\r` or `\n`;
//! quotes are doubled; rows end in `\r\n`; a row holding a single empty
//! field is written as `""`. The Python reference writes the same bytes.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;

use arrow_array::{ArrayRef, Decimal128Array, Float64Array, RecordBatch, StringArray};
use arrow_buffer::{Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field as ArrowField, Schema as ArrowSchema, SchemaRef};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression as ParquetCompression, GzipLevel, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::decode::{write_decimal, write_python_float, Block, Column, DecimalMode};
use crate::error::Result;
use crate::schema::{FieldType, Schema};
use crate::zip::ZipWriter;

/// A line-oriented output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Csv { delimiter: u8 },
    Jsonl,
}

/// Append the header row (and BOM, if asked) for a text format.
pub fn text_header(schema: &Schema, kind: TextKind, bom: bool, out: &mut Vec<u8>) {
    if bom {
        out.extend_from_slice("\u{feff}".as_bytes());
    }
    if let TextKind::Csv { delimiter } = kind {
        let names: Vec<&str> = schema.field_names().collect();
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                out.push(delimiter);
            }
            csv_field(out, name.as_bytes(), delimiter, names.len() == 1);
        }
        out.extend_from_slice(b"\r\n");
    }
}

#[inline]
fn csv_field(out: &mut Vec<u8>, value: &[u8], delimiter: u8, only_field: bool) {
    if value.is_empty() {
        if only_field {
            out.extend_from_slice(b"\"\"");
        }
        return;
    }
    if value
        .iter()
        .any(|&b| b == delimiter || b == b'"' || b == b'\r' || b == b'\n')
    {
        out.push(b'"');
        for &b in value {
            if b == b'"' {
                out.push(b'"');
            }
            out.push(b);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(value);
    }
}

fn json_string(out: &mut Vec<u8>, value: &[u8]) {
    out.push(b'"');
    let mut start = 0;
    for (i, &b) in value.iter().enumerate() {
        let escape: Option<&[u8]> = match b {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            0..=0x1F => None,
            _ => continue,
        };
        out.extend_from_slice(&value[start..i]);
        match escape {
            Some(e) => out.extend_from_slice(e),
            None => out.extend_from_slice(format!("\\u{b:04x}").as_bytes()),
        }
        start = i + 1;
    }
    out.extend_from_slice(&value[start..]);
    out.push(b'"');
}

/// Append a block as CSV or JSON Lines. When `row_ends` is given, the end
/// offset of every row is recorded, so an output can be cut at an exact
/// row count.
pub fn encode_text(
    block: &Block,
    kind: TextKind,
    out: &mut Vec<u8>,
    mut row_ends: Option<&mut Vec<u32>>,
) {
    let fields = block.schema.fields();
    let names: Vec<Vec<u8>> = fields
        .iter()
        .map(|f| {
            let mut key = Vec::new();
            json_string(&mut key, f.name.as_bytes());
            key.push(b':');
            key
        })
        .collect();
    let mut scratch = Vec::with_capacity(48);
    let single = fields.len() == 1;
    let base = out.len();

    for row in 0..block.rows {
        match kind {
            TextKind::Csv { delimiter } => {
                for (i, column) in block.columns.iter().enumerate() {
                    if i > 0 {
                        out.push(delimiter);
                    }
                    if !column.is_valid(row) {
                        csv_field(out, b"", delimiter, single);
                        continue;
                    }
                    match column {
                        Column::Text { .. } => csv_field(out, column.text(row), delimiter, single),
                        Column::Decimal { values, .. } => {
                            scratch.clear();
                            write_decimal(&mut scratch, values[row], fields[i].decimals);
                            csv_field(out, &scratch, delimiter, single);
                        }
                        Column::Float { values, .. } => {
                            scratch.clear();
                            write_python_float(&mut scratch, values[row]);
                            csv_field(out, &scratch, delimiter, single);
                        }
                    }
                }
                out.extend_from_slice(b"\r\n");
            }
            TextKind::Jsonl => {
                out.push(b'{');
                for (i, column) in block.columns.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(&names[i]);
                    if !column.is_valid(row) {
                        out.extend_from_slice(b"null");
                        continue;
                    }
                    match column {
                        Column::Text { .. } => json_string(out, column.text(row)),
                        Column::Decimal { values, .. } => {
                            write_decimal(out, values[row], fields[i].decimals)
                        }
                        Column::Float { values, .. } => write_python_float(out, values[row]),
                    }
                }
                out.extend_from_slice(b"}\n");
            }
        }
        if let Some(ends) = row_ends.as_deref_mut() {
            ends.push((out.len() - base) as u32);
        }
    }
}

/// Arrow precision for a packed field: the digits its byte size can hold,
/// the same rule the Python writer uses.
pub fn decimal_precision(size: usize, decimals: u32) -> u8 {
    (size * 2 - 1).max(decimals as usize + 1).min(38) as u8
}

/// The Arrow schema a SAP schema maps to.
pub fn arrow_schema(schema: &Schema, mode: DecimalMode) -> SchemaRef {
    let fields: Vec<ArrowField> = schema
        .fields()
        .iter()
        .map(|f| {
            let dtype = match (f.kind, mode) {
                (FieldType::P, DecimalMode::Exact) => {
                    DataType::Decimal128(decimal_precision(f.size, f.decimals), f.decimals as i8)
                }
                (FieldType::P, DecimalMode::Float) => DataType::Float64,
                _ => DataType::Utf8,
            };
            ArrowField::new(&f.name, dtype, true)
        })
        .collect();
    Arc::new(ArrowSchema::new(fields))
}

fn nulls(valid: &[bool], count: usize) -> Option<NullBuffer> {
    (count > 0).then(|| NullBuffer::from(valid.to_vec()))
}

/// Convert a block to an Arrow record batch (consuming its buffers).
pub fn to_record_batch(block: Block, schema: &SchemaRef) -> Result<RecordBatch> {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(block.columns.len());
    for (column, field) in block.columns.into_iter().zip(schema.fields()) {
        let array: ArrayRef = match column {
            Column::Text {
                offsets,
                data,
                valid,
                nulls: count,
            } => Arc::new(StringArray::try_new(
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                Buffer::from_vec(data),
                nulls(&valid, count),
            )?),
            Column::Decimal {
                values,
                valid,
                nulls: count,
            } => {
                let DataType::Decimal128(precision, scale) = field.data_type() else {
                    unreachable!("decimal column with a non-decimal field")
                };
                Arc::new(
                    Decimal128Array::new(ScalarBuffer::from(values), nulls(&valid, count))
                        .with_precision_and_scale(*precision, *scale)?,
                )
            }
            Column::Float {
                values,
                valid,
                nulls: count,
            } => Arc::new(Float64Array::new(
                ScalarBuffer::from(values),
                nulls(&valid, count),
            )),
        };
        arrays.push(array);
    }
    Ok(RecordBatch::try_new(schema.clone(), arrays)?)
}

/// Parquet page compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    #[default]
    Zstd,
    Snappy,
    Gzip,
    None,
}

impl Compression {
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "zstd" => Some(Self::Zstd),
            "snappy" => Some(Self::Snappy),
            "gzip" => Some(Self::Gzip),
            "none" | "uncompressed" => Some(Self::None),
            _ => None,
        }
    }
}

pub fn parquet_properties(compression: Compression) -> WriterProperties {
    let codec = match compression {
        Compression::Zstd => ParquetCompression::ZSTD(ZstdLevel::default()),
        Compression::Snappy => ParquetCompression::SNAPPY,
        Compression::Gzip => ParquetCompression::GZIP(GzipLevel::default()),
        Compression::None => ParquetCompression::UNCOMPRESSED,
    };
    WriterProperties::builder()
        .set_compression(codec)
        .set_created_by(format!("sap-bin version {} (eidox ai)", crate::VERSION))
        .build()
}

/// One encoded piece of output, produced by a worker.
pub enum Encoded {
    Text {
        bytes: Vec<u8>,
        rows: usize,
        row_ends: Option<Vec<u32>>,
    },
    Batch(RecordBatch),
}

impl Encoded {
    pub fn rows(&self) -> usize {
        match self {
            Encoded::Text { rows, .. } => *rows,
            Encoded::Batch(batch) => batch.num_rows(),
        }
    }

    /// Keep only the first `rows` rows.
    pub fn truncate(self, rows: usize) -> Self {
        match self {
            Encoded::Text {
                mut bytes,
                row_ends,
                ..
            } => {
                let ends = row_ends.expect("row offsets are recorded whenever a limit is set");
                bytes.truncate(if rows == 0 {
                    0
                } else {
                    ends[rows - 1] as usize
                });
                Encoded::Text {
                    bytes,
                    rows,
                    row_ends: None,
                }
            }
            Encoded::Batch(batch) => Encoded::Batch(batch.slice(0, rows)),
        }
    }
}

/// What the ordered writer needs to know about a format.
#[derive(Debug, Clone)]
pub struct OutputFormat {
    pub text: Option<TextKind>,
    pub bom: bool,
    pub compression: Compression,
    pub arrow: SchemaRef,
    /// How record batches are stored, when the output is not text.
    pub columnar: Columnar,
    pub extension: &'static str,
}

/// Columnar output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Columnar {
    Parquet,
    /// The Arrow IPC stream format, which pandas, Polars and DuckDB read as
    /// it is.
    Arrow,
}

/// Writes record batches in a columnar format.
enum Columns<W: Write + Send> {
    Parquet(Box<ArrowWriter<W>>),
    Arrow(Box<StreamWriter<W>>),
}

impl<W: Write + Send> Columns<W> {
    fn new(out: W, format: &OutputFormat) -> Result<Self> {
        Ok(match format.columnar {
            Columnar::Parquet => Columns::Parquet(Box::new(ArrowWriter::try_new(
                out,
                format.arrow.clone(),
                Some(parquet_properties(format.compression)),
            )?)),
            Columnar::Arrow => Columns::Arrow(Box::new(StreamWriter::try_new(out, &format.arrow)?)),
        })
    }

    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        match self {
            Columns::Parquet(writer) => writer.write(batch)?,
            Columns::Arrow(writer) => writer.write(batch)?,
        }
        Ok(())
    }

    /// Write the footer (or end-of-stream marker) and return the output.
    fn into_inner(self) -> Result<W> {
        Ok(match self {
            Columns::Parquet(writer) => writer.into_inner()?,
            Columns::Arrow(writer) => writer.into_inner()?,
        })
    }
}

/// Where encoded output goes.
pub trait Sink {
    fn begin_shard(&mut self, name: &str) -> Result<()>;
    fn write(&mut self, chunk: Encoded) -> Result<()>;
    fn end_shard(&mut self) -> Result<()>;
    /// Finalise the output (footer, central directory, flush).
    fn finish(self: Box<Self>) -> Result<()>;
}

/// A single output file: CSV, JSON Lines, Parquet or Arrow.
pub struct MergedSink<W: Write + Send> {
    state: Merged<W>,
}

enum Merged<W: Write + Send> {
    Text(W),
    Columns(Columns<W>),
}

impl<W: Write + Send> MergedSink<W> {
    pub fn new(mut out: W, schema: &Schema, format: &OutputFormat) -> Result<Self> {
        let state = match format.text {
            Some(kind) => {
                let mut header = Vec::new();
                text_header(schema, kind, format.bom, &mut header);
                out.write_all(&header)?;
                Merged::Text(out)
            }
            None => Merged::Columns(Columns::new(out, format)?),
        };
        Ok(Self { state })
    }
}

impl<W: Write + Send> Sink for MergedSink<W> {
    fn begin_shard(&mut self, _: &str) -> Result<()> {
        Ok(())
    }

    fn write(&mut self, chunk: Encoded) -> Result<()> {
        match (&mut self.state, chunk) {
            (Merged::Text(out), Encoded::Text { bytes, .. }) => out.write_all(&bytes)?,
            (Merged::Columns(writer), Encoded::Batch(batch)) => writer.write(&batch)?,
            _ => unreachable!("chunk kind does not match the output format"),
        }
        Ok(())
    }

    fn end_shard(&mut self) -> Result<()> {
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        match self.state {
            Merged::Text(mut out) => out.flush()?,
            Merged::Columns(writer) => writer.into_inner()?.flush()?,
        }
        Ok(())
    }
}

/// One file per shard inside a zip archive, streamed.
pub struct ZipSink<W: Write + Send> {
    state: Option<Zipped<W>>,
    schema: Schema,
    format: OutputFormat,
}

enum Zipped<W: Write + Send> {
    Idle(ZipWriter<W>),
    Text(ZipWriter<W>),
    Columns(Columns<ZipWriter<W>>),
}

impl<W: Write + Send> ZipSink<W> {
    pub fn new(out: W, schema: &Schema, format: &OutputFormat) -> Self {
        Self {
            state: Some(Zipped::Idle(ZipWriter::new(out))),
            schema: schema.clone(),
            format: format.clone(),
        }
    }
}

impl<W: Write + Send> Sink for ZipSink<W> {
    fn begin_shard(&mut self, name: &str) -> Result<()> {
        let Some(Zipped::Idle(mut zip)) = self.state.take() else {
            unreachable!("shard begun while another is open")
        };
        zip.start_entry(&format!("{}.{}", shard_stem(name), self.format.extension))?;
        self.state = Some(match self.format.text {
            Some(kind) => {
                let mut header = Vec::new();
                text_header(&self.schema, kind, self.format.bom, &mut header);
                zip.write_all(&header)?;
                Zipped::Text(zip)
            }
            None => Zipped::Columns(Columns::new(zip, &self.format)?),
        });
        Ok(())
    }

    fn write(&mut self, chunk: Encoded) -> Result<()> {
        match (self.state.as_mut(), chunk) {
            (Some(Zipped::Text(zip)), Encoded::Text { bytes, .. }) => zip.write_all(&bytes)?,
            (Some(Zipped::Columns(writer)), Encoded::Batch(batch)) => writer.write(&batch)?,
            _ => unreachable!("chunk written outside a shard"),
        }
        Ok(())
    }

    fn end_shard(&mut self) -> Result<()> {
        let mut zip = match self.state.take() {
            Some(Zipped::Text(zip)) => zip,
            Some(Zipped::Columns(writer)) => writer.into_inner()?,
            _ => unreachable!("no shard open"),
        };
        zip.finish_entry()?;
        self.state = Some(Zipped::Idle(zip));
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        if let Some(Zipped::Idle(zip)) = self.state.take() {
            zip.finish()?.flush()?;
        }
        Ok(())
    }
}

/// One file per shard in a directory (the CLI's `--split`).
pub struct DirSink {
    dir: PathBuf,
    schema: Schema,
    format: OutputFormat,
    current: Option<Merged<BufWriter<File>>>,
}

impl DirSink {
    pub fn new(dir: PathBuf, schema: &Schema, format: &OutputFormat) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            schema: schema.clone(),
            format: format.clone(),
            current: None,
        })
    }

    /// The path a shard is written to.
    pub fn path_for(&self, shard: &str) -> PathBuf {
        self.dir
            .join(format!("{}.{}", shard_stem(shard), self.format.extension))
    }
}

impl Sink for DirSink {
    fn begin_shard(&mut self, name: &str) -> Result<()> {
        let file = BufWriter::with_capacity(1 << 20, File::create(self.path_for(name))?);
        self.current = Some(MergedSink::new(file, &self.schema, &self.format)?.state);
        Ok(())
    }

    fn write(&mut self, chunk: Encoded) -> Result<()> {
        match (self.current.as_mut(), chunk) {
            (Some(Merged::Text(out)), Encoded::Text { bytes, .. }) => out.write_all(&bytes)?,
            (Some(Merged::Columns(writer)), Encoded::Batch(batch)) => writer.write(&batch)?,
            _ => unreachable!("chunk written outside a shard"),
        }
        Ok(())
    }

    fn end_shard(&mut self) -> Result<()> {
        if let Some(state) = self.current.take() {
            Box::new(MergedSink { state }).finish()?;
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

/// `DATA.1.BIN` -> `DATA.1`, as Python's `Path(name).stem`.
pub fn shard_stem(name: &str) -> &str {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(0) | None => base,
        Some(i) => &base[..i],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Decoder;
    use crate::sample::{bsis_rows, encode_file, BSIS_SIDECAR};

    fn block(mode: DecimalMode) -> Block {
        let schema = Arc::new(Schema::parse(BSIS_SIDECAR, Some("BSIS")).unwrap());
        let data = encode_file(&schema, &bsis_rows());
        Decoder::new(schema, None, mode, true)
            .unwrap()
            .decode(&data, 0)
            .unwrap()
    }

    #[test]
    fn csv_matches_python_byte_for_byte() {
        let block = block(DecimalMode::Exact);
        let mut out = Vec::new();
        text_header(
            &block.schema,
            TextKind::Csv { delimiter: b',' },
            false,
            &mut out,
        );
        encode_text(&block, TextKind::Csv { delimiter: b',' }, &mut out, None);
        let expected = "BUKRS,HKONT,ZUONR,GJAHR,BELNR,BUZEI,BUDAT,BLART,DMBTR\r\n\
            0100,0000123456,20250616,2025,1000000001,001,2025-06-16,PR,0.50\r\n\
            0100,0000123456,,2025,1000000002,003,2025-06-16,PR,90.90\r\n\
            0100,0000654321,REVERSAL,2025,1000000003,002,,KR,-1234.56\r\n";
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn csv_quotes_minimally() {
        let mut out = Vec::new();
        csv_field(&mut out, b"a,b\nc", b',', false);
        csv_field(&mut out, b"say \"hi\"", b',', false);
        csv_field(&mut out, b"plain", b',', false);
        assert_eq!(out, b"\"a,b\nc\"\"say \"\"hi\"\"\"plain");
        let mut out = Vec::new();
        csv_field(&mut out, b"", b',', true);
        assert_eq!(out, b"\"\"");
    }

    #[test]
    fn jsonl_rows() {
        let block = block(DecimalMode::Float);
        let mut out = Vec::new();
        let mut ends = Vec::new();
        encode_text(&block, TextKind::Jsonl, &mut out, Some(&mut ends));
        let text = String::from_utf8(out).unwrap();
        let first = text.lines().next().unwrap();
        assert!(first.starts_with("{\"BUKRS\":\"0100\""));
        assert!(first.ends_with("\"DMBTR\":0.5}"));
        assert!(text.lines().nth(2).unwrap().contains("\"BUDAT\":null"));
        assert_eq!(ends.len(), 3);
    }

    #[test]
    fn arrow_batches_keep_exact_decimals() {
        let block = block(DecimalMode::Exact);
        let schema = arrow_schema(&block.schema, DecimalMode::Exact);
        assert_eq!(schema.field(8).data_type(), &DataType::Decimal128(13, 2));
        let batch = to_record_batch(block, &schema).unwrap();
        let amounts = batch
            .column(8)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(amounts.value_as_string(2), "-1234.56");
        assert_eq!(batch.column(6).null_count(), 1);
    }

    #[test]
    fn stems() {
        assert_eq!(shard_stem("DATA.1.BIN"), "DATA.1");
        assert_eq!(shard_stem("x/DATA.12.TXT"), "DATA.12");
    }
}
