//! The tab-separated variant of a SAP export.
//!
//! The same table is delivered either as fixed-width `.BIN` shards or as
//! tab-separated `.TXT` shards with a header row, under an identical
//! `DATA.0.TXT` sidecar. This decodes the text form into the same [`Block`]
//! the binary decoder produces, so every writer handles both.
//!
//! Parsing follows Python's `csv` reader with a tab delimiter (quoted fields,
//! doubled quotes), because that is what the reference implementation uses.

use std::sync::Arc;

use encoding_rs::Encoding;

use crate::decode::{Block, DecimalMode};
use crate::error::{Error, Result};
use crate::schema::{FieldType, Schema};

/// Text shards observed so far are windows-1251; the exporter does not say.
pub const DEFAULT_ENCODING: &str = "windows-1251";

const STRIP: &[char] = &['\0', '\t', '\n', '\x0b', '\x0c', '\r', ' '];

/// Split tab-separated text into rows of cells, following Python's `csv`
/// module semantics for the default dialect.
fn parse_rows(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut at_start = true;
    let mut row_started = false;

    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if at_start => {
                quoted = true;
                at_start = false;
                row_started = true;
            }
            c if c == delimiter => {
                row.push(std::mem::take(&mut cell));
                at_start = true;
                row_started = true;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if row_started || !cell.is_empty() {
                    row.push(std::mem::take(&mut cell));
                    rows.push(std::mem::take(&mut row));
                } else {
                    rows.push(Vec::new());
                }
                at_start = true;
                row_started = false;
            }
            c => {
                cell.push(c);
                at_start = false;
                row_started = true;
            }
        }
    }
    if row_started || !cell.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

/// Options for decoding a text shard.
#[derive(Debug, Clone)]
pub struct TextOptions {
    pub encoding: &'static Encoding,
    pub mode: DecimalMode,
    pub strict: bool,
}

impl TextOptions {
    pub fn new(encoding: Option<&str>, mode: DecimalMode, strict: bool) -> Result<Self> {
        let label = encoding.unwrap_or(DEFAULT_ENCODING);
        let encoding = Encoding::for_label(label.as_bytes())
            .ok_or_else(|| Error::Schema(format!("unknown text encoding '{label}'")))?;
        Ok(Self {
            encoding,
            mode,
            strict,
        })
    }
}

/// Decode a whole text shard into blocks of at most `block_rows` rows.
pub fn decode_text_shard(
    bytes: &[u8],
    schema: &Arc<Schema>,
    options: &TextOptions,
    block_rows: usize,
    limit: Option<u64>,
    mut emit: impl FnMut(Block) -> Result<()>,
) -> Result<()> {
    let (text, _, _) = options.encoding.decode(bytes);
    let rows = parse_rows(&text, '\t');
    let mut rows = rows.into_iter();

    let mut names: Vec<String> = schema.field_names().map(str::to_owned).collect();
    let Some(header) = rows.next() else {
        return Ok(());
    };
    let header: Vec<String> = header
        .iter()
        .map(|c| c.trim_matches(STRIP).to_owned())
        .filter(|c| !c.is_empty())
        .collect();
    if !header.is_empty() && options.strict && header != names {
        let mut missing: Vec<&str> = names
            .iter()
            .filter(|n| !header.contains(n))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            missing.sort_unstable();
            return Err(Error::Schema(format!(
                "text shard header does not match the schema; missing column(s): {}",
                missing.join(", ")
            )));
        }
        names = header;
    }

    // Where each schema field sits in a row, by header name.
    let positions: Vec<Option<usize>> = schema
        .fields()
        .iter()
        .map(|f| names.iter().position(|n| *n == f.name))
        .collect();

    let mut block = Block::new(schema.clone(), options.mode, block_rows);
    let mut record_index: u64 = 0;
    for row in rows {
        if limit.is_some_and(|l| record_index >= l) {
            break;
        }
        if !row.iter().any(|c| !c.trim_matches(STRIP).is_empty()) {
            continue;
        }
        for ((field, column), position) in schema
            .fields()
            .iter()
            .zip(block.columns.iter_mut())
            .zip(&positions)
        {
            let raw = position
                .and_then(|p| row.get(p))
                .map_or("", |c| c.trim_matches(STRIP));
            if field.kind == FieldType::P {
                match parse_decimal(raw, field.decimals) {
                    Some(value) => column.push_unscaled(value, field.decimals),
                    None if options.strict => {
                        return Err(Error::record(
                            format!("record {record_index}, field {}: {raw:?} is not a number", field.name),
                            record_index,
                            Some(&field.name),
                        ))
                    }
                    None => {
                        column.push_null();
                        block.failed += 1;
                    }
                }
            } else {
                column.push_str(field.kind, raw);
            }
        }
        block.rows += 1;
        record_index += 1;
        if block.rows == block_rows {
            emit(std::mem::replace(
                &mut block,
                Block::new(schema.clone(), options.mode, block_rows),
            ))?;
        }
    }
    if block.rows > 0 {
        emit(block)?;
    }
    Ok(())
}

/// Parse SAP's text rendering of an amount into an unscaled integer at
/// `scale`: optional sign, digits with an optional point and exponent, and
/// SAP's trailing minus for credits (`123.45-`). Spaces are ignored. Extra
/// fraction digits round half to even, as Python's `Decimal.quantize` does.
/// A blank amount is zero.
pub fn parse_decimal(raw: &str, scale: u32) -> Option<i128> {
    let text: String = raw.chars().filter(|c| *c != ' ').collect();
    if text.is_empty() {
        return Some(0);
    }
    let (mut negative, mut body) = match text.strip_suffix('-') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    if let Some(rest) = body.strip_prefix('-') {
        if negative {
            return None;
        }
        negative = true;
        body = rest;
    } else if let Some(rest) = body.strip_prefix('+') {
        body = rest;
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()?),
        None => (body, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    // value = digits * 10^(exponent - len(fraction)); want digits * 10^shift.
    let shift = exponent as i64 - fraction.len() as i64 + scale as i64;
    if digits.len() as i64 + shift.max(0) > 38 {
        return None;
    }
    let mut value: i128 = if digits.is_empty() { 0 } else { digits.parse().ok()? };
    if shift >= 0 {
        value = value.checked_mul(10i128.checked_pow(shift as u32)?)?;
    } else {
        let divisor = 10i128.checked_pow((-shift) as u32).unwrap_or(i128::MAX);
        let (quotient, remainder) = (value / divisor, value % divisor);
        let twice = remainder.saturating_mul(2);
        value = if twice > divisor || (twice == divisor && quotient % 2 == 1) {
            quotient + 1
        } else {
            quotient
        };
    }
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Column;
    use crate::sample::BSIS_SIDECAR;

    const SHARD: &str = "BUKRS\tHKONT\tZUONR\tGJAHR\tBELNR\tBUZEI\tBUDAT\tBLART\tDMBTR\r\n\
        0100\t0000123456\t20250601\t2025\t1000000005\t001\t20250601\tPR\t47.12 \r\n\
        0100\t0000123456\t20250601\t2025\t1000000006\t001\t20250601\tПР\t931.68-\r\n";

    fn decode(text: &str, strict: bool) -> Result<Vec<Block>> {
        let schema = Arc::new(Schema::parse(BSIS_SIDECAR, None).unwrap());
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(text);
        let options = TextOptions::new(None, DecimalMode::Exact, strict).unwrap();
        let mut blocks = Vec::new();
        decode_text_shard(&bytes, &schema, &options, 1000, None, |b| {
            blocks.push(b);
            Ok(())
        })?;
        Ok(blocks)
    }

    #[test]
    fn reads_rows_like_the_binary_decoder() {
        let blocks = decode(SHARD, true).unwrap();
        let block = &blocks[0];
        assert_eq!(block.rows, 2);
        assert_eq!(block.columns[4].text(0), b"1000000005");
        assert_eq!(block.columns[6].text(0), b"2025-06-01");
        assert_eq!(block.columns[7].text(1), "ПР".as_bytes());
        let Column::Decimal { values, .. } = &block.columns[8] else {
            panic!()
        };
        assert_eq!(values, &vec![4712, -93168]);
    }

    #[test]
    fn rejects_a_header_missing_a_column() {
        let err = decode(&SHARD.replacen("BUKRS\t", "", 1), true).unwrap_err();
        assert!(err.to_string().contains("missing column(s): BUKRS"));
    }

    #[test]
    fn lenient_mode_nulls_a_bad_number() {
        let blocks = decode(&SHARD.replace("47.12 ", "not-a-number"), false).unwrap();
        assert!(!blocks[0].columns[8].is_valid(0));
    }

    #[test]
    fn parses_amounts() {
        assert_eq!(parse_decimal("", 2), Some(0));
        assert_eq!(parse_decimal("47.12", 2), Some(4712));
        assert_eq!(parse_decimal("47.12-", 2), Some(-4712));
        assert_eq!(parse_decimal("1 234.5", 2), Some(123450));
        assert_eq!(parse_decimal("0.125", 2), Some(12));
        assert_eq!(parse_decimal("0.135", 2), Some(14));
        assert_eq!(parse_decimal("1e2", 0), Some(100));
        assert_eq!(parse_decimal("abc", 2), None);
        assert_eq!(parse_decimal("-0.00", 2), Some(0));
    }

    #[test]
    fn csv_semantics() {
        let rows = parse_rows("a\t\"b\tc\"\t\"d\"\"e\"\n\n\"x\ny\"\tz", '\t');
        assert_eq!(rows[0], vec!["a", "b\tc", "d\"e"]);
        assert!(rows[1].is_empty());
        assert_eq!(rows[2], vec!["x\ny", "z"]);
    }
}
