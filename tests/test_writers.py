"""CSV and Parquet output."""

from __future__ import annotations

import csv
from decimal import Decimal
from pathlib import Path

import pytest

from sap_bin_parser.reader import BinReader
from sap_bin_parser.schema import Schema, schema_from_tuples
from sap_bin_parser.writers import write_csv, write_parquet

from .conftest import BSIS_ROWS, encode_file

pyarrow = pytest.importorskip("pyarrow", reason="Parquet output requires pyarrow")


class TestCsv:
    def test_writes_header_and_rows(self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path):
        out = tmp_path / "bsis.csv"
        stats = write_csv(BinReader(bsis_bin, bsis_schema), out, bsis_schema)
        assert stats.rows == len(BSIS_ROWS)

        rows = list(csv.DictReader(out.open(encoding="utf-8")))
        assert len(rows) == len(BSIS_ROWS)
        assert rows[0]["BELNR"] == "1000000001"

    def test_writes_decimals_positionally(
        self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path
    ):
        # Exponent notation ("5E-1") would be read as text by a spreadsheet.
        out = tmp_path / "bsis.csv"
        write_csv(BinReader(bsis_bin, bsis_schema), out, bsis_schema)
        rows = list(csv.DictReader(out.open(encoding="utf-8")))
        assert rows[0]["DMBTR"] == "0.50"
        assert rows[2]["DMBTR"] == "-1234.56"

    def test_null_date_becomes_empty_cell(
        self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path
    ):
        out = tmp_path / "bsis.csv"
        write_csv(BinReader(bsis_bin, bsis_schema), out, bsis_schema)
        assert list(csv.DictReader(out.open(encoding="utf-8")))[2]["BUDAT"] == ""

    def test_quotes_a_field_containing_the_delimiter(self, tmp_path: Path):
        schema = schema_from_tuples([("SGTXT", "C", 12, 0, 24)])
        payload = "a,b\nc".ljust(12).encode("utf-16-be")
        source = tmp_path / "t.BIN"
        source.write_bytes(payload)

        out = tmp_path / "t.csv"
        write_csv(BinReader(source, schema), out, schema)
        rows = list(csv.DictReader(out.open(encoding="utf-8")))
        assert rows[0]["SGTXT"] == "a,b\nc"

    def test_honours_a_custom_delimiter(self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path):
        out = tmp_path / "bsis.tsv"
        write_csv(BinReader(bsis_bin, bsis_schema), out, bsis_schema, delimiter="\t")
        assert "\t" in out.read_text(encoding="utf-8").splitlines()[0]

    def test_creates_missing_parent_directories(
        self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path
    ):
        out = tmp_path / "nested" / "deeper" / "bsis.csv"
        write_csv(BinReader(bsis_bin, bsis_schema), out, bsis_schema)
        assert out.exists()


class TestParquet:
    def test_writes_readable_file(self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path):
        import pyarrow.parquet as pq

        out = tmp_path / "bsis.parquet"
        stats = write_parquet(BinReader(bsis_bin, bsis_schema), out, bsis_schema)
        assert stats.rows == len(BSIS_ROWS)

        table = pq.read_table(out)
        assert table.num_rows == len(BSIS_ROWS)
        assert table.column_names == list(bsis_schema.field_names)

    def test_amounts_are_exact_decimals(self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path):
        import pyarrow as pa
        import pyarrow.parquet as pq

        out = tmp_path / "bsis.parquet"
        write_parquet(BinReader(bsis_bin, bsis_schema), out, bsis_schema)
        table = pq.read_table(out)

        assert pa.types.is_decimal(table.schema.field("DMBTR").type)
        assert table.column("DMBTR").to_pylist()[0] == Decimal("0.50")
        assert table.column("DMBTR").to_pylist()[2] == Decimal("-1234.56")

    def test_float_mode_when_asked(self, bsis_bin: Path, bsis_schema: Schema, tmp_path: Path):
        import pyarrow as pa
        import pyarrow.parquet as pq

        out = tmp_path / "bsis.parquet"
        write_parquet(
            BinReader(bsis_bin, bsis_schema, decimal_as_float=True),
            out,
            bsis_schema,
            decimal_as_float=True,
        )
        assert pa.types.is_floating(pq.read_table(out).schema.field("DMBTR").type)

    def test_spans_multiple_batches(self, tmp_path: Path, bsis_schema: Schema):
        import pyarrow.parquet as pq

        count = 2500
        source = tmp_path / "big.BIN"
        source.write_bytes(encode_file(bsis_schema, [BSIS_ROWS[0]] * count))

        out = tmp_path / "big.parquet"
        stats = write_parquet(BinReader(source, bsis_schema), out, bsis_schema, batch_size=1000)
        assert stats.rows == count
        assert pq.read_table(out).num_rows == count

    def test_empty_input_still_yields_a_typed_file(self, tmp_path: Path, bsis_schema: Schema):
        import pyarrow.parquet as pq

        out = tmp_path / "empty.parquet"
        stats = write_parquet(iter([]), out, bsis_schema)
        assert stats.rows == 0
        table = pq.read_table(out)
        assert table.num_rows == 0
        assert table.column_names == list(bsis_schema.field_names)
