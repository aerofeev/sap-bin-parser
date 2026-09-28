"""Synthetic SAP exports, built with the library's own encoder.

No client data ships with this project. Everything here reproduces the *shape*
of real exports: a tab-separated schema sidecar, UTF-16BE character fields,
packed-decimal numerics, records padded to an even boundary, and the zip of
per-shard zips they are delivered in. The test suite, the benchmark and the
web page's "load a sample" button all draw on this module.
"""

from __future__ import annotations

import io
import zipfile
from collections.abc import Iterable, Sequence
from decimal import Decimal
from typing import Any

from .decode import pack_decimal
from .schema import Schema, parse_schema

__all__ = [
    "BSIS_ROWS",
    "BSIS_SIDECAR",
    "SIMPLE_SIDECAR",
    "build_archive",
    "encode_file",
    "encode_record",
    "sample_archive",
    "sample_rows",
]

# The BSIS layout, as the sidecar of a real export declares it. Field widths
# sum to 125 bytes, so records are padded to 126 — the odd-payload case.
BSIS_SIDECAR = """NAME\tTABLE\tTYPE\tLENG\tDEC\tSIZE\tROLL\tKEY
BUKRS\t\tC\t4 \t0 \t8 \tBUKRS\t
HKONT\t\tC\t10 \t0 \t20 \tHKONT\t
ZUONR\t\tC\t18 \t0 \t36 \tDZUONR\t
GJAHR\t\tN\t4 \t0 \t8 \tGJAHR\t
BELNR\t\tC\t10 \t0 \t20 \tBELNR_D\t
BUZEI\t\tN\t3 \t0 \t6 \tBUZEI\t
BUDAT\t\tD\t8 \t0 \t16 \tBUDAT\t
BLART\t\tC\t2 \t0 \t4 \tBLART\t
DMBTR\t\tP\t7 \t2 \t7 \tDMBTR\t
"""

# An even-payload layout, including a time field, to cover the no-padding case.
SIMPLE_SIDECAR = """NAME\tTABLE\tTYPE\tLENG\tDEC\tSIZE\tROLL\tKEY
MANDT\t\tC\t3 \t0 \t6 \tMANDT\t
CPUTM\t\tT\t6 \t0 \t12 \tCPUTM\t
MENGE\t\tP\t7 \t3 \t7 \tMENGE_D\t
"""

BSIS_ROWS: list[dict[str, Any]] = [
    {
        "BUKRS": "0100",
        "HKONT": "0000123456",
        "ZUONR": "20250616",
        "GJAHR": "2025",
        "BELNR": "1000000001",
        "BUZEI": "001",
        "BUDAT": "20250616",
        "BLART": "PR",
        "DMBTR": Decimal("0.50"),
    },
    {
        "BUKRS": "0100",
        "HKONT": "0000123456",
        "ZUONR": "",
        "GJAHR": "2025",
        "BELNR": "1000000002",
        "BUZEI": "003",
        "BUDAT": "20250616",
        "BLART": "PR",
        "DMBTR": Decimal("90.90"),
    },
    {
        # A credit: negative packed decimal, and a null date.
        "BUKRS": "0100",
        "HKONT": "0000654321",
        "ZUONR": "REVERSAL",
        "GJAHR": "2025",
        "BELNR": "1000000003",
        "BUZEI": "002",
        "BUDAT": "00000000",
        "BLART": "KR",
        "DMBTR": Decimal("-1234.56"),
    },
]


def encode_record(schema: Schema, values: dict[str, object]) -> bytes:
    """Encode one record the way the SAP exporter does."""
    parts: list[bytes] = []
    for field in schema:
        value = values.get(field.name, "")
        if field.type == "P":
            parts.append(pack_decimal(Decimal(str(value or 0)), field.size, field.decimals))
        else:
            # Pad in UTF-16 code units, not characters: an emoji takes two.
            encoded = str(value).encode("utf-16-be")[: field.length * 2]
            parts.append(encoded + " ".encode("utf-16-be") * (field.length - len(encoded) // 2))
    record = b"".join(parts)
    # Pad to the even boundary, as observed in real files.
    if len(record) % 2:
        record += b"\x00"
    return record


def encode_file(schema: Schema, rows: Iterable[dict[str, object]]) -> bytes:
    return b"".join(encode_record(schema, row) for row in rows)


def sample_rows(count: int, *, seed: int = 1) -> list[dict[str, Any]]:
    """``count`` plausible BSIS rows, deterministic for a given seed."""
    import random

    rng = random.Random(seed)
    accounts = ["0000123456", "0000654321", "0000400000", "0000191000"]
    kinds = ["PR", "KR", "SA", "DZ", "AB"]
    rows: list[dict[str, Any]] = []
    for index in range(count):
        cents = rng.randint(-5_000_000, 50_000_000)
        day = 1 + index % 28
        rows.append(
            {
                "BUKRS": "0100",
                "HKONT": accounts[index % len(accounts)],
                "ZUONR": f"2025{index % 12 + 1:02d}{day:02d}" if index % 7 else "",
                "GJAHR": "2025",
                "BELNR": f"{1_000_000_000 + index:010d}",
                "BUZEI": f"{index % 999 + 1:03d}",
                "BUDAT": f"2025{index % 12 + 1:02d}{day:02d}" if index % 11 else "00000000",
                "BLART": kinds[index % len(kinds)],
                "DMBTR": Decimal(cents).scaleb(-2),
            }
        )
    return rows


def _nested(inner_name: str, data: bytes, compression: int) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression) as zf:
        zf.writestr(inner_name, data)
    return buffer.getvalue()


def build_archive(
    sidecar: str,
    shards: Sequence[bytes],
    *,
    table: str = "BSIS",
    suffix: str = "BIN",
    inner_compression: int = zipfile.ZIP_DEFLATED,
    outer_compression: int = zipfile.ZIP_STORED,
    sidecar_first: bool = True,
    directory_entry: bool = True,
) -> bytes:
    """A zip-of-zips archive shaped like a delivered export, in memory.

    ``shards`` are the raw bytes of ``DATA.1``, ``DATA.2``, ... in order.
    """
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", outer_compression) as outer:
        if directory_entry:
            outer.writestr(f"{table}.QUERY/", b"")
        members = [
            (
                f"{table}.QUERY/DATA.{index}.zip",
                _nested(f"DATA.{index}.{suffix}", data, inner_compression),
            )
            for index, data in enumerate(shards, start=1)
        ]
        sidecar_member = (
            f"{table}.QUERY/DATA.0.zip",
            _nested("DATA.0.TXT", sidecar.encode("utf-8"), inner_compression),
        )
        ordered = [sidecar_member, *members] if sidecar_first else [*members, sidecar_member]
        for name, data in ordered:
            outer.writestr(name, data)
    return buffer.getvalue()


def sample_archive(records: int = 5_000, *, shards: int = 2, seed: int = 1) -> bytes:
    """A complete synthetic ``BSIS.QUERY.zip`` with ``records`` rows per shard."""
    schema = parse_schema(BSIS_SIDECAR, name="BSIS")
    payloads = [
        encode_file(schema, sample_rows(records, seed=seed + shard)) for shard in range(shards)
    ]
    return build_archive(BSIS_SIDECAR, payloads)
