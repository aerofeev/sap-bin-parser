"""Reading delivered zip-of-zips export archives."""

from __future__ import annotations

from pathlib import Path

import pytest

from sap_bin_parser.archive import ArchiveError, SapArchive
from sap_bin_parser.reader import BinReader

from .conftest import BSIS_ROWS


class TestArchive:
    def test_lists_data_shards(self, bsis_archive: Path):
        with SapArchive(bsis_archive) as archive:
            shards = archive.data_shards()
        assert [s.index for s in shards] == [1, 2]
        assert all(s.name.endswith(".BIN") for s in shards)

    def test_separates_schema_sidecar_from_data(self, bsis_archive: Path):
        with SapArchive(bsis_archive) as archive:
            assert [s.index for s in archive.shards() if s.is_schema] == [0]

    def test_reads_its_own_schema(self, bsis_archive: Path):
        with SapArchive(bsis_archive) as archive:
            schema = archive.schema()
        assert len(schema) == 9
        assert schema.record_size == 126

    def test_derives_table_name(self, bsis_archive: Path):
        with SapArchive(bsis_archive) as archive:
            assert archive.table_name == "BSIS"

    def test_parses_records_without_unpacking_to_disk(self, bsis_archive: Path):
        with SapArchive(bsis_archive) as archive:
            schema = archive.schema()
            rows = [
                row
                for _, stream in archive.iter_shard_streams()
                for row in BinReader(stream, schema)
            ]
        assert len(rows) == len(BSIS_ROWS) * 2
        assert rows[0]["BELNR"] == "1000000001"

    def test_rejects_a_non_zip(self, tmp_path: Path):
        path = tmp_path / "not.zip"
        path.write_bytes(b"not a zip at all")
        with pytest.raises(ArchiveError, match="readable zip"):
            SapArchive(path)

    def test_reports_a_missing_sidecar(self, tmp_path: Path):
        import zipfile

        path = tmp_path / "NOSCHEMA.zip"
        with zipfile.ZipFile(path, "w") as zf:
            zf.writestr("NOSCHEMA/DATA.1.BIN", b"\x00" * 126)
        with SapArchive(path) as archive, pytest.raises(ArchiveError, match="no DATA"):
            archive.schema()
