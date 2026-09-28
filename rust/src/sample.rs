//! Synthetic SAP exports, built with the engine's own encoder.
//!
//! No client data ships with this project. These reproduce the *shape* of
//! real exports: the BSIS sidecar, UTF-16BE character fields, packed-decimal
//! amounts, the one-byte record padding, and the zip of per-shard zips. The
//! same layout and rows exist in the Python package (`sap_bin_parser.testing`)
//! so the two implementations are tested on identical vectors.

use std::collections::HashMap;

use crate::decode::pack_decimal;
use crate::schema::{FieldType, Schema};
use crate::zip::{ZipMethod, ZipWriter};

/// The BSIS layout, as the sidecar of a real export declares it. Field
/// widths sum to 125 bytes, so records are padded to 126.
pub const BSIS_SIDECAR: &str = "NAME\tTABLE\tTYPE\tLENG\tDEC\tSIZE\tROLL\tKEY
BUKRS\t\tC\t4 \t0 \t8 \tBUKRS\t
HKONT\t\tC\t10 \t0 \t20 \tHKONT\t
ZUONR\t\tC\t18 \t0 \t36 \tDZUONR\t
GJAHR\t\tN\t4 \t0 \t8 \tGJAHR\t
BELNR\t\tC\t10 \t0 \t20 \tBELNR_D\t
BUZEI\t\tN\t3 \t0 \t6 \tBUZEI\t
BUDAT\t\tD\t8 \t0 \t16 \tBUDAT\t
BLART\t\tC\t2 \t0 \t4 \tBLART\t
DMBTR\t\tP\t7 \t2 \t7 \tDMBTR\t
";

/// One synthetic record: field name to value. Packed decimals are given as
/// decimal text (`"-1234.56"`).
pub type SampleRow = HashMap<&'static str, String>;

fn row(values: [(&'static str, &str); 9]) -> SampleRow {
    values.into_iter().map(|(k, v)| (k, v.to_owned())).collect()
}

/// The three reference rows shared with the Python test suite.
pub fn bsis_rows() -> Vec<SampleRow> {
    vec![
        row([
            ("BUKRS", "0100"),
            ("HKONT", "0000123456"),
            ("ZUONR", "20250616"),
            ("GJAHR", "2025"),
            ("BELNR", "1000000001"),
            ("BUZEI", "001"),
            ("BUDAT", "20250616"),
            ("BLART", "PR"),
            ("DMBTR", "0.50"),
        ]),
        row([
            ("BUKRS", "0100"),
            ("HKONT", "0000123456"),
            ("ZUONR", ""),
            ("GJAHR", "2025"),
            ("BELNR", "1000000002"),
            ("BUZEI", "003"),
            ("BUDAT", "20250616"),
            ("BLART", "PR"),
            ("DMBTR", "90.90"),
        ]),
        row([
            ("BUKRS", "0100"),
            ("HKONT", "0000654321"),
            ("ZUONR", "REVERSAL"),
            ("GJAHR", "2025"),
            ("BELNR", "1000000003"),
            ("BUZEI", "002"),
            ("BUDAT", "00000000"),
            ("BLART", "KR"),
            ("DMBTR", "-1234.56"),
        ]),
    ]
}

/// Parse decimal text into an unscaled integer at `scale`, truncating any
/// extra fraction digits (sample data never has them).
fn unscaled(text: &str, scale: u32) -> i128 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let mut digits = String::from(whole);
    for i in 0..scale as usize {
        digits.push(fraction.as_bytes().get(i).map_or('0', |&b| b as char));
    }
    let value: i128 = digits.parse().unwrap_or(0);
    if negative {
        -value
    } else {
        value
    }
}

/// Encode one record the way the SAP exporter does.
pub fn encode_record(schema: &Schema, values: &SampleRow, out: &mut Vec<u8>) {
    let start = out.len();
    for field in schema.fields() {
        let value = values.get(field.name.as_str()).map_or("", String::as_str);
        if field.kind == FieldType::P {
            let packed = pack_decimal(unscaled(value, field.decimals), field.size)
                .expect("sample value fits its field");
            out.extend_from_slice(&packed);
        } else {
            let mut units: Vec<u16> = value.encode_utf16().take(field.length).collect();
            units.resize(field.length, 0x20);
            out.extend(units.iter().flat_map(|u| u.to_be_bytes()));
        }
    }
    if (out.len() - start) % 2 == 1 {
        out.push(0);
    }
}

pub fn encode_file(schema: &Schema, rows: &[SampleRow]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows.len() * schema.record_size());
    for row in rows {
        encode_record(schema, row, &mut out);
    }
    out
}

/// A tiny deterministic generator (SplitMix64), so samples are reproducible
/// without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Encode `count` plausible BSIS records directly, without building rows:
/// fast enough to synthesise millions of records for the benchmark.
pub fn encode_bsis_records(count: usize, seed: u64, out: &mut Vec<u8>) {
    let accounts = ["0000123456", "0000654321", "0000400000", "0000191000"];
    let kinds = ["PR", "KR", "SA", "DZ", "AB"];
    let mut rng = Rng(seed);
    let put = |out: &mut Vec<u8>, text: &str, width: usize| {
        let mut n = 0;
        for unit in text.encode_utf16().take(width) {
            out.extend_from_slice(&unit.to_be_bytes());
            n += 1;
        }
        for _ in n..width {
            out.extend_from_slice(&[0, 0x20]);
        }
    };
    out.reserve(count * 126);
    for index in 0..count {
        let month = index % 12 + 1;
        let day = index % 28 + 1;
        let date = format!("2025{month:02}{day:02}");
        put(out, "0100", 4);
        put(out, accounts[index % accounts.len()], 10);
        put(out, if index % 7 == 0 { "" } else { &date }, 18);
        put(out, "2025", 4);
        put(out, &format!("{:010}", 1_000_000_000 + index), 10);
        put(out, &format!("{:03}", index % 999 + 1), 3);
        put(out, if index % 11 == 0 { "00000000" } else { &date }, 8);
        put(out, kinds[index % kinds.len()], 2);
        let cents = (rng.next() % 55_000_000) as i128 - 5_000_000;
        out.extend_from_slice(&pack_decimal(cents, 7).unwrap());
        out.push(0);
    }
}

/// A zip-of-zips archive shaped like a delivered export, in memory.
pub fn build_archive(sidecar: &str, table: &str, suffix: &str, shards: &[Vec<u8>]) -> Vec<u8> {
    let nested = |name: &str, data: &[u8]| -> Vec<u8> {
        let mut inner = ZipWriter::new(Vec::new());
        inner
            .add(name, ZipMethod::Deflate, data)
            .expect("in-memory zip");
        inner.finish().expect("in-memory zip")
    };
    let mut outer = ZipWriter::new(Vec::new());
    let folder = format!("{table}.QUERY");
    outer
        .add(&format!("{folder}/"), ZipMethod::Stored, b"")
        .expect("in-memory zip");
    outer
        .add(
            &format!("{folder}/DATA.0.zip"),
            ZipMethod::Stored,
            &nested("DATA.0.TXT", sidecar.as_bytes()),
        )
        .expect("in-memory zip");
    for (index, data) in shards.iter().enumerate() {
        let index = index + 1;
        outer
            .add(
                &format!("{folder}/DATA.{index}.zip"),
                ZipMethod::Stored,
                &nested(&format!("DATA.{index}.{suffix}"), data),
            )
            .expect("in-memory zip");
    }
    outer.finish().expect("in-memory zip")
}

/// A complete synthetic `BSIS.QUERY.zip` with `records` rows in each shard.
pub fn sample_archive(records: usize, shards: usize) -> Vec<u8> {
    let payloads: Vec<Vec<u8>> = (0..shards)
        .map(|shard| {
            let mut data = Vec::new();
            encode_bsis_records(records, 1 + shard as u64, &mut data);
            data
        })
        .collect();
    build_archive(BSIS_SIDECAR, "BSIS", "BIN", &payloads)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_records_have_the_schema_width() {
        let schema = Schema::parse(BSIS_SIDECAR, None).unwrap();
        let mut data = Vec::new();
        encode_bsis_records(10, 1, &mut data);
        assert_eq!(data.len(), 10 * schema.record_size());
        assert_eq!(encode_file(&schema, &bsis_rows()).len(), 3 * 126);
    }
}
