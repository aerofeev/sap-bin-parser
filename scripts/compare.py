#!/usr/bin/env python3
"""Check a real export: do the Python library and the Rust engine agree?

    python scripts/compare.py ZALTVALA.QUERY.zip
    python scripts/compare.py DATA.1.BIN --schema DATA.0.TXT
    python scripts/compare.py BSIS.QUERY/            # an unzipped export (Rust only)

Converts the export with both implementations into a temporary directory,
compares the CSV byte for byte and the Parquet value for value, reports
timings, and deletes everything it wrote. It never prints a field value:
on a mismatch it reports the line number and the names of the columns that
differ, so the output is safe to paste into an issue.

The Rust binary is taken from --rust, $SAP_BIN_RUST, target/release/sap-bin,
or the PATH, in that order.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))

from sap_bin_parser.cli import main as python_cli  # noqa: E402


def find_rust(explicit: str | None) -> Path:
    for candidate in (
        explicit,
        os.environ.get("SAP_BIN_RUST"),
        ROOT / "target" / "release" / "sap-bin",
    ):
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    found = shutil.which("sap-bin")
    if not found:
        sys.exit(
            "error: the Rust sap-bin binary was not found; build it with `cargo build --release`"
        )
    return Path(found)


def digest(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()[:16]


def first_difference(a: Path, b: Path) -> str:
    with a.open(newline="", encoding="utf-8") as fa, b.open(newline="", encoding="utf-8") as fb:
        ra, rb = csv.reader(fa), csv.reader(fb)
        header = next(ra, [])
        next(rb, [])
        for number, (row_a, row_b) in enumerate(zip(ra, rb, strict=False), start=2):
            if row_a != row_b:
                columns = [
                    header[i] if i < len(header) else f"#{i}"
                    for i in range(max(len(row_a), len(row_b)))
                    if (row_a[i : i + 1] or [None]) != (row_b[i : i + 1] or [None])
                ]
                return f"line {number}, column(s): {', '.join(columns)}"
    return "one file is longer than the other"


def timed(label: str, action) -> float:
    started = time.perf_counter()
    code = action()
    seconds = time.perf_counter() - started
    if code not in (0, None):
        sys.exit(f"error: {label} failed with exit code {code}")
    return seconds


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("export", help="a delivered .zip, a .BIN, or an unzipped export folder")
    parser.add_argument("--schema", help="DATA.0.TXT, for a loose .BIN")
    parser.add_argument("--rust", help="path to the Rust sap-bin binary")
    parser.add_argument("--parquet", action="store_true", help="also compare Parquet output")
    args = parser.parse_args()

    rust = find_rust(args.rust)
    export = Path(args.export)
    folder = export.is_dir()
    extra = ["--schema", args.schema] if args.schema else []

    size = "folder" if folder else f"{export.stat().st_size / 1e6:,.1f} MB"
    print(f"export: {export.name}  ({size})")
    if not folder:
        subprocess.run([str(rust), "info", str(export), *extra], check=False)
    print()

    with tempfile.TemporaryDirectory(prefix="sap-bin-compare-") as tmp:
        work = Path(tmp)
        results = []
        formats = ["csv", "parquet"] if args.parquet else ["csv"]
        for fmt in formats:
            rust_out = work / f"rust.{fmt}"
            rust_command = [str(rust), "convert", str(export), "-o", str(rust_out), "-f", fmt]
            rust_seconds = timed(
                "rust",
                lambda command=rust_command: (
                    subprocess.run([*command, "--quiet", *extra], check=False).returncode
                ),
            )
            if folder:
                print(f"{fmt}: rust {rust_seconds:.2f}s (the Python library does not read folders)")
                continue
            python_out = work / f"python.{fmt}"
            python_seconds = timed(
                "python",
                lambda fmt=fmt, python_out=python_out: python_cli(
                    ["convert", str(export), "-o", str(python_out), "-f", fmt, *extra]
                ),
            )
            if fmt == "csv":
                same = (
                    python_out.read_bytes() == rust_out.read_bytes()
                    if python_out.stat().st_size < 2 << 30
                    else digest(python_out) == digest(rust_out)
                )
                detail = (
                    ""
                    if same
                    else f" — first difference at {first_difference(python_out, rust_out)}"
                )
            else:
                import pyarrow.parquet as pq

                a, b = pq.read_table(python_out), pq.read_table(rust_out)
                same = a.schema.types == b.schema.types and a.equals(b.cast(a.schema))
                detail = "" if same else " — the tables differ"
            results.append(same)
            print(
                f"{fmt}: {'IDENTICAL' if same else 'DIFFERENT'}{detail}\n"
                f"     python {python_seconds:.2f}s, rust {rust_seconds:.2f}s "
                f"({python_seconds / max(rust_seconds, 1e-9):.0f}x faster)"
            )
    print("\ntemporary files deleted.")
    return 0 if all(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
