"""Record reading, geometry checks and record-size probing."""

from __future__ import annotations

from decimal import Decimal
from pathlib import Path

import pytest

from sap_bin_parser.reader import BinReader, RecordError, probe_record_size
from sap_bin_parser.schema import Schema

from .conftest import BSIS_ROWS, encode_file


class TestReading:
    def test_reads_every_record(self, bsis_bin: Path, bsis_schema: Schema):
        rows = list(BinReader(bsis_bin, bsis_schema))
        assert len(rows) == len(BSIS_ROWS)

    def test_round_trips_field_values(self, bsis_bin: Path, bsis_schema: Schema):
        first = list(BinReader(bsis_bin, bsis_schema))[0]
        assert first["BUKRS"] == "0100"
        assert first["BELNR"] == "1000000001"
        assert first["BLART"] == "PR"
        assert first["DMBTR"] == Decimal("0.50")

    def test_dates_come_back_iso(self, bsis_bin: Path, bsis_schema: Schema):
        assert list(BinReader(bsis_bin, bsis_schema))[0]["BUDAT"] == "2025-06-16"

    def test_null_date_is_none(self, bsis_bin: Path, bsis_schema: Schema):
        assert list(BinReader(bsis_bin, bsis_schema))[2]["BUDAT"] is None

    def test_negative_amount_keeps_its_sign(self, bsis_bin: Path, bsis_schema: Schema):
        assert list(BinReader(bsis_bin, bsis_schema))[2]["DMBTR"] == Decimal("-1234.56")

    def test_blank_field_is_empty_string(self, bsis_bin: Path, bsis_schema: Schema):
        assert list(BinReader(bsis_bin, bsis_schema))[1]["ZUONR"] == ""

    def test_decimal_as_float_when_asked(self, bsis_bin: Path, bsis_schema: Schema):
        rows = list(BinReader(bsis_bin, bsis_schema, decimal_as_float=True))
        assert isinstance(rows[0]["DMBTR"], float)
        assert rows[0]["DMBTR"] == 0.5

    def test_reads_from_a_stream(self, bsis_bin: Path, bsis_schema: Schema):
        with open(bsis_bin, "rb") as handle:
            assert len(list(BinReader(handle, bsis_schema))) == len(BSIS_ROWS)

    def test_spans_the_internal_block_boundary(self, tmp_path: Path, bsis_schema: Schema):
        # The reader consumes 4096 records per block; a file larger than one
        # block must not drop or duplicate records at the seam.
        count = 4096 + 17
        path = tmp_path / "big.BIN"
        path.write_bytes(encode_file(bsis_schema, [BSIS_ROWS[0]] * count))
        rows = list(BinReader(path, bsis_schema))
        assert len(rows) == count
        assert all(row["BELNR"] == "1000000001" for row in rows)


class TestGeometry:
    def test_reports_record_count(self, bsis_bin: Path, bsis_schema: Schema):
        geometry = BinReader(bsis_bin, bsis_schema).geometry()
        assert geometry.record_count == len(BSIS_ROWS)
        assert geometry.record_size == 126
        assert geometry.is_clean

    def test_detects_trailing_bytes(self, tmp_path: Path, bsis_schema: Schema):
        path = tmp_path / "ragged.BIN"
        path.write_bytes(encode_file(bsis_schema, BSIS_ROWS) + b"\x00\x01\x02")
        assert not BinReader(path, bsis_schema).geometry().is_clean

    def test_trailing_bytes_raise_in_strict_mode(self, tmp_path: Path, bsis_schema: Schema):
        path = tmp_path / "ragged.BIN"
        path.write_bytes(encode_file(bsis_schema, BSIS_ROWS) + b"\x00\x01\x02")
        with pytest.raises(RecordError, match="trailing"):
            list(BinReader(path, bsis_schema))

    def test_trailing_bytes_tolerated_when_not_strict(self, tmp_path: Path, bsis_schema: Schema):
        path = tmp_path / "ragged.BIN"
        path.write_bytes(encode_file(bsis_schema, BSIS_ROWS) + b"\x00\x01\x02")
        assert len(list(BinReader(path, bsis_schema, strict=False))) == len(BSIS_ROWS)

    def test_rejects_record_size_below_payload(self, bsis_bin: Path, bsis_schema: Schema):
        with pytest.raises(ValueError, match="smaller than the schema payload"):
            BinReader(bsis_bin, bsis_schema, record_size=100)


class TestWrongRecordSize:
    """A wrong record size must fail loudly rather than emit plausible junk."""

    def test_misalignment_raises(self, bsis_bin: Path, bsis_schema: Schema):
        with pytest.raises(RecordError):
            list(BinReader(bsis_bin, bsis_schema, record_size=128))

    def test_error_names_the_field_and_offset(self, bsis_bin: Path, bsis_schema: Schema):
        with pytest.raises(RecordError) as excinfo:
            list(BinReader(bsis_bin, bsis_schema, record_size=127))
        assert excinfo.value.field is not None
        assert "offset" in str(excinfo.value)

    def test_non_strict_mode_nulls_the_bad_field(self, bsis_bin: Path, bsis_schema: Schema):
        rows = list(BinReader(bsis_bin, bsis_schema, record_size=127, strict=False))
        assert any(value is None for value in rows[1].values())


class TestProbe:
    def test_ranks_the_true_record_size_first(self, bsis_bin: Path, bsis_schema: Schema):
        results = probe_record_size(bsis_bin, bsis_schema)
        assert results[0][0] == 126

    def test_reports_even_division(self, bsis_bin: Path, bsis_schema: Schema):
        size, clean, divides = probe_record_size(bsis_bin, bsis_schema)[0]
        assert divides
        assert clean == len(BSIS_ROWS)
