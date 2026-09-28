"""Synthetic SAP exports, built with the library's own encoder.

No client data ships with this project. The fixtures reproduce the structure
observed in real exports: a tab-separated schema sidecar, UTF-16BE character
fields, packed-decimal numerics, and a record padded to an even boundary.
"""

from __future__ import annotations

import io
import zipfile
from decimal import Decimal
from pathlib import Path

import pytest

from sap_bin_parser.decode import pack_decimal
from sap_bin_parser.schema import Schema, parse_schema

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

BSIS_ROWS = [
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
            text = str(value).ljust(field.length)[: field.length]
            parts.append(text.encode("utf-16-be"))
    record = b"".join(parts)
    # Pad to the even boundary, as observed in real files.
    if len(record) % 2:
        record += b"\x00"
    return record


def encode_file(schema: Schema, rows: list[dict[str, object]]) -> bytes:
    return b"".join(encode_record(schema, row) for row in rows)


@pytest.fixture
def bsis_schema() -> Schema:
    return parse_schema(BSIS_SIDECAR, name="BSIS")


@pytest.fixture
def simple_schema() -> Schema:
    return parse_schema(SIMPLE_SIDECAR, name="SIMPLE")


@pytest.fixture
def bsis_bin(tmp_path: Path, bsis_schema: Schema) -> Path:
    path = tmp_path / "DATA.1.BIN"
    path.write_bytes(encode_file(bsis_schema, BSIS_ROWS))
    return path


@pytest.fixture
def bsis_archive(tmp_path: Path, bsis_schema: Schema) -> Path:
    """A zip-of-zips archive shaped like a delivered export."""
    archive_path = tmp_path / "BSIS.QUERY.zip"
    payload = encode_file(bsis_schema, BSIS_ROWS)

    def nested(inner_name: str, data: bytes) -> bytes:
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.writestr(inner_name, data)
        return buffer.getvalue()

    with zipfile.ZipFile(archive_path, "w") as outer:
        outer.writestr("BSIS.QUERY/", b"")
        outer.writestr("BSIS.QUERY/DATA.0.zip", nested("DATA.0.TXT", BSIS_SIDECAR.encode()))
        outer.writestr("BSIS.QUERY/DATA.1.zip", nested("DATA.1.BIN", payload))
        outer.writestr("BSIS.QUERY/DATA.2.zip", nested("DATA.2.BIN", payload))
    return archive_path
