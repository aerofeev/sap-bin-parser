"""The tab-separated delivery format.

The same table arrives either as fixed-width ``.BIN`` or as tab-separated
``.TXT``, under an identical sidecar. Both must yield the same rows.
"""

from __future__ import annotations

import io
import zipfile
from decimal import Decimal
from pathlib import Path

import pytest

from sap_bin_parser.archive import SapArchive
from sap_bin_parser.decode import DecodeError
from sap_bin_parser.reader import BinReader
from sap_bin_parser.schema import Schema
from sap_bin_parser.text import TextReader

from .conftest import BSIS_ROWS, BSIS_SIDECAR, encode_file

TEXT_SHARD = (
    "BUKRS\tHKONT\tZUONR\tGJAHR\tBELNR\tBUZEI\tBUDAT\tBLART\tDMBTR\r\n"
    "0100\t0000123456\t20250601\t2025\t1000000005\t001\t20250601\tPR\t47.12 \r\n"
    "0100\t0000123456\t20250601\t2025\t1000000006\t001\t20250601\tPR\t931.68 \r\n"
)


def _text_source(text: str) -> io.BytesIO:
    return io.BytesIO(text.encode("windows-1251"))


class TestTextReader:
    def test_reads_rows(self, bsis_schema: Schema):
        rows = list(TextReader(_text_source(TEXT_SHARD), bsis_schema))
        assert len(rows) == 2
        assert rows[0]["BELNR"] == "1000000005"

    def test_amounts_become_decimals(self, bsis_schema: Schema):
        rows = list(TextReader(_text_source(TEXT_SHARD), bsis_schema))
        assert rows[0]["DMBTR"] == Decimal("47.12")
        assert rows[1]["DMBTR"] == Decimal("931.68")

    def test_dates_become_iso(self, bsis_schema: Schema):
        assert list(TextReader(_text_source(TEXT_SHARD), bsis_schema))[0]["BUDAT"] == "2025-06-01"

    def test_trailing_minus_is_a_credit(self, bsis_schema: Schema):
        text = TEXT_SHARD.replace("47.12 ", "47.12-")
        assert list(TextReader(_text_source(text), bsis_schema))[0]["DMBTR"] == Decimal("-47.12")

    def test_null_date_is_none(self, bsis_schema: Schema):
        text = TEXT_SHARD.replace("\t20250601\tPR", "\t00000000\tPR")
        assert list(TextReader(_text_source(text), bsis_schema))[0]["BUDAT"] is None

    def test_blank_amount_is_zero(self, bsis_schema: Schema):
        text = TEXT_SHARD.replace("\t47.12 ", "\t")
        assert list(TextReader(_text_source(text), bsis_schema))[0]["DMBTR"] == Decimal("0.00")

    def test_float_mode(self, bsis_schema: Schema):
        rows = list(TextReader(_text_source(TEXT_SHARD), bsis_schema, decimal_as_float=True))
        assert isinstance(rows[0]["DMBTR"], float)

    def test_rejects_header_missing_a_column(self, bsis_schema: Schema):
        text = TEXT_SHARD.replace("BUKRS\t", "")
        with pytest.raises(DecodeError, match="missing column"):
            list(TextReader(_text_source(text), bsis_schema))

    def test_non_strict_tolerates_a_bad_number(self, bsis_schema: Schema):
        text = TEXT_SHARD.replace("47.12 ", "not-a-number")
        rows = list(TextReader(_text_source(text), bsis_schema, strict=False))
        assert rows[0]["DMBTR"] is None

    def test_decodes_cyrillic(self, bsis_schema: Schema):
        text = TEXT_SHARD.replace("\tPR\t", "\tПР\t")
        assert list(TextReader(_text_source(text), bsis_schema))[0]["BLART"] == "ПР"


class TestFormatsAgree:
    def test_same_rows_from_bin_and_text(self, tmp_path: Path, bsis_schema: Schema):
        """The two delivery formats must decode to identical rows."""
        binary = tmp_path / "DATA.1.BIN"
        binary.write_bytes(encode_file(bsis_schema, BSIS_ROWS))
        from_bin = list(BinReader(binary, bsis_schema))

        # Render the same records in the text form the exporter emits.
        lines = ["\t".join(bsis_schema.field_names)]
        for row in BSIS_ROWS:
            lines.append(
                "\t".join(
                    str(row[name]) if row[name] != "" else "" for name in bsis_schema.field_names
                )
            )
        from_text = list(TextReader(_text_source("\r\n".join(lines) + "\r\n"), bsis_schema))

        assert len(from_bin) == len(from_text)
        for a, b in zip(from_bin, from_text, strict=True):
            assert a == b


class TestArchiveFormatDetection:
    """Shard 0 is the sidecar; DATA.N.TXT for N>=1 is data, not a schema."""

    def _archive(self, tmp_path: Path, suffix: str, payload: bytes) -> Path:
        path = tmp_path / f"BSIS.{suffix}.zip"

        def nested(name: str, data: bytes) -> bytes:
            buffer = io.BytesIO()
            with zipfile.ZipFile(buffer, "w") as zf:
                zf.writestr(name, data)
            return buffer.getvalue()

        with zipfile.ZipFile(path, "w") as outer:
            outer.writestr("BSIS.QUERY/DATA.0.zip", nested("DATA.0.TXT", BSIS_SIDECAR.encode()))
            outer.writestr("BSIS.QUERY/DATA.1.zip", nested(f"DATA.1.{suffix}", payload))
        return path

    def test_text_shards_are_data_not_schema(self, tmp_path: Path):
        path = self._archive(tmp_path, "TXT", TEXT_SHARD.encode("windows-1251"))
        with SapArchive(path) as archive:
            assert len(archive.data_shards()) == 1
            assert archive.format == "text"

    def test_binary_shards_detected(self, tmp_path: Path, bsis_schema: Schema):
        path = self._archive(tmp_path, "BIN", encode_file(bsis_schema, BSIS_ROWS))
        with SapArchive(path) as archive:
            assert len(archive.data_shards()) == 1
            assert archive.format == "bin"

    def test_schema_still_found_alongside_text_shards(self, tmp_path: Path):
        path = self._archive(tmp_path, "TXT", TEXT_SHARD.encode("windows-1251"))
        with SapArchive(path) as archive:
            assert len(archive.schema()) == 9
