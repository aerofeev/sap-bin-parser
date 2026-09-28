//! Field decoding, one block of records at a time.
//!
//! Two encodings appear in these files:
//!
//! * Character-ish fields (`C`, `N`, `D`, `T`) are UTF-16 big-endian, two
//!   bytes per character, padded on the right.
//! * Numeric fields (`P`) are packed decimal (COMP-3 / BCD): each byte
//!   carries two decimal digits, except the final byte, which carries one
//!   digit plus a sign nibble.
//!
//! A block is decoded column by column straight out of the byte buffer into
//! a [`Block`]: UTF-8 bytes plus offsets for text, scaled integers for
//! decimals. No per-value allocation happens anywhere on this path, which is
//! what makes the engine fast; the same `Block` then feeds the CSV, JSON
//! Lines and Parquet encoders.
//!
//! Semantics match the Python reference implementation exactly, including
//! error wording, so the two can be tested against each other.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::error::{DecodeError, Error, Result};
use crate::schema::{Field, FieldType, Schema};

/// Largest packed field the engine accepts: 19 bytes hold 37 digits, which
/// still fits an `i128` and Arrow's 38-digit `decimal128`. SAP's own maximum
/// is 16 bytes.
pub const MAX_PACKED_SIZE: usize = 19;

/// How packed decimals are represented in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecimalMode {
    /// Exact: scaled integers, written positionally (`0.50`) or as Arrow
    /// `decimal128`.
    #[default]
    Exact,
    /// IEEE float64, accepting the rounding.
    Float,
}

/// One decoded column of a block.
#[derive(Debug, Clone)]
pub enum Column {
    /// UTF-8 text: value `i` is `data[offsets[i]..offsets[i + 1]]`.
    Text {
        offsets: Vec<i32>,
        data: Vec<u8>,
        valid: Vec<bool>,
        nulls: usize,
    },
    /// Packed decimal as an unscaled integer; the scale is the field's `DEC`.
    Decimal {
        values: Vec<i128>,
        valid: Vec<bool>,
        nulls: usize,
    },
    /// Packed decimal converted to float64.
    Float {
        values: Vec<f64>,
        valid: Vec<bool>,
        nulls: usize,
    },
}

impl Column {
    fn for_field(field: &Field, mode: DecimalMode, capacity: usize) -> Self {
        match (field.kind, mode) {
            (FieldType::P, DecimalMode::Exact) => Column::Decimal {
                values: Vec::with_capacity(capacity),
                valid: Vec::with_capacity(capacity),
                nulls: 0,
            },
            (FieldType::P, DecimalMode::Float) => Column::Float {
                values: Vec::with_capacity(capacity),
                valid: Vec::with_capacity(capacity),
                nulls: 0,
            },
            _ => {
                let mut offsets = Vec::with_capacity(capacity + 1);
                offsets.push(0);
                Column::Text {
                    offsets,
                    data: Vec::with_capacity(capacity * field.length.max(1)),
                    valid: Vec::with_capacity(capacity),
                    nulls: 0,
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Column::Text { valid, .. } | Column::Decimal { valid, .. } | Column::Float { valid, .. } => {
                valid.len()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn null_count(&self) -> usize {
        match self {
            Column::Text { nulls, .. } | Column::Decimal { nulls, .. } | Column::Float { nulls, .. } => {
                *nulls
            }
        }
    }

    pub fn is_valid(&self, row: usize) -> bool {
        match self {
            Column::Text { valid, .. } | Column::Decimal { valid, .. } | Column::Float { valid, .. } => {
                valid[row]
            }
        }
    }

    /// The UTF-8 bytes of text value `row`.
    #[inline]
    pub fn text(&self, row: usize) -> &[u8] {
        match self {
            Column::Text { offsets, data, .. } => {
                &data[offsets[row] as usize..offsets[row + 1] as usize]
            }
            _ => &[],
        }
    }

    pub(crate) fn push_null(&mut self) {
        match self {
            Column::Text {
                offsets,
                data,
                valid,
                nulls,
            } => {
                offsets.push(data.len() as i32);
                valid.push(false);
                *nulls += 1;
            }
            Column::Decimal { values, valid, nulls } => {
                values.push(0);
                valid.push(false);
                *nulls += 1;
            }
            Column::Float { values, valid, nulls } => {
                values.push(0.0);
                valid.push(false);
                *nulls += 1;
            }
        }
    }

    /// Push a text value. For `D`/`T` fields the value is normalised the same
    /// way the binary decoder does it (ISO formatting, null dates).
    pub(crate) fn push_str(&mut self, kind: FieldType, text: &str) {
        let Column::Text {
            offsets,
            data,
            valid,
            nulls,
        } = self
        else {
            unreachable!("push_str on a numeric column");
        };
        let start = data.len();
        data.extend_from_slice(text.as_bytes());
        if finish_text(kind, data, start) {
            offsets.push(data.len() as i32);
            valid.push(true);
        } else {
            data.truncate(start);
            offsets.push(start as i32);
            valid.push(false);
            *nulls += 1;
        }
    }

    pub(crate) fn push_unscaled(&mut self, unscaled: i128, scale: u32) {
        match self {
            Column::Decimal { values, valid, .. } => {
                values.push(unscaled);
                valid.push(true);
            }
            Column::Float { values, valid, .. } => {
                values.push(unscaled_to_f64(unscaled, scale));
                valid.push(true);
            }
            Column::Text { .. } => unreachable!("push_unscaled on a text column"),
        }
    }
}

/// A block of decoded records, column-major, in schema order.
#[derive(Debug, Clone)]
pub struct Block {
    pub schema: Arc<Schema>,
    pub columns: Vec<Column>,
    pub rows: usize,
    /// Values that failed to decode and were left null (lenient mode only).
    pub failed: usize,
}

impl Block {
    pub fn new(schema: Arc<Schema>, mode: DecimalMode, capacity: usize) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|f| Column::for_field(f, mode, capacity))
            .collect();
        Self {
            schema,
            columns,
            rows: 0,
            failed: 0,
        }
    }

    pub fn null_count(&self) -> usize {
        self.columns.iter().map(Column::null_count).sum()
    }
}

/// Everything the block decoder needs, precomputed once per conversion.
#[derive(Debug, Clone)]
pub struct Decoder {
    schema: Arc<Schema>,
    record_size: usize,
    mode: DecimalMode,
    strict: bool,
}

/// A decode failure inside a block: which record (relative to the block)
/// and the fully worded error.
#[derive(Debug)]
pub struct BlockError {
    pub row: usize,
    pub error: Error,
}

impl Decoder {
    pub fn new(
        schema: Arc<Schema>,
        record_size: Option<usize>,
        mode: DecimalMode,
        strict: bool,
    ) -> Result<Self> {
        let record_size = record_size.unwrap_or_else(|| schema.record_size());
        if record_size < schema.payload_size() {
            return Err(Error::Schema(format!(
                "record_size {record_size} is smaller than the schema payload ({} bytes across {} fields)",
                schema.payload_size(),
                schema.len()
            )));
        }
        if let Some(field) = schema
            .fields()
            .iter()
            .find(|f| f.kind == FieldType::P && f.size > MAX_PACKED_SIZE)
        {
            return Err(Error::Schema(format!(
                "packed field {} is {} bytes; the largest supported is {MAX_PACKED_SIZE} bytes (37 digits)",
                field.name, field.size
            )));
        }
        Ok(Self {
            schema,
            record_size,
            mode,
            strict,
        })
    }

    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    pub fn record_size(&self) -> usize {
        self.record_size
    }

    pub fn mode(&self) -> DecimalMode {
        self.mode
    }

    pub fn strict(&self) -> bool {
        self.strict
    }

    /// Decode `data`, which must hold a whole number of records.
    ///
    /// `first_record` is the index of the block's first record within its
    /// shard, used in error messages. In strict mode the first failing
    /// record (and within it the first failing field) is reported, exactly as
    /// a row-at-a-time reader would report it; in lenient mode failing
    /// fields become nulls.
    pub fn decode(&self, data: &[u8], first_record: u64) -> std::result::Result<Block, BlockError> {
        debug_assert_eq!(data.len() % self.record_size, 0);
        let rows = data.len() / self.record_size;
        let mut block = Block::new(self.schema.clone(), self.mode, rows);
        block.rows = rows;

        // In strict mode, stop each later field at the earliest failure seen
        // so far: the reported error is then the lexicographic minimum of
        // (record, field), without decoding anything past it.
        let mut failure: Option<(usize, usize, DecodeError)> = None;

        for (index, ((field, offset), column)) in self
            .schema
            .offsets()
            .zip(block.columns.iter_mut())
            .enumerate()
        {
            let limit = failure.as_ref().map_or(rows, |(row, _, _)| *row);
            let outcome = match field.kind {
                FieldType::P => decode_packed_column(
                    column,
                    data,
                    self.record_size,
                    offset,
                    field,
                    limit,
                    self.strict,
                ),
                kind => decode_text_column(
                    column,
                    data,
                    self.record_size,
                    offset,
                    field.size,
                    kind,
                    limit,
                    self.strict,
                ),
            };
            match outcome {
                Ok(failed) => block.failed += failed,
                Err((row, error)) => failure = Some((row, index, error)),
            }
        }

        match failure {
            None => Ok(block),
            Some((row, index, error)) => {
                let (field, offset) = self.schema.offsets().nth(index).unwrap();
                let record_index = first_record + row as u64;
                Err(BlockError {
                    row,
                    error: Error::record(
                        format!(
                            "record {record_index}, field {} (offset {offset}, {}{}): {error}",
                            field.name,
                            field.kind.as_str(),
                            field.length
                        ),
                        record_index,
                        Some(&field.name),
                    ),
                })
            }
        }
    }
}

/// The number of values nulled after a decode failure, or the first failure.
type ColumnResult = std::result::Result<usize, (usize, DecodeError)>;

#[allow(clippy::too_many_arguments)]
fn decode_text_column(
    column: &mut Column,
    data: &[u8],
    record_size: usize,
    offset: usize,
    size: usize,
    kind: FieldType,
    limit: usize,
    strict: bool,
) -> ColumnResult {
    let Column::Text {
        offsets,
        data: out,
        valid,
        nulls,
    } = column
    else {
        unreachable!()
    };
    let mut failed = 0;
    for row in 0..limit {
        let start = row * record_size + offset;
        let raw = &data[start..start + size];
        let before = out.len();
        match utf16be_to_utf8(raw, out) {
            Ok(()) if finish_text(kind, out, before) => {
                offsets.push(out.len() as i32);
                valid.push(true);
            }
            Ok(()) => {
                out.truncate(before);
                offsets.push(before as i32);
                valid.push(false);
                *nulls += 1;
            }
            Err(error) => {
                out.truncate(before);
                if strict {
                    return Err((row, error));
                }
                offsets.push(before as i32);
                valid.push(false);
                *nulls += 1;
                failed += 1;
            }
        }
    }
    Ok(failed)
}

fn decode_packed_column(
    column: &mut Column,
    data: &[u8],
    record_size: usize,
    offset: usize,
    field: &Field,
    limit: usize,
    strict: bool,
) -> ColumnResult {
    let mut failed = 0;
    for row in 0..limit {
        let start = row * record_size + offset;
        match unpack_packed_decimal(&data[start..start + field.size]) {
            Ok(value) => column.push_unscaled(value, field.decimals),
            Err(error) if strict => return Err((row, error)),
            Err(_) => {
                column.push_null();
                failed += 1;
            }
        }
    }
    Ok(failed)
}

/// Is this UTF-16 code unit padding? NUL, `\t`..`\r`, and space: the same
/// explicit set as `sap_bin_parser.decode.STRIP_CHARS` in Python.
#[inline(always)]
fn is_pad(unit: u16) -> bool {
    unit == 0x20 || unit == 0 || (0x09..=0x0D).contains(&unit)
}

/// Decode a UTF-16BE field, strip its padding, and append the UTF-8 result.
pub fn utf16be_to_utf8(raw: &[u8], out: &mut Vec<u8>) -> std::result::Result<(), DecodeError> {
    if raw.len() % 2 != 0 {
        return Err(utf16_error(raw));
    }
    let unit = |i: usize| u16::from_be_bytes([raw[2 * i], raw[2 * i + 1]]);
    let count = raw.len() / 2;
    let mut end = count;
    while end > 0 && is_pad(unit(end - 1)) {
        end -= 1;
    }
    let mut start = 0;
    while start < end && is_pad(unit(start)) {
        start += 1;
    }

    // Fast path: plain ASCII, which is nearly every SAP field.
    let mut i = start;
    while i < end {
        let (high, low) = (raw[2 * i], raw[2 * i + 1]);
        if high != 0 || low >= 0x80 {
            break;
        }
        out.push(low);
        i += 1;
    }

    while i < end {
        let u = unit(i);
        let code = if (0xD800..0xDC00).contains(&u) {
            let next = if i + 1 < end { unit(i + 1) } else { 0 };
            if !(0xDC00..0xE000).contains(&next) {
                return Err(utf16_error(raw));
            }
            i += 1;
            0x10000 + (((u as u32) - 0xD800) << 10) + ((next as u32) - 0xDC00)
        } else if (0xDC00..0xE000).contains(&u) {
            return Err(utf16_error(raw));
        } else {
            u as u32
        };
        // Surrogates were handled above, so this is always a valid scalar.
        let ch = char::from_u32(code).expect("valid scalar value");
        let mut buffer = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
        i += 1;
    }
    Ok(())
}

fn utf16_error(raw: &[u8]) -> DecodeError {
    DecodeError(format!(
        "cannot decode {} as UTF-16BE (usually means the record size is wrong)",
        hex(raw)
    ))
}

/// Normalise a just-appended text value in place. Returns false when the
/// value is null (a blank or all-zero date, a blank time).
#[inline]
fn finish_text(kind: FieldType, out: &mut Vec<u8>, start: usize) -> bool {
    match kind {
        FieldType::D => {
            let value = &out[start..];
            if value.is_empty() || value.iter().all(|&b| b == b'0') || value == b"0000-00-00" {
                return false;
            }
            if value.len() == 8 && value.iter().all(u8::is_ascii_digit) {
                let digits: [u8; 8] = value.try_into().unwrap();
                out.truncate(start);
                out.extend_from_slice(&digits[0..4]);
                out.push(b'-');
                out.extend_from_slice(&digits[4..6]);
                out.push(b'-');
                out.extend_from_slice(&digits[6..8]);
            }
            true
        }
        FieldType::T => {
            let value = &out[start..];
            if value.is_empty() {
                return false;
            }
            if value.len() == 6 && value.iter().all(u8::is_ascii_digit) {
                let digits: [u8; 6] = value.try_into().unwrap();
                out.truncate(start);
                out.extend_from_slice(&digits[0..2]);
                out.push(b':');
                out.extend_from_slice(&digits[2..4]);
                out.push(b':');
                out.extend_from_slice(&digits[4..6]);
            }
            true
        }
        _ => true,
    }
}

/// Decode a packed-decimal (COMP-3/BCD) field to its unscaled value.
///
/// A field of all zero bytes is SAP's null and decodes as zero. A negatively
/// signed zero is zero. The scale (the schema's `DEC`) is applied by the
/// caller; the digits themselves carry no decimal point.
pub fn unpack_packed_decimal(raw: &[u8]) -> std::result::Result<i128, DecodeError> {
    let Some((&last, body)) = raw.split_last() else {
        return Err(DecodeError("packed decimal field is empty".into()));
    };
    if last == 0 && body.iter().all(|&b| b == 0) {
        return Ok(0);
    }
    let sign = last & 0x0F;
    if sign < 0x0A {
        return Err(DecodeError(format!(
            "invalid packed-decimal sign nibble 0x{sign:X} in {} (usually means the record size is wrong)",
            hex(raw)
        )));
    }
    let magnitude = if raw.len() <= 10 {
        // Up to 19 digits fit a u64, which is markedly faster than u128.
        let mut value: u64 = 0;
        for &byte in body {
            let (high, low) = (byte >> 4, byte & 0x0F);
            if high > 9 || low > 9 {
                return Err(nibble_error(raw));
            }
            value = value * 100 + (high * 10 + low) as u64;
        }
        let high = last >> 4;
        if high > 9 {
            return Err(nibble_error(raw));
        }
        (value * 10 + high as u64) as i128
    } else {
        let mut value: u128 = 0;
        for &byte in body {
            let (high, low) = (byte >> 4, byte & 0x0F);
            if high > 9 || low > 9 {
                return Err(nibble_error(raw));
            }
            value = value * 100 + (high * 10 + low) as u128;
        }
        let high = last >> 4;
        if high > 9 {
            return Err(nibble_error(raw));
        }
        (value * 10 + high as u128) as i128
    };
    Ok(if sign == 0x0B || sign == 0x0D {
        -magnitude
    } else {
        magnitude
    })
}

fn nibble_error(raw: &[u8]) -> DecodeError {
    DecodeError(format!(
        "non-decimal nibble in packed field {} (usually means the record size is wrong)",
        hex(raw)
    ))
}

/// Encode a scaled integer as a packed-decimal field of `size` bytes: the
/// inverse of [`unpack_packed_decimal`]. Used to build synthetic exports.
pub fn pack_decimal(unscaled: i128, size: usize) -> std::result::Result<Vec<u8>, String> {
    let capacity = size * 2 - 1;
    let digits = unscaled.unsigned_abs().to_string();
    if digits.len() > capacity {
        return Err(format!(
            "{unscaled} needs {} digits but a {size}-byte field holds {capacity}",
            digits.len()
        ));
    }
    let mut nibbles: Vec<u8> = std::iter::repeat_n(0u8, capacity - digits.len())
        .chain(digits.bytes().map(|b| b - b'0'))
        .collect();
    nibbles.push(if unscaled < 0 { 0x0D } else { 0x0C });
    Ok(nibbles.chunks(2).map(|pair| (pair[0] << 4) | pair[1]).collect())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Convert an unscaled decimal to the nearest float64, correctly rounded
/// (the same value Python's `float(Decimal(...))` gives).
pub fn unscaled_to_f64(unscaled: i128, scale: u32) -> f64 {
    const EXACT: i128 = 1 << 53;
    if unscaled.abs() <= EXACT && scale <= 22 {
        // Both operands are exact, so one IEEE division is correctly rounded.
        return unscaled as f64 / 10f64.powi(scale as i32);
    }
    let mut text = String::with_capacity(48);
    write_decimal(&mut text, unscaled, scale);
    text.parse().expect("decimal text parses as f64")
}

/// Append a scaled integer in positional notation: `-1234.56`, `0.05`,
/// `0.00`, `123`. This matches Python's `format(Decimal, "f")`.
pub fn write_decimal(out: &mut impl DecimalSink, unscaled: i128, scale: u32) {
    let mut digits = [0u8; 40];
    let mut n = unscaled.unsigned_abs();
    let mut len = 0;
    loop {
        digits[39 - len] = b'0' + (n % 10) as u8;
        n /= 10;
        len += 1;
        if n == 0 {
            break;
        }
    }
    let digits = &digits[40 - len..];
    if unscaled < 0 {
        out.put(b"-");
    }
    let scale = scale as usize;
    if scale == 0 {
        out.put(digits);
    } else if len > scale {
        out.put(&digits[..len - scale]);
        out.put(b".");
        out.put(&digits[len - scale..]);
    } else {
        out.put(b"0.");
        for _ in 0..scale - len {
            out.put(b"0");
        }
        out.put(digits);
    }
}

/// Append a float the way Python's `repr(float)` writes it: shortest
/// round-trip digits, `.0` on integral values, and exponent notation
/// (`1e+16`, `1e-05`) outside `[1e-4, 1e16)`.
pub fn write_python_float(out: &mut impl DecimalSink, value: f64) {
    // Rust's Debug formatting is shortest round-trip, uses the same exponent
    // thresholds, and appends ".0" to integral values; only the exponent's
    // spelling differs.
    let text = format!("{value:?}");
    match text.split_once('e') {
        None => out.put(text.as_bytes()),
        Some((mantissa, exponent)) => {
            let exponent: i32 = exponent.parse().expect("float exponent");
            let mantissa = mantissa.strip_suffix(".0").unwrap_or(mantissa);
            out.put(mantissa.as_bytes());
            let sign = if exponent < 0 { '-' } else { '+' };
            out.put(format!("e{sign}{:02}", exponent.abs()).as_bytes());
        }
    }
}

/// Something decimal text can be appended to: a byte buffer or a `String`.
pub trait DecimalSink {
    fn put(&mut self, bytes: &[u8]);
}

impl DecimalSink for Vec<u8> {
    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

impl DecimalSink for String {
    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        self.push_str(std::str::from_utf8(bytes).expect("ASCII"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{encode_file, SampleRow, BSIS_SIDECAR};

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    fn decimal(raw: &str, scale: u32) -> String {
        let mut out = String::new();
        write_decimal(&mut out, unpack_packed_decimal(&unhex(raw)).unwrap(), scale);
        out
    }

    #[test]
    fn decodes_known_packed_values() {
        assert_eq!(decimal("0000000000050C", 2), "0.50");
        assert_eq!(decimal("0000000009090C", 2), "90.90");
        assert_eq!(decimal("000C", 0), "0");
        assert_eq!(decimal("123C", 0), "123");
        assert_eq!(decimal("123D", 0), "-123");
        assert_eq!(decimal("123B", 0), "-123");
        assert_eq!(decimal("123A", 0), "123");
        assert_eq!(decimal("123E", 0), "123");
        assert_eq!(decimal("123F", 0), "123");
        assert_eq!(decimal("12345C", 3), "12.345");
        assert_eq!(decimal("00005C", 2), "0.05");
    }

    #[test]
    fn nulls_and_negative_zero_print_as_scaled_zero() {
        assert_eq!(decimal("00000000000000", 2), "0.00");
        assert_eq!(decimal("000D", 2), "0.00");
        assert_eq!(decimal("000000", 0), "0");
    }

    #[test]
    fn wide_fields_keep_every_digit() {
        assert_eq!(
            decimal("1234567890123456789012345678901C", 4),
            "123456789012345678901234567.8901"
        );
    }

    #[test]
    fn rejects_bad_packed_fields_with_python_wording() {
        let err = unpack_packed_decimal(&unhex("1231")).unwrap_err().0;
        assert_eq!(
            err,
            "invalid packed-decimal sign nibble 0x1 in 1231 (usually means the record size is wrong)"
        );
        let err = unpack_packed_decimal(&unhex("1A2C")).unwrap_err().0;
        assert!(err.starts_with("non-decimal nibble in packed field 1a2c"));
        assert!(unpack_packed_decimal(&[]).is_err());
    }

    #[test]
    fn pack_round_trips() {
        for value in [0i128, 1, -1, 50, -123456, 9_999_999, -1] {
            let packed = pack_decimal(value, 7).unwrap();
            assert_eq!(unpack_packed_decimal(&packed).unwrap(), value);
        }
        assert!(pack_decimal(10i128.pow(20), 3).is_err());
    }

    fn text(raw: &str) -> std::result::Result<String, DecodeError> {
        let bytes: Vec<u8> = raw.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let mut out = Vec::new();
        utf16be_to_utf8(&bytes, &mut out).map(|()| String::from_utf8(out).unwrap())
    }

    #[test]
    fn strips_only_the_documented_padding_set() {
        assert_eq!(text("PR        ").unwrap(), "PR");
        assert_eq!(text("AB \0\t").unwrap(), "AB");
        assert_eq!(text("\u{a0}AB\u{a0}").unwrap(), "\u{a0}AB\u{a0}");
        assert_eq!(text("A\0B").unwrap(), "A\0B");
        assert_eq!(text("Пример").unwrap(), "Пример");
        assert_eq!(text("a\u{1F600}").unwrap(), "a\u{1F600}");
        assert_eq!(text("          ").unwrap(), "");
    }

    #[test]
    fn surrogate_and_odd_length_errors() {
        let mut out = Vec::new();
        assert!(utf16be_to_utf8(&[0xD8, 0x00, 0x00, 0x41], &mut out).is_err());
        assert!(utf16be_to_utf8(&[0xDC, 0x00], &mut out).is_err());
        let err = utf16be_to_utf8(&[0x00], &mut out).unwrap_err().0;
        assert!(err.contains("UTF-16BE"));
    }

    #[test]
    fn dates_and_times_normalise_like_python() {
        let mut column = Column::for_field(
            &Field::new("D", "D", 8, 0, 16).unwrap(),
            DecimalMode::Exact,
            4,
        );
        for value in ["20250616", "00000000", "", "2025-06", "0000-00-00", "0000"] {
            column.push_str(FieldType::D, value);
        }
        let got: Vec<Option<&[u8]>> = (0..6)
            .map(|i| column.is_valid(i).then(|| column.text(i)))
            .collect();
        assert_eq!(
            got,
            vec![
                Some(&b"2025-06-16"[..]),
                None,
                None,
                Some(&b"2025-06"[..]),
                None,
                None
            ]
        );

        let mut column = Column::for_field(
            &Field::new("T", "T", 6, 0, 12).unwrap(),
            DecimalMode::Exact,
            4,
        );
        for value in ["143005", "000000", ""] {
            column.push_str(FieldType::T, value);
        }
        assert_eq!(column.text(0), b"14:30:05");
        assert_eq!(column.text(1), b"00:00:00");
        assert!(!column.is_valid(2));
    }

    #[test]
    fn python_float_repr() {
        let cases = [
            (0.5, "0.5"),
            (0.0, "0.0"),
            (-1234.56, "-1234.56"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1.5e16, "1.5e+16"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123.0, "123.0"),
        ];
        for (value, expected) in cases {
            let mut out = String::new();
            write_python_float(&mut out, value);
            assert_eq!(out, expected, "{value}");
        }
        assert_eq!(unscaled_to_f64(7, 2), 0.07);
        assert_eq!(unscaled_to_f64(-123456, 2), -1234.56);
    }

    #[test]
    fn decodes_a_block_and_reports_the_first_failure() {
        let schema = Arc::new(Schema::parse(BSIS_SIDECAR, Some("BSIS")).unwrap());
        let rows = crate::sample::bsis_rows();
        let data = encode_file(&schema, &rows);
        let decoder = Decoder::new(schema.clone(), None, DecimalMode::Exact, true).unwrap();
        let block = decoder.decode(&data, 0).unwrap();
        assert_eq!(block.rows, 3);
        assert_eq!(block.columns[4].text(0), b"1000000001");
        assert_eq!(block.columns[6].text(0), b"2025-06-16");
        assert!(!block.columns[6].is_valid(2));
        let Column::Decimal { values, .. } = &block.columns[8] else {
            panic!()
        };
        assert_eq!(values, &vec![50, 9090, -123456]);

        // A 127-byte record size misaligns everything after record 0.
        let data = &data[..127 * 2];
        let decoder = Decoder::new(schema.clone(), Some(127), DecimalMode::Exact, true).unwrap();
        let failure = decoder.decode(data, 0).unwrap_err();
        assert_eq!(failure.row, 1);
        let message = failure.error.to_string();
        assert!(message.starts_with("record 1, field "), "{message}");
        assert!(message.contains("(offset "), "{message}");

        let lenient = Decoder::new(schema, Some(127), DecimalMode::Exact, false).unwrap();
        let block = lenient.decode(data, 0).unwrap();
        assert!(block.failed > 0);
        let _: &[SampleRow] = &rows;
    }
}
