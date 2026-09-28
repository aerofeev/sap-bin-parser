[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · **Python** · [Docker](docker.md) · [HTTP API](http-api.md) · [Privacy](privacy.md) · [Performance](performance.md) · [Export format](format.md)

English · [Русский](ru/python.md)

# Python

The Python package is the reference implementation: the [app](cli.md) is 15 to 25 times
faster and is tested to produce identical output.

## The single-file script

[`sap-bin.pyz`](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin.pyz)
is the whole Python implementation in one 60 KB file. It needs Python 3.10 or later and
nothing else:

```bash
python sap-bin.pyz convert BSIS.QUERY.zip -o bsis.csv
python sap-bin.pyz info BSIS.QUERY.zip --fields
python sap-bin.pyz head BSIS.QUERY.zip -n 3
```

It has the `info`, `head`, `probe` and `convert` commands of the [app](cli.md), for a
`.zip` or a single `.BIN` with `--schema`. For Parquet output, `pip install pyarrow`
first.

## The library

```bash
pip install 'sap-bin-parser[parquet] @ git+https://github.com/aerofeev/sap-bin-parser'
```

This also installs the script as the `sap-bin-py` command.

```python
from sap_bin_parser import SapArchive, BinReader, write_parquet

with SapArchive("BSIS.QUERY.zip") as archive:
    schema = archive.schema()
    print(f"{len(schema)} fields, {schema.record_size} bytes per record")

    for shard, stream in archive.iter_shard_streams():
        write_parquet(BinReader(stream, schema), f"{shard.name}.parquet", schema)
```

A loose `.BIN` with its sidecar:

```python
from sap_bin_parser import BinReader, load_schema

schema = load_schema("DATA.0.TXT")
for row in BinReader("DATA.1.BIN", schema):
    print(row["BELNR"], row["DMBTR"])
```

A schema declared in code, when no sidecar came with the file:

```python
from sap_bin_parser import schema_from_tuples

schema = schema_from_tuples([
    ("BUKRS", "C", 4, 0, 8),
    ("DMBTR", "P", 7, 2, 7),
])
```

Rows are dictionaries. Amounts decode to `Decimal` and reach Parquet as `decimal128`, so a
value SAP wrote as `0.07` stays `0.07`; pass `decimal_as_float=True` for float64. Dates
and times come back as ISO text (`2025-06-16`, `14:30:05`), and blank dates as `None`.
CSV output needs no dependencies at all.
