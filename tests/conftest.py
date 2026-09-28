"""Shared fixtures.

The synthetic exports themselves live in :mod:`sap_bin_parser.testing` so the
benchmark, the web page's sample endpoint and the cross-implementation tests
draw on the same vectors as this suite. No client data is involved anywhere.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from sap_bin_parser.schema import Schema, parse_schema
from sap_bin_parser.testing import (
    BSIS_ROWS,
    BSIS_SIDECAR,
    SIMPLE_SIDECAR,
    build_archive,
    encode_file,
    encode_record,
)

__all__ = ["BSIS_ROWS", "BSIS_SIDECAR", "SIMPLE_SIDECAR", "encode_file", "encode_record"]


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
    """A zip-of-zips archive shaped like a delivered export, with two shards."""
    archive_path = tmp_path / "BSIS.QUERY.zip"
    payload = encode_file(bsis_schema, BSIS_ROWS)
    archive_path.write_bytes(build_archive(BSIS_SIDECAR, [payload, payload]))
    return archive_path
