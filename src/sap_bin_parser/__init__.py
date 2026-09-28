"""Parse SAP fixed-width binary table exports (``.BIN``) into CSV or Parquet.

The exports this handles are produced by a SAP download of a table or query,
and arrive as a zip of per-shard zips, each holding one ``DATA.N.BIN`` of
fixed-width records, alongside a ``DATA.0.TXT`` sidecar describing the fields.

Typical use — let the export describe itself::

    from sap_bin_parser import SapArchive, BinReader, write_csv

    with SapArchive("BSIS.QUERY.zip") as archive:
        schema = archive.schema()
        for shard, stream in archive.iter_shard_streams():
            rows = BinReader(stream, schema)
            write_csv(rows, f"{shard.name}.csv", schema)
"""

from __future__ import annotations

from .archive import ArchiveError, SapArchive, Shard
from .decode import (
    DecodeError,
    decode_date,
    decode_text,
    decode_time,
    pack_decimal,
    unpack_packed_decimal,
)
from .reader import BinReader, FileGeometry, RecordError, probe_record_size, read_records
from .schema import Field, Schema, SchemaError, load_schema, parse_schema, schema_from_tuples
from .text import TextReader
from .writers import ConversionStats, write_csv, write_parquet

__version__ = "0.2.0"

__all__ = [
    "ArchiveError",
    "BinReader",
    "ConversionStats",
    "DecodeError",
    "Field",
    "FileGeometry",
    "RecordError",
    "SapArchive",
    "Schema",
    "SchemaError",
    "Shard",
    "TextReader",
    "__version__",
    "decode_date",
    "decode_text",
    "decode_time",
    "load_schema",
    "pack_decimal",
    "parse_schema",
    "probe_record_size",
    "read_records",
    "schema_from_tuples",
    "unpack_packed_decimal",
    "write_csv",
    "write_parquet",
]
