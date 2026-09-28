# sap-bin-parser

Parse SAP fixed-width binary table exports (`.BIN`) into CSV or Parquet.

When you download a table or query result out of SAP as a binary export, you get a zip of
per-shard zips holding `DATA.N.BIN` files. There is no delimiter, no header row, and no
obvious record boundary — amounts are packed decimal (COMP-3) and text is UTF-16
big-endian. Opening one in a text editor shows interleaved null bytes and nothing useful.

This reads them.

```bash
pip install 'sap-bin-parser[parquet] @ git+https://github.com/aerofeev/sap-bin-parser'
```

```bash
# What am I even looking at?
sap-bin info BSIS.QUERY.zip --fields

# Eyeball the first few records before committing to a conversion
sap-bin head BSIS.QUERY.zip -n 3

# Convert — the archive carries its own schema, so nothing to declare
sap-bin convert BSIS.QUERY.zip -o bsis.csv
sap-bin convert BSIS.QUERY.zip -o bsis.parquet -f parquet
```

The archive is read in place. A 2.4 GB export does not get unpacked to disk first, and
records stream rather than accumulating in memory.

## The export format

Worked out against real exports; this is the part that is genuinely fiddly.

**The export describes itself.** Every archive ships a `DATA.0.TXT` sidecar giving each
field's name, type, length and byte size:

```
NAME     TABLE  TYPE  LENG  DEC  SIZE  ROLL     KEY
BUKRS           C     4     0    8     BUKRS
HKONT           C     10    0    20    HKONT
DMBTR           P     7     2    7     DMBTR
```

You do not need to transcribe an SE11 screen by hand. `SapArchive.schema()` reads it.

**Five field types, two encodings.** `C` (character), `N` (numeric text), `D` (date,
`YYYYMMDD`) and `T` (time, `HHMMSS`) are all UTF-16BE — two bytes per character, so
`SIZE` is always `LENG * 2`. `P` is packed decimal, where `SIZE` is the raw byte count.

**Packed decimal.** Each byte holds two digits, except the last, which holds one digit
plus a sign nibble: `0xB`/`0xD` negative, `0xA`/`0xC`/`0xE` positive, `0xF` unsigned.
`DEC` gives the implied decimal places — the digits carry no point. A field of all zero
bytes is SAP's null, not a valid COMP-3 value, and is decoded as zero.

**Records are padded to an even boundary.** This is the one that costs people an
afternoon. The BSIS layout above sums to **125** bytes, but records sit at **126** — SAP
pads odd-width records by one byte so each record starts on a two-byte boundary and its
UTF-16 fields stay aligned. Get this wrong and every record after the first is shifted:
you still get output, it still looks tabular, and the amounts are wrong.

So the rule is `record_size = payload + (payload % 2)`, which the library applies for you.
If an export still does not line up, `sap-bin probe` ranks candidate sizes by how many
records decode cleanly and whether the file divides evenly, instead of adjusting a
constant by hand until the output stops looking wrong.

**A wrong record size is an error, not a warning.** Misalignment reliably produces
invalid sign nibbles and undecodable UTF-16, and the parser raises on both rather than
writing plausible-looking garbage. Pass `--on-error skip` if you would rather have
partial output than none.

**The same table ships in two different formats.** Some exports contain fixed-width
`DATA.N.BIN` shards; others contain tab-separated `DATA.N.TXT` shards with a header row
(windows-1251, and a trailing minus for credits) — under an identical sidecar, from the
same table, with no indication on the outside of the archive. `sap-bin info` reports
which you have, and every command picks the right reader on its own, so a mixed pile of
archives can be converted with one loop. Note that `DATA.0.TXT` is always the schema
sidecar, while `DATA.1.TXT` and up are data.

## Library use

```python
from sap_bin_parser import SapArchive, BinReader, write_parquet

with SapArchive("BSIS.QUERY.zip") as archive:
    schema = archive.schema()
    print(f"{len(schema)} fields, {schema.record_size} bytes per record")

    for shard, stream in archive.iter_shard_streams():
        write_parquet(BinReader(stream, schema), f"{shard.name}.parquet", schema)
```

A loose `.BIN` with its sidecar alongside:

```python
from sap_bin_parser import BinReader, load_schema

schema = load_schema("DATA.0.TXT")
for row in BinReader("DATA.1.BIN", schema):
    print(row["BELNR"], row["DMBTR"])
```

Declaring a schema in code, when no sidecar came with the file:

```python
from sap_bin_parser import schema_from_tuples

schema = schema_from_tuples([
    ("BUKRS", "C", 4, 0, 8),
    ("DMBTR", "P", 7, 2, 7),
])
```

Amounts decode to `Decimal` and reach Parquet as `decimal128`, so a value SAP wrote as
`0.07` stays `0.07`. Pass `decimal_as_float=True` (or `--float-decimals`) if you want
float64 and accept the rounding.

## Commands

| | |
|---|---|
| `sap-bin info PATH` | Schema, record geometry, shard count. `--fields` for offsets |
| `sap-bin head PATH` | Decode the first few records and print them |
| `sap-bin probe PATH` | Rank candidate record sizes for a file that will not line up |
| `sap-bin convert PATH -o OUT` | Convert to `--format csv` or `parquet`; `--split` for one file per shard |

`PATH` is either a delivered `.zip` or a loose `.BIN`; for the latter, pass
`--schema DATA.0.TXT`. `sap-bin <command> --help` has the rest.

## Verified against

The format notes above were derived from real SAP ECC exports: the standard ledger tables
`BSIS` and `BSIM`, plus a 61-column custom table exercising the awkward corners — `T`
time fields and four different packed-decimal scales in one record. Checked by decoding a
31.5 MB / 250,000-record `BSIS` shard end to end: every record decoded in strict mode, no
invalid sign nibbles, all dates within range, and the resulting Parquet summed to the
same exact `Decimal` total as a raw scan of the bytes.

No data is included here. The test fixtures are synthetic, generated by the library's own
`pack_decimal` encoder.

## Install

Python 3.10+. CSV output has no dependencies at all; Parquet needs `pyarrow`.

Not published to PyPI; install from git:

```bash
REPO=git+https://github.com/aerofeev/sap-bin-parser.git

pip install "$REPO"                                  # CSV only
pip install "$REPO#egg=sap-bin-parser[parquet]"      # + Parquet
```

Or from a clone, which is what you want for development:

```bash
git clone https://github.com/aerofeev/sap-bin-parser.git
cd sap-bin-parser
pip install -e '.[dev]'
pytest
```

## Licence

MIT
