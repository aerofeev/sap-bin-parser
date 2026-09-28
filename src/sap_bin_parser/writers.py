"""Write decoded records out as CSV or Parquet."""

from __future__ import annotations

import csv
from collections.abc import Iterable, Iterator
from decimal import Decimal
from pathlib import Path
from typing import Any

from .schema import Schema

__all__ = ["write_csv", "write_parquet", "ConversionStats"]

_PARQUET_BATCH = 50_000


class ConversionStats:
    """Row and byte counts for a completed conversion."""

    __slots__ = ("rows", "path")

    def __init__(self, rows: int, path: Path) -> None:
        self.rows = rows
        self.path = path

    def __repr__(self) -> str:
        return f"ConversionStats(rows={self.rows}, path={self.path!s})"


def _csv_value(value: Any) -> Any:
    if value is None:
        return ""
    if isinstance(value, Decimal):
        # str() on a Decimal can produce exponent notation for small values,
        # which spreadsheets read as text; format positionally instead.
        return format(value, "f")
    return value


def write_csv(
    rows: Iterable[dict[str, Any]],
    destination: str | Path,
    schema: Schema,
    *,
    delimiter: str = ",",
    encoding: str = "utf-8",
    write_header: bool = True,
) -> ConversionStats:
    """Stream records to a CSV file.

    Written with ``newline=""`` and ``QUOTE_MINIMAL`` so that free-text fields
    such as ``SGTXT`` survive embedded separators and newlines intact.
    """
    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    names = schema.field_names
    count = 0

    with open(destination, "w", newline="", encoding=encoding) as handle:
        writer = csv.writer(handle, delimiter=delimiter, quoting=csv.QUOTE_MINIMAL)
        if write_header:
            writer.writerow(names)
        for row in rows:
            writer.writerow([_csv_value(row.get(name)) for name in names])
            count += 1

    return ConversionStats(rows=count, path=destination)


def write_parquet(
    rows: Iterable[dict[str, Any]],
    destination: str | Path,
    schema: Schema,
    *,
    compression: str = "zstd",
    batch_size: int = _PARQUET_BATCH,
    decimal_as_float: bool = False,
) -> ConversionStats:
    """Stream records to a Parquet file. Requires ``pyarrow``.

    Packed-decimal fields land as Arrow ``decimal128`` by default, preserving
    the exact value SAP wrote. Pass ``decimal_as_float=True`` for float64.
    """
    try:
        import pyarrow as pa
        import pyarrow.parquet as pq
    except ModuleNotFoundError as exc:  # pragma: no cover
        raise ModuleNotFoundError(
            "Parquet output needs pyarrow; install sap-bin-parser[parquet]"
        ) from exc

    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    arrow_schema = _arrow_schema(schema, decimal_as_float=decimal_as_float)

    count = 0
    writer = None
    try:
        for batch_rows in _batched(rows, batch_size):
            batch = pa.RecordBatch.from_pylist(batch_rows, schema=arrow_schema)
            if writer is None:
                writer = pq.ParquetWriter(destination, arrow_schema, compression=compression)
            writer.write_batch(batch)
            count += len(batch_rows)
        if writer is None:
            # No rows: still emit a valid, empty, correctly-typed file.
            writer = pq.ParquetWriter(destination, arrow_schema, compression=compression)
    finally:
        if writer is not None:
            writer.close()

    return ConversionStats(rows=count, path=destination)


def _arrow_schema(schema: Schema, *, decimal_as_float: bool):
    import pyarrow as pa

    fields = []
    for field in schema:
        if field.type == "P":
            if decimal_as_float:
                dtype = pa.float64()
            else:
                # Digit capacity of a packed field is 2*size-1; keep headroom
                # so no legitimate value overflows the declared precision.
                precision = max(field.size * 2 - 1, field.decimals + 1)
                dtype = pa.decimal128(min(precision, 38), field.decimals)
        else:
            dtype = pa.string()
        fields.append(pa.field(field.name, dtype))
    return pa.schema(fields)


def _batched(rows: Iterable[dict[str, Any]], size: int) -> Iterator[list[dict[str, Any]]]:
    buffer: list[dict[str, Any]] = []
    for row in rows:
        buffer.append(row)
        if len(buffer) >= size:
            yield buffer
            buffer = []
    if buffer:
        yield buffer
