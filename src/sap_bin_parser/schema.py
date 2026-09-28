"""Schema handling for SAP flat-file exports.

Every export ships its own schema as a tab-separated sidecar (``DATA.0.TXT``)
with the columns ``NAME TABLE TYPE LENG DEC SIZE ROLL KEY``::

    NAME    TABLE   TYPE    LENG    DEC     SIZE    ROLL    KEY
    BUKRS           C       4       0       8       BUKRS
    DMBTR           P       7       2       7       DMBTR

``SIZE`` is the field's width in bytes and is authoritative for every type:
for the character-ish types it is always ``LENG * 2`` (UTF-16), and for packed
decimals it is the raw byte count. So field widths never need to be derived.
"""

from __future__ import annotations

import csv
import io
from collections.abc import Iterable, Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path

TEXT_TYPES = frozenset({"C", "N"})
DATE_TYPE = "D"
TIME_TYPE = "T"
PACKED_TYPE = "P"
KNOWN_TYPES = frozenset({"C", "N", "D", "T", "P"})

# Types stored as UTF-16BE, i.e. two bytes per declared character.
_WIDE_TYPES = frozenset({"C", "N", "D", "T"})

_EXPECTED_HEADER = ("NAME", "TYPE", "LENG", "DEC", "SIZE")


class SchemaError(ValueError):
    """The schema sidecar could not be understood."""


@dataclass(frozen=True, slots=True)
class Field:
    """One column of a fixed-width SAP record."""

    name: str
    type: str
    length: int
    decimals: int
    size: int

    def __post_init__(self) -> None:
        if self.type not in KNOWN_TYPES:
            raise SchemaError(
                f"field {self.name!r} has unsupported type {self.type!r} "
                f"(known: {', '.join(sorted(KNOWN_TYPES))})"
            )
        if self.size < 1:
            raise SchemaError(f"field {self.name!r} has non-positive size {self.size}")

    @property
    def is_wide_text(self) -> bool:
        """True when the field is stored as UTF-16BE text."""
        return self.type in _WIDE_TYPES

    @property
    def implied_size(self) -> int:
        """The width ``SIZE`` should have, given ``TYPE`` and ``LENG``."""
        return self.length * 2 if self.is_wide_text else self.size


@dataclass(frozen=True, slots=True)
class Schema:
    """An ordered set of fields, plus the record geometry they imply."""

    fields: tuple[Field, ...]
    name: str | None = None

    def __post_init__(self) -> None:
        if not self.fields:
            raise SchemaError("schema has no fields")
        seen: set[str] = set()
        for field in self.fields:
            if field.name in seen:
                raise SchemaError(f"duplicate field name {field.name!r}")
            seen.add(field.name)

    def __iter__(self) -> Iterator[Field]:
        return iter(self.fields)

    def __len__(self) -> int:
        return len(self.fields)

    @property
    def field_names(self) -> tuple[str, ...]:
        return tuple(f.name for f in self.fields)

    @property
    def payload_size(self) -> int:
        """Total bytes the declared fields occupy, before record padding."""
        return sum(f.size for f in self.fields)

    @property
    def record_size(self) -> int:
        """Bytes per record on disk.

        Records are padded to an even boundary so that each one begins on a
        two-byte boundary and its UTF-16 fields stay aligned. A schema whose
        fields sum to an odd width therefore carries one trailing pad byte,
        whose content is unspecified — do not read it.
        """
        payload = self.payload_size
        return payload + (payload % 2)

    @property
    def padding_size(self) -> int:
        """Trailing pad bytes per record: 1 for an odd payload, else 0."""
        return self.record_size - self.payload_size

    def offsets(self) -> tuple[tuple[Field, int], ...]:
        """Each field with its byte offset into a record."""
        result: list[tuple[Field, int]] = []
        offset = 0
        for field in self.fields:
            result.append((field, offset))
            offset += field.size
        return tuple(result)

    def inconsistencies(self) -> tuple[str, ...]:
        """Fields whose ``SIZE`` disagrees with ``TYPE``/``LENG``.

        Empty for every export seen so far; a non-empty result means the
        sidecar is unusual and the geometry deserves a second look.
        """
        return tuple(
            f"{f.name}: SIZE={f.size} but TYPE={f.type} LENG={f.length} implies {f.implied_size}"
            for f in self.fields
            if f.is_wide_text and f.size != f.implied_size
        )


def parse_schema(text: str, *, name: str | None = None) -> Schema:
    """Parse the text of a ``DATA.0.TXT`` schema sidecar."""
    # utf-8-sig handles the BOM some exports carry.
    rows = list(csv.reader(io.StringIO(text.lstrip("﻿")), delimiter="\t"))
    rows = [[cell.strip() for cell in row] for row in rows if any(c.strip() for c in row)]
    if not rows:
        raise SchemaError("schema sidecar is empty")

    header = [cell.upper() for cell in rows[0]]
    if "NAME" not in header or "TYPE" not in header:
        raise SchemaError(
            f"schema sidecar has no recognisable header; got {rows[0]!r}. "
            f"Expected tab-separated columns including {', '.join(_EXPECTED_HEADER)}."
        )
    try:
        index = {key: header.index(key) for key in _EXPECTED_HEADER}
    except ValueError as exc:
        missing = [k for k in _EXPECTED_HEADER if k not in header]
        raise SchemaError(f"schema sidecar is missing column(s): {', '.join(missing)}") from exc

    fields: list[Field] = []
    for line_number, row in enumerate(rows[1:], start=2):
        if len(row) <= max(index.values()):
            raise SchemaError(f"line {line_number}: expected {len(header)} columns, got {len(row)}")
        field_name = row[index["NAME"]]
        if not field_name:
            continue
        try:
            fields.append(
                Field(
                    name=field_name,
                    type=row[index["TYPE"]].upper(),
                    length=int(row[index["LENG"]] or 0),
                    decimals=int(row[index["DEC"]] or 0),
                    size=int(row[index["SIZE"]] or 0),
                )
            )
        except SchemaError as exc:
            raise SchemaError(f"line {line_number}: {exc}") from exc
        except ValueError as exc:
            raise SchemaError(f"line {line_number}: non-numeric LENG/DEC/SIZE in {row!r}") from exc

    return Schema(fields=tuple(fields), name=name)


def load_schema(path: str | Path) -> Schema:
    """Read a schema sidecar from disk."""
    path = Path(path)
    return parse_schema(path.read_text(encoding="utf-8-sig"), name=path.stem)


def schema_from_tuples(rows: Iterable[Sequence[object]], *, name: str | None = None) -> Schema:
    """Build a schema from ``(name, type, length, decimals, size)`` tuples.

    For declaring a schema in code when no sidecar is available.
    """
    fields = []
    for row in rows:
        if len(row) != 5:
            raise SchemaError(
                f"expected 5-tuples (name, type, length, decimals, size), got {row!r}"
            )
        field_name, field_type, length, decimals, size = row
        fields.append(
            Field(
                name=str(field_name),
                type=str(field_type).upper(),
                length=int(length),  # type: ignore[arg-type]
                decimals=int(decimals),  # type: ignore[arg-type]
                size=int(size),  # type: ignore[arg-type]
            )
        )
    return Schema(fields=tuple(fields), name=name)


__all__ = [
    "Field",
    "Schema",
    "SchemaError",
    "parse_schema",
    "load_schema",
    "schema_from_tuples",
    "KNOWN_TYPES",
]
