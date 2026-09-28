"""Read fixed-width records out of a SAP ``.BIN`` export."""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import IO, Any

from .decode import DecodeError, decode_date, decode_text, decode_time, unpack_packed_decimal
from .schema import Field, Schema

__all__ = ["BinReader", "RecordError", "read_records", "probe_record_size"]

# Records are read in blocks rather than one at a time; a whole-file read would
# need ~1.4 GB of RAM for the larger exports in this family.
_BLOCK_RECORDS = 4096


class RecordError(ValueError):
    """A record could not be decoded."""

    def __init__(self, message: str, *, record_index: int, field: str | None = None) -> None:
        super().__init__(message)
        self.record_index = record_index
        self.field = field


@dataclass(frozen=True, slots=True)
class FileGeometry:
    """How a file divides into records under a given schema."""

    path: Path
    file_size: int
    record_size: int
    record_count: int
    trailing_bytes: int

    @property
    def is_clean(self) -> bool:
        """True when the file divides exactly into whole records."""
        return self.trailing_bytes == 0


def _decode_field(field: Field, raw: bytes, *, errors: str, decimal_as_float: bool) -> Any:
    if field.type == "P":
        value = unpack_packed_decimal(raw, field.decimals)
        return float(value) if decimal_as_float else value
    if field.type == "D":
        return decode_date(raw, errors=errors)
    if field.type == "T":
        return decode_time(raw, errors=errors)
    return decode_text(raw, errors=errors)


class BinReader:
    """Iterate the records of a SAP ``.BIN`` export as dictionaries.

    The record size comes from the schema (see :attr:`Schema.record_size`) and
    can be overridden for an export whose sidecar does not match its payload.

    >>> reader = BinReader("DATA.1.BIN", schema)      # doctest: +SKIP
    >>> for row in reader:                            # doctest: +SKIP
    ...     print(row["BELNR"])
    """

    def __init__(
        self,
        path: str | Path | IO[bytes],
        schema: Schema,
        *,
        record_size: int | None = None,
        errors: str = "strict",
        decimal_as_float: bool = False,
        strict: bool = True,
    ) -> None:
        self.schema = schema
        self.record_size = record_size or schema.record_size
        if self.record_size < schema.payload_size:
            raise ValueError(
                f"record_size {self.record_size} is smaller than the schema payload "
                f"({schema.payload_size} bytes across {len(schema)} fields)"
            )
        self.errors = errors
        self.decimal_as_float = decimal_as_float
        self.strict = strict
        self._offsets = schema.offsets()

        if hasattr(path, "read"):
            self._stream: IO[bytes] | None = path  # type: ignore[assignment]
            self.path = Path(getattr(path, "name", "<stream>"))
            self._owns_stream = False
        else:
            self._stream = None
            self.path = Path(path)  # type: ignore[arg-type]
            self._owns_stream = True

    def geometry(self) -> FileGeometry:
        """Report how the file divides into records, without decoding any."""
        size = self.path.stat().st_size
        return FileGeometry(
            path=self.path,
            file_size=size,
            record_size=self.record_size,
            record_count=size // self.record_size,
            trailing_bytes=size % self.record_size,
        )

    def decode_record(self, chunk: bytes, *, record_index: int = 0) -> dict[str, Any]:
        """Decode one record. ``chunk`` must be at least the payload size."""
        row: dict[str, Any] = {}
        for field, offset in self._offsets:
            raw = chunk[offset : offset + field.size]
            try:
                row[field.name] = _decode_field(
                    field, raw, errors=self.errors, decimal_as_float=self.decimal_as_float
                )
            except DecodeError as exc:
                if self.strict:
                    raise RecordError(
                        f"record {record_index}, field {field.name} "
                        f"(offset {offset}, {field.type}{field.length}): {exc}",
                        record_index=record_index,
                        field=field.name,
                    ) from exc
                row[field.name] = None
        return row

    def __iter__(self) -> Iterator[dict[str, Any]]:
        stream = self._stream
        if stream is None:
            # Closed in the finally below; a with-block would close it before
            # the generator has yielded anything.
            stream = open(self.path, "rb")  # noqa: SIM115
        try:
            record_index = 0
            block_size = self.record_size * _BLOCK_RECORDS
            leftover = b""
            while True:
                block = stream.read(block_size)
                if not block:
                    break
                if leftover:
                    block = leftover + block
                    leftover = b""
                usable = len(block) - (len(block) % self.record_size)
                for start in range(0, usable, self.record_size):
                    yield self.decode_record(
                        block[start : start + self.record_size], record_index=record_index
                    )
                    record_index += 1
                leftover = block[usable:]

            if leftover:
                message = (
                    f"{self.path.name}: {len(leftover)} trailing byte(s) after "
                    f"{record_index} records — not a whole multiple of record size "
                    f"{self.record_size}"
                )
                if self.strict:
                    raise RecordError(message, record_index=record_index)
        finally:
            if self._owns_stream and stream is not None:
                stream.close()

    def to_arrow(self, *, batch_size: int = 50_000):
        """Read the file into a ``pyarrow.Table``. Requires ``pyarrow``."""
        try:
            import pyarrow as pa
        except ModuleNotFoundError as exc:  # pragma: no cover
            raise ModuleNotFoundError(
                "to_arrow() needs pyarrow; install sap-bin-parser[arrow]"
            ) from exc

        from .writers import arrow_schema

        target = arrow_schema(self.schema, decimal_as_float=self.decimal_as_float)
        batches, buffer = [], []
        for row in self:
            buffer.append(row)
            if len(buffer) >= batch_size:
                batches.append(pa.RecordBatch.from_pylist(buffer, schema=target))
                buffer = []
        if buffer or not batches:
            batches.append(pa.RecordBatch.from_pylist(buffer, schema=target))
        return pa.Table.from_batches(batches)


def read_records(
    path: str | Path,
    schema: Schema,
    **kwargs: Any,
) -> Iterator[dict[str, Any]]:
    """Convenience wrapper around :class:`BinReader`."""
    yield from BinReader(path, schema, **kwargs)


def probe_record_size(
    path: str | Path,
    schema: Schema,
    *,
    candidates: range | None = None,
    sample_records: int = 200,
) -> list[tuple[int, int, bool]]:
    """Rank candidate record sizes for a file whose geometry is in doubt.

    Returns ``(record_size, records_decoded_cleanly, divides_evenly)`` sorted
    best-first. The schema's own :attr:`Schema.record_size` is normally right;
    this is for exports where it is not, and it replaces adjusting a hardcoded
    size by hand until the output stops looking wrong.
    """
    path = Path(path)
    file_size = path.stat().st_size
    payload = schema.payload_size
    if candidates is None:
        candidates = range(payload, payload + 8)

    results: list[tuple[int, int, bool]] = []
    with open(path, "rb") as handle:
        for candidate in candidates:
            if candidate < payload:
                continue
            handle.seek(0)
            block = handle.read(candidate * sample_records)
            reader = BinReader(path, schema, record_size=candidate, strict=True)
            clean = 0
            for index in range(len(block) // candidate):
                chunk = block[index * candidate : (index + 1) * candidate]
                try:
                    reader.decode_record(chunk, record_index=index)
                except (RecordError, DecodeError):
                    break
                clean += 1
            results.append((candidate, clean, file_size % candidate == 0))

    results.sort(key=lambda item: (item[2], item[1]), reverse=True)
    return results
