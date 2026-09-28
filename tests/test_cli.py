"""Command line behaviour."""

from __future__ import annotations

import csv
from pathlib import Path

import pytest

from sap_bin_parser.cli import main

from .conftest import BSIS_ROWS, BSIS_SIDECAR


@pytest.fixture
def sidecar(tmp_path: Path) -> Path:
    path = tmp_path / "DATA.0.TXT"
    path.write_text(BSIS_SIDECAR, encoding="utf-8")
    return path


class TestInfo:
    def test_reports_geometry_from_archive(self, bsis_archive: Path, capsys):
        assert main(["info", str(bsis_archive)]) == 0
        out = capsys.readouterr().out
        assert "9 fields" in out
        assert "126 bytes" in out
        assert "pad byte" in out

    def test_lists_fields_with_offsets(self, bsis_archive: Path, capsys):
        assert main(["info", str(bsis_archive), "--fields"]) == 0
        out = capsys.readouterr().out
        assert "DMBTR" in out and "OFFSET" in out

    def test_uses_explicit_schema_for_a_bare_bin(self, bsis_bin: Path, sidecar: Path, capsys):
        assert main(["info", str(bsis_bin), "--schema", str(sidecar)]) == 0
        assert f"records: {len(BSIS_ROWS):,}" in capsys.readouterr().out

    def test_errors_without_a_schema(self, bsis_bin: Path, capsys):
        assert main(["info", str(bsis_bin)]) == 2
        assert "no schema available" in capsys.readouterr().err


class TestConvert:
    def test_archive_to_csv(self, bsis_archive: Path, tmp_path: Path):
        out = tmp_path / "bsis.csv"
        assert main(["convert", str(bsis_archive), "-o", str(out)]) == 0
        rows = list(csv.DictReader(out.open(encoding="utf-8")))
        assert len(rows) == len(BSIS_ROWS) * 2  # two shards

    def test_bin_to_csv_with_explicit_schema(self, bsis_bin: Path, sidecar: Path, tmp_path: Path):
        out = tmp_path / "bsis.csv"
        code = main(["convert", str(bsis_bin), "--schema", str(sidecar), "-o", str(out)])
        assert code == 0
        assert len(list(csv.DictReader(out.open(encoding="utf-8")))) == len(BSIS_ROWS)

    def test_split_writes_one_file_per_shard(self, bsis_archive: Path, tmp_path: Path):
        out = tmp_path / "shards"
        assert main(["convert", str(bsis_archive), "-o", str(out), "--split"]) == 0
        assert sorted(p.name for p in out.glob("*.csv")) == ["DATA.1.csv", "DATA.2.csv"]

    def test_limit_caps_total_rows(self, bsis_archive: Path, tmp_path: Path):
        out = tmp_path / "bsis.csv"
        assert main(["convert", str(bsis_archive), "-o", str(out), "--limit", "4"]) == 0
        assert len(list(csv.DictReader(out.open(encoding="utf-8")))) == 4

    def test_custom_delimiter(self, bsis_archive: Path, tmp_path: Path):
        out = tmp_path / "bsis.tsv"
        assert main(["convert", str(bsis_archive), "-o", str(out), "--delimiter", "\t"]) == 0
        assert "\t" in out.read_text(encoding="utf-8").splitlines()[0]

    def test_parquet_output(self, bsis_archive: Path, tmp_path: Path):
        pytest.importorskip("pyarrow")
        import pyarrow.parquet as pq

        out = tmp_path / "bsis.parquet"
        code = main(["convert", str(bsis_archive), "-o", str(out), "-f", "parquet"])
        assert code == 0
        assert pq.read_table(out).num_rows == len(BSIS_ROWS) * 2

    def test_bad_record_size_exits_nonzero_with_a_hint(
        self, bsis_bin: Path, sidecar: Path, tmp_path: Path, capsys
    ):
        out = tmp_path / "bsis.csv"
        code = main(
            ["convert", str(bsis_bin), "--schema", str(sidecar),
             "-o", str(out), "--record-size", "128"]
        )
        assert code == 1
        assert "probe" in capsys.readouterr().err


class TestProbeAndHead:
    def test_probe_ranks_true_size_first(self, bsis_bin: Path, sidecar: Path, capsys):
        assert main(["probe", str(bsis_bin), "--schema", str(sidecar)]) == 0
        lines = [line for line in capsys.readouterr().out.splitlines() if line.strip()]
        assert lines[1].split()[0] == "126"

    def test_head_prints_records(self, bsis_archive: Path, capsys):
        assert main(["head", str(bsis_archive), "-n", "2"]) == 0
        out = capsys.readouterr().out
        assert out.count("--- record") == 2
        assert "BELNR" in out

    def test_missing_file_exits_two(self, tmp_path: Path, capsys):
        assert main(["info", str(tmp_path / "nope.zip")]) == 2
        assert "no such file" in capsys.readouterr().err

    def test_parquet_without_pyarrow_fails_cleanly(
        self, bsis_archive: Path, tmp_path: Path, capsys, monkeypatch
    ):
        # A missing optional extra should be a one-line error, not a traceback.
        import builtins

        real_import = builtins.__import__

        def no_pyarrow(name, *args, **kwargs):
            if name.startswith("pyarrow"):
                raise ModuleNotFoundError(f"No module named {name!r}")
            return real_import(name, *args, **kwargs)

        monkeypatch.setattr(builtins, "__import__", no_pyarrow)
        code = main(
            ["convert", str(bsis_archive), "-o", str(tmp_path / "x.parquet"), "-f", "parquet"]
        )
        assert code == 2
        assert "pyarrow" in capsys.readouterr().err
