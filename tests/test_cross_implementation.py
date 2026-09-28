"""The Python library and the Rust engine must agree exactly.

Both convert the same synthetic exports, chosen to hit the awkward corners:
quoting, embedded newlines, Cyrillic, characters outside the BMP, a
non-breaking space, null dates, null and negatively signed zero amounts,
an amount at the field's full width, a time field, text shards, a folder
input and a hand-supplied schema. CSV must match byte for byte; Parquet must
hold the same values with the same Arrow types.

The Rust binary is found through ``SAP_BIN_RUST`` or at ``target/release``
(``cargo build --release``). Without it these tests are skipped.
"""

from __future__ import annotations

import io
import os
import shutil
import subprocess
import zipfile
from decimal import Decimal
from pathlib import Path

import pytest

from sap_bin_parser.cli import main as python_cli
from sap_bin_parser.schema import parse_schema
from sap_bin_parser.testing import (
    BSIS_ROWS,
    BSIS_SIDECAR,
    SIMPLE_SIDECAR,
    build_archive,
    encode_file,
    sample_rows,
)

ROOT = Path(__file__).resolve().parents[1]


def _rust_binary() -> Path | None:
    candidates = [os.environ.get("SAP_BIN_RUST"), ROOT / "target" / "release" / "sap-bin"]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    found = shutil.which("sap-bin")
    return Path(found) if found else None


RUST = _rust_binary()
pytestmark = pytest.mark.skipif(RUST is None, reason="the Rust sap-bin binary is not built")

EDGE_ROWS = [
    *BSIS_ROWS,
    {
        "BUKRS": "0200",
        "HKONT": "0000999999",
        "ZUONR": 'a,b "quoted"',
        "GJAHR": "2024",
        "BELNR": "1900000001",
        "BUZEI": "001",
        "BUDAT": "20241231",
        "BLART": "SA",
        "DMBTR": Decimal("99999999999.99"),
    },
    {
        "BUKRS": "0200",
        "HKONT": "Пример",
        "ZUONR": "line one\nline two",
        "GJAHR": "2024",
        "BELNR": "1900000002",
        "BUZEI": "002",
        "BUDAT": "0000",
        "BLART": "Ж",
        "DMBTR": Decimal("-0.01"),
    },
    {
        "BUKRS": "0300",
        "HKONT": "  lead",
        "ZUONR": "emoji \U0001f600 here",
        "GJAHR": "2024",
        "BELNR": "1900000003",
        "BUZEI": "003",
        "BUDAT": "2024-13",
        "BLART": " X",
        "DMBTR": Decimal("0"),
    },
    {
        # Its amount is overwritten below with SAP's all-zero null.
        "BUKRS": "0300",
        "HKONT": "NULLAMOUNT",
        "ZUONR": "",
        "GJAHR": "2024",
        "BELNR": "1900000004",
        "BUZEI": "004",
        "BUDAT": "",
        "BLART": "",
        "DMBTR": Decimal("1"),
    },
    {
        # Its amount is overwritten below with a negatively signed zero.
        "BUKRS": "0300",
        "HKONT": "NEGZERO",
        "ZUONR": "",
        "GJAHR": "2024",
        "BELNR": "1900000005",
        "BUZEI": "005",
        "BUDAT": "20240101",
        "BLART": "",
        "DMBTR": Decimal("1"),
    },
]


def _edge_payload() -> bytes:
    schema = parse_schema(BSIS_SIDECAR)
    data = bytearray(encode_file(schema, EDGE_ROWS))
    size, amount_at = schema.record_size, 118
    null_row = len(EDGE_ROWS) - 2
    negzero_row = len(EDGE_ROWS) - 1
    data[null_row * size + amount_at : null_row * size + amount_at + 7] = bytes(7)
    data[negzero_row * size + amount_at : negzero_row * size + amount_at + 7] = bytes.fromhex(
        "0000000000000D"
    )
    return bytes(data)


def _run_rust(*args: str) -> None:
    result = subprocess.run([str(RUST), *args], capture_output=True, text=True, check=False)
    assert result.returncode == 0, result.stderr


def _both(tmp_path: Path, source: Path, *flags: str, suffix: str = "csv") -> tuple[Path, Path]:
    python_out = tmp_path / f"python.{suffix}"
    rust_out = tmp_path / f"rust.{suffix}"
    assert python_cli(["convert", str(source), "-o", str(python_out), *flags]) == 0
    _run_rust("convert", str(source), "-o", str(rust_out), "--quiet", *flags)
    return python_out, rust_out


@pytest.fixture
def edge_archive(tmp_path: Path) -> Path:
    path = tmp_path / "BSIS.QUERY.zip"
    payload = _edge_payload()
    many = encode_file(parse_schema(BSIS_SIDECAR), sample_rows(20_000))
    path.write_bytes(build_archive(BSIS_SIDECAR, [payload, many, payload]))
    return path


def test_csv_is_byte_identical(tmp_path: Path, edge_archive: Path):
    python_out, rust_out = _both(tmp_path, edge_archive)
    assert python_out.read_bytes() == rust_out.read_bytes()


def test_csv_edge_values_are_what_we_expect(tmp_path: Path, edge_archive: Path):
    # Guard against both implementations agreeing on something wrong.
    _, rust_out = _both(tmp_path, edge_archive)
    text = rust_out.read_bytes().decode("utf-8")
    assert '"a,b ""quoted"""' in text
    assert '"line one\nline two"' in text
    assert "emoji \U0001f600 here" in text
    assert ",Пример," in text
    assert ",99999999999.99\r\n" in text
    assert "NULLAMOUNT,,2024,1900000004,004,,,0.00\r\n" in text
    assert "NEGZERO,,2024,1900000005,005,2024-01-01,,0.00\r\n" in text
    assert "1900000002,002,,Ж,-0.01\r\n" in text  # any all-zero date is null
    assert ",2024-13, X,0.00\r\n" in text  # odd dates pass through; NBSP is data


@pytest.mark.parametrize(
    "flags",
    [
        ("--float-decimals",),
        ("--delimiter", ";"),
        ("--delimiter", "\t"),
        ("--limit", "7"),
        ("--encoding", "utf-8-sig"),
    ],
)
def test_csv_options_agree(tmp_path: Path, edge_archive: Path, flags: tuple[str, ...]):
    python_out, rust_out = _both(tmp_path, edge_archive, *flags)
    assert python_out.read_bytes() == rust_out.read_bytes()


def test_parquet_holds_the_same_table(tmp_path: Path, edge_archive: Path):
    pq = pytest.importorskip("pyarrow.parquet")
    python_out, rust_out = _both(tmp_path, edge_archive, "-f", "parquet", suffix="parquet")
    python_table, rust_table = pq.read_table(python_out), pq.read_table(rust_out)
    assert python_table.schema.types == rust_table.schema.types
    assert python_table.column_names == rust_table.column_names
    assert python_table.to_pylist() == rust_table.to_pylist()


def test_split_outputs_agree(tmp_path: Path, edge_archive: Path):
    python_dir, rust_dir = tmp_path / "py", tmp_path / "rs"
    assert python_cli(["convert", str(edge_archive), "-o", str(python_dir), "--split"]) == 0
    _run_rust("convert", str(edge_archive), "-o", str(rust_dir), "--split", "--quiet")
    names = sorted(p.name for p in python_dir.iterdir())
    assert (
        names
        == sorted(p.name for p in rust_dir.iterdir())
        == [
            "DATA.1.csv",
            "DATA.2.csv",
            "DATA.3.csv",
        ]
    )
    for name in names:
        assert (python_dir / name).read_bytes() == (rust_dir / name).read_bytes()


def test_time_fields_and_even_records_agree(tmp_path: Path):
    schema = parse_schema(SIMPLE_SIDECAR)
    rows = [
        {"MANDT": "100", "CPUTM": "143005", "MENGE": Decimal("12.345")},
        {"MANDT": "100", "CPUTM": "000000", "MENGE": Decimal("-0.001")},
        {"MANDT": "200", "CPUTM": "", "MENGE": Decimal("0")},
    ]
    path = tmp_path / "SIMPLE.QUERY.zip"
    path.write_bytes(build_archive(SIMPLE_SIDECAR, [encode_file(schema, rows)], table="SIMPLE"))
    python_out, rust_out = _both(tmp_path, path)
    assert python_out.read_bytes() == rust_out.read_bytes()
    assert b"100,00:00:00,-0.001\r\n" in rust_out.read_bytes()


def test_text_shards_agree(tmp_path: Path):
    shard = (
        "BUKRS\tHKONT\tZUONR\tGJAHR\tBELNR\tBUZEI\tBUDAT\tBLART\tDMBTR\r\n"
        "0100\t0000123456\t20250601\t2025\t1000000005\t001\t20250601\tПР\t47.12 \r\n"
        "0100\t0000123456\t\t2025\t1000000006\t002\t00000000\tPR\t931.68-\r\n"
        "0100\t0000123456\tX\t2025\t1000000007\t003\t0000-00-00\tPR\t0.125\r\n"
    ).encode("windows-1251")
    path = tmp_path / "BSIS.QUERY.zip"
    path.write_bytes(build_archive(BSIS_SIDECAR, [shard], suffix="TXT"))
    python_out, rust_out = _both(tmp_path, path)
    assert python_out.read_bytes() == rust_out.read_bytes()
    assert b"1000000006,002,,PR,-931.68\r\n" in rust_out.read_bytes()


def test_loose_bin_with_schema_agrees(tmp_path: Path):
    sidecar = tmp_path / "DATA.0.TXT"
    sidecar.write_text(BSIS_SIDECAR, encoding="utf-8")
    loose = tmp_path / "DATA.1.BIN"
    loose.write_bytes(_edge_payload())
    python_out, rust_out = _both(tmp_path, loose, "--schema", str(sidecar))
    assert python_out.read_bytes() == rust_out.read_bytes()


def test_an_unzipped_folder_matches_the_zip(tmp_path: Path, edge_archive: Path):
    folder = tmp_path / "unzipped"
    zipfile.ZipFile(edge_archive).extractall(folder)
    from_zip, from_folder = tmp_path / "zip.csv", tmp_path / "folder.csv"
    _run_rust("convert", str(edge_archive), "-o", str(from_zip), "--quiet")
    _run_rust("convert", str(folder), "-o", str(from_folder), "--quiet")
    assert from_zip.read_bytes() == from_folder.read_bytes()


def test_separate_shards_match_the_zip(tmp_path: Path, edge_archive: Path):
    archive = zipfile.ZipFile(edge_archive)
    paths = []
    for n in (1, 2, 3):
        inner = zipfile.ZipFile(io.BytesIO(archive.read(f"BSIS.QUERY/DATA.{n}.zip")))
        path = tmp_path / f"DATA.{n}.BIN"
        path.write_bytes(inner.read(f"DATA.{n}.BIN"))
        paths.append(str(path))
    sidecar = tmp_path / "DATA.0.TXT"
    sidecar.write_text(BSIS_SIDECAR, encoding="utf-8")
    python_out = tmp_path / "python.csv"
    rust_out = tmp_path / "rust.csv"
    assert python_cli(["convert", str(edge_archive), "-o", str(python_out)]) == 0
    _run_rust("convert", *paths, "--schema", str(sidecar), "-o", str(rust_out), "--quiet")
    assert python_out.read_bytes() == rust_out.read_bytes()


def test_a_wrong_record_size_fails_in_both(tmp_path: Path, edge_archive: Path):
    python_code = python_cli(
        ["convert", str(edge_archive), "-o", str(tmp_path / "p.csv"), "--record-size", "128"]
    )
    rust = subprocess.run(
        [
            str(RUST),
            "convert",
            str(edge_archive),
            "-o",
            str(tmp_path / "r.csv"),
            "--record-size",
            "128",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert python_code == rust.returncode == 1
    assert "probe" in rust.stderr
