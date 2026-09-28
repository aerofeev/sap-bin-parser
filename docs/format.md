[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · [Python](python.md) · [Docker](docker.md) · [HTTP API](http-api.md) · [Privacy](privacy.md) · [Performance](performance.md) · **Export format**

English · [Русский](ru/format.md)

# The export format

Worked out against real exports; this is the part that is genuinely fiddly.

## The export describes itself

An export arrives as a zip of per-shard zips:

```
BSIS.QUERY.zip
  BSIS.QUERY/DATA.0.zip   -> DATA.0.TXT    the schema sidecar
  BSIS.QUERY/DATA.1.zip   -> DATA.1.BIN    a shard of records
  BSIS.QUERY/DATA.2.zip   -> DATA.2.BIN
```

The `DATA.0.TXT` sidecar gives each field's name, type, length and byte size:

```
NAME     TABLE  TYPE  LENG  DEC  SIZE  ROLL     KEY
BUKRS           C     4     0    8     BUKRS
HKONT           C     10    0    20    HKONT
DMBTR           P     7     2    7     DMBTR
```

## Five field types, two encodings

`C` (character), `N` (numeric text), `D` (date, `YYYYMMDD`) and `T` (time, `HHMMSS`) are all
UTF-16BE, two bytes per character, so `SIZE` is always `LENG * 2`. `P` is packed decimal,
where `SIZE` is the raw byte count.

## Packed decimal

Each byte holds two digits, except the last, which holds one digit plus a sign nibble:
`0xB`/`0xD` negative, `0xA`/`0xC`/`0xE` positive, `0xF` unsigned. `DEC` gives the implied
decimal places; the digits carry no point. A field of all zero bytes is SAP's null, not a
valid COMP-3 value, and is decoded as zero.

## Records are padded to an even boundary

This is the one that costs people an afternoon. The BSIS layout above sums to **125**
bytes, but records sit at **126**: SAP pads odd-width records by one byte so each record
starts on a two-byte boundary and its UTF-16 fields stay aligned. Get this wrong and every
record after the first is shifted: you still get output, it still looks tabular, and the
amounts are wrong.

So the rule is `record_size = payload + (payload % 2)`, which both implementations apply.
If an export still does not line up, `sap-bin probe` ranks candidate sizes by how many
records decode cleanly and whether the data divides evenly.

## A wrong record size is an error, not a warning

Misalignment reliably produces invalid sign nibbles and undecodable UTF-16, and the parser
raises on both rather than writing plausible-looking garbage. `--on-error skip` leaves the
bad values empty, counts them, and carries on.

## The same table ships in two formats

Some exports contain fixed-width `DATA.N.BIN` shards; others contain tab-separated
`DATA.N.TXT` shards with a header row (windows-1251, and a trailing minus for credits),
under an identical sidecar and with no indication on the outside of the archive. Every
command picks the right reader on its own. `DATA.0.TXT` is always the schema sidecar, while
`DATA.1.TXT` and up are data.

## Values

- Text is stripped of NUL and ASCII whitespace only. A non-breaking space is data, and
  survives.
- Dates become `YYYY-MM-DD`. Blank and all-zero dates are empty. Anything else passes
  through unchanged, so an odd value is surfaced rather than dropped.
- Times become `HH:MM:SS`. `000000` is midnight, not empty.

## Verified against

The format notes were derived from real SAP ECC exports: the ledger tables `BSIS` and
`BSIM`, plus a 61-column custom table exercising `T` time fields and four packed-decimal
scales in one record. A 250,000-record `BSIS` shard decoded end to end in strict mode, and
the resulting Parquet summed to the same exact `Decimal` total as a raw scan of the bytes.

No data is included in the repository. Every test fixture is synthetic, built by the
libraries' own encoders. To check a real export of your own:

```bash
python scripts/compare.py YOUR.QUERY.zip --parquet
```

It converts the export with both implementations in a temporary directory, compares the
output byte for byte, reports timings, and deletes everything. It never prints a field
value, so its output is safe to paste into an issue.
