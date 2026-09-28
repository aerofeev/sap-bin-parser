"""Read the tab-separated variant of a SAP export.

The same table is delivered either as fixed-width ``.BIN`` shards or as
tab-separated ``.TXT`` shards, under an identical ``DATA.0.TXT`` sidecar. This
reader yields the same rows as :class:`~sap_bin_parser.reader.BinReader` from
the text form, so downstream code does not care which arrived.
"""

from __future__ import annotations

import csv
import io
from collections.abc import Iterator
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import IO, Any

from .decode import STRIP_CHARS, DecodeError, normalise_date, normalise_time
from .schema import Schema

__all__ = ["TextReader"]

# Text shards observed so far are windows-1251; the exporter does not mark it.
DEFAULT_ENCODING = "windows-1251"


class TextReader:
    """Iterate a tab-separated SAP export shard as dictionaries.

    Values are normalised to match the binary reader's output: dates become
    ISO, packed-decimal columns become :class:`~decimal.Decimal`, and trailing
    padding is stripped.
    """

    def __init__(
        self,
        source: str | Path | IO[bytes] | IO[str],
        schema: Schema,
        *,
        delimiter: str = "\t",
        encoding: str = DEFAULT_ENCODING,
        decimal_as_float: bool = False,
        strict: bool = True,
        has_header: bool = True,
    ) -> None:
        self.schema = schema
        self.delimiter = delimiter
        self.encoding = encoding
        self.decimal_as_float = decimal_as_float
        self.strict = strict
        self.has_header = has_header
        self._source = source

    def _open(self) -> tuple[IO[str], bool]:
        source = self._source
        if hasattr(source, "read"):
            if isinstance(source, io.TextIOBase):
                return source, False  # type: ignore[return-value]
            return io.TextIOWrapper(source, encoding=self.encoding, newline=""), False  # type: ignore[arg-type]
        return open(source, encoding=self.encoding, newline=""), True  # type: ignore[arg-type]

    def __iter__(self) -> Iterator[dict[str, Any]]:
        handle, owned = self._open()
        try:
            reader = csv.reader(handle, delimiter=self.delimiter)
            names = list(self.schema.field_names)

            if self.has_header:
                try:
                    header = next(reader)
                except StopIteration:
                    return
                header = [cell.strip(STRIP_CHARS) for cell in header if cell.strip(STRIP_CHARS)]
                if header and self.strict and header != names:
                    missing = set(names) - set(header)
                    if missing:
                        raise DecodeError(
                            f"text shard header does not match the schema; "
                            f"missing column(s): {', '.join(sorted(missing))}"
                        )
                    names = header

            for row in reader:
                if not any(cell.strip(STRIP_CHARS) for cell in row):
                    continue
                yield self._decode_row(names, row)
        finally:
            if owned:
                handle.close()

    def _decode_row(self, names: list[str], row: list[str]) -> dict[str, Any]:
        by_name = dict(zip(names, (cell.strip(STRIP_CHARS) for cell in row), strict=False))
        result: dict[str, Any] = {}
        for field in self.schema:
            raw = by_name.get(field.name, "")
            if field.type == "P":
                result[field.name] = self._decimal(raw, field.decimals, field.name)
            elif field.type == "D":
                result[field.name] = normalise_date(raw)
            elif field.type == "T":
                result[field.name] = normalise_time(raw)
            else:
                result[field.name] = raw
        return result

    def _decimal(self, raw: str, decimals: int, name: str) -> Any:
        text = raw.strip(STRIP_CHARS).replace(" ", "")
        if not text:
            value = Decimal(0)
        else:
            # A trailing minus ("123.45-") is how SAP writes a credit in text.
            if text.endswith("-"):
                text = "-" + text[:-1]
            try:
                value = Decimal(text)
            except InvalidOperation as exc:
                if self.strict:
                    raise DecodeError(f"field {name}: {raw!r} is not a number") from exc
                return None
        # Always quantize to the field's scale (rounding half to even), so a
        # value never carries more places than its column can hold.
        value = value.quantize(Decimal(1).scaleb(-decimals))
        if not value:
            value = value.copy_abs()  # "-0.00" is zero
        return float(value) if self.decimal_as_float else value
