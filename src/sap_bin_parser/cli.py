"""Command line interface: ``sap-bin-py``.

The Rust ``sap-bin`` binary has the same commands and flags, and is much faster;
this one is the reference implementation and needs nothing but Python.
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Iterator, Sequence
from pathlib import Path
from typing import Any

from . import __version__
from .archive import ArchiveError, SapArchive
from .decode import DecodeError
from .reader import BinReader, RecordError, probe_record_size
from .schema import Schema, SchemaError, load_schema
from .text import TextReader
from .writers import write_csv, write_parquet


def _eprint(message: str) -> None:
    print(message, file=sys.stderr)


def _resolve_schema(args: argparse.Namespace, archive: SapArchive | None) -> Schema:
    """Take the schema from ``--schema`` if given, else from the archive."""
    if getattr(args, "schema", None):
        return load_schema(args.schema)
    if archive is not None:
        return archive.schema()
    raise SchemaError(
        "no schema available: pass --schema DATA.0.TXT, or point at an archive "
        "that carries its own sidecar"
    )


def _make_reader(
    source: Any,
    schema: Schema,
    args: argparse.Namespace,
    *,
    is_text: bool,
) -> Any:
    """Pick the reader matching the shard's format."""
    strict = getattr(args, "on_error", "stop") == "stop"
    if is_text:
        return TextReader(
            source,
            schema,
            encoding=getattr(args, "text_encoding", None) or "windows-1251",
            decimal_as_float=getattr(args, "float_decimals", False),
            strict=strict,
        )
    return BinReader(
        source,
        schema,
        record_size=getattr(args, "record_size", None),
        strict=strict,
        decimal_as_float=getattr(args, "float_decimals", False),
    )


def _shard_is_text(shard: Any) -> bool:
    return shard.suffix == "TXT"


def _open_archive(path: Path) -> SapArchive | None:
    """Open ``path`` as an archive, or return None if it is a plain .BIN.

    A path that does not exist is reported as such here, rather than surfacing
    later as a confusing complaint about a missing schema.
    """
    if not path.exists():
        raise FileNotFoundError(2, "No such file or directory", str(path))
    if path.is_file() and path.suffix.lower() == ".zip":
        return SapArchive(path)
    return None


def cmd_info(args: argparse.Namespace) -> int:
    """Describe an export: its schema, geometry and shards."""
    path = Path(args.path)
    archive = _open_archive(path)
    try:
        schema = _resolve_schema(args, archive)

        print(f"schema: {schema.name or '(unnamed)'} — {len(schema)} fields")
        print(f"payload: {schema.payload_size} bytes")
        print(f"record:  {schema.record_size} bytes", end="")
        if schema.padding_size:
            print(f"  ({schema.padding_size} pad byte to even alignment)")
        else:
            print()

        for problem in schema.inconsistencies():
            _eprint(f"warning: {problem}")

        if args.fields:
            print()
            print(f"{'OFFSET':>7}  {'NAME':<24} {'TYPE':<5} {'LEN':>4} {'DEC':>4} {'BYTES':>6}")
            for field, offset in schema.offsets():
                print(
                    f"{offset:>7}  {field.name:<24} {field.type:<5} "
                    f"{field.length:>4} {field.decimals:>4} {field.size:>6}"
                )

        if archive is not None:
            shards = archive.data_shards()
            total = sum(s.size for s in shards)
            print()
            print(f"archive: {archive.table_name} — {len(shards)} data shard(s)")
            if shards:
                described = {
                    "bin": "fixed-width binary (.BIN)",
                    "text": "tab-separated text (.TXT)",
                }.get(archive.format, archive.format)
                print(f"format:  {described}")
                print(f"compressed: {total / 1e6:,.1f} MB")
        else:
            reader = BinReader(path, schema)
            geometry = reader.geometry()
            print()
            print(f"file:    {geometry.path.name}")
            print(f"size:    {geometry.file_size:,} bytes")
            print(f"records: {geometry.record_count:,}")
            if not geometry.is_clean:
                _eprint(
                    f"warning: {geometry.trailing_bytes} trailing byte(s) — "
                    f"file is not a whole multiple of {geometry.record_size}. "
                    "Try `sap-bin probe`."
                )
        return 0
    finally:
        if archive is not None:
            archive.close()


def cmd_probe(args: argparse.Namespace) -> int:
    """Rank candidate record sizes for a file whose geometry is in doubt."""
    path = Path(args.path)
    archive = _open_archive(path)
    try:
        schema = _resolve_schema(args, archive)
        if archive is not None:
            _eprint("probe needs a plain .BIN file, not an archive")
            return 2

        results = probe_record_size(path, schema, sample_records=args.sample)
        print(f"{'SIZE':>6}  {'CLEAN RECORDS':>14}  {'DIVIDES EVENLY':>15}")
        for size, clean, divides in results:
            marker = "  <- schema" if size == schema.record_size else ""
            print(f"{size:>6}  {clean:>14}  {str(divides):>15}{marker}")
        return 0
    finally:
        if archive is not None:
            archive.close()


def _iter_rows(reader: BinReader, *, limit: int | None, on_error: str) -> Iterator[dict[str, Any]]:
    """Yield rows, honouring ``--limit`` and ``--on-error``."""
    skipped = 0
    try:
        for index, row in enumerate(reader):
            if limit is not None and index >= limit:
                return
            yield row
    except (RecordError, DecodeError) as exc:
        if on_error == "stop":
            raise
        skipped += 1
        _eprint(f"warning: stopped early: {exc}")
    if skipped:
        _eprint(f"warning: {skipped} record(s) skipped")


def cmd_convert(args: argparse.Namespace) -> int:
    """Convert an export to CSV or Parquet."""
    path = Path(args.path)
    destination = Path(args.output)
    archive = _open_archive(path)

    try:
        schema = _resolve_schema(args, archive)
        writer = write_parquet if args.format == "parquet" else write_csv

        if archive is None:
            reader = _make_reader(path, schema, args, is_text=path.suffix.upper() == ".TXT")
            rows = _iter_rows(reader, limit=args.limit, on_error=args.on_error)
            stats = _write(writer, rows, destination, schema, args)
            print(f"{stats.rows:,} records -> {stats.path}")
            return 0

        shards = archive.data_shards()
        if not shards:
            _eprint(f"{path.name} contains no DATA.*.BIN shards")
            return 1

        if args.split:
            destination.mkdir(parents=True, exist_ok=True)
            suffix = "parquet" if args.format == "parquet" else "csv"
            total = 0
            for shard, stream in archive.iter_shard_streams():
                reader = _make_reader(stream, schema, args, is_text=_shard_is_text(shard))
                target = destination / f"{Path(shard.name).stem}.{suffix}"
                rows = _iter_rows(reader, limit=args.limit, on_error=args.on_error)
                stats = _write(writer, rows, target, schema, args)
                total += stats.rows
                print(f"{stats.rows:>10,} records -> {stats.path}")
            print(f"{total:,} records across {len(shards)} shard(s)")
            return 0

        def all_rows() -> Iterator[dict[str, Any]]:
            emitted = 0
            for shard, stream in archive.iter_shard_streams():
                reader = _make_reader(stream, schema, args, is_text=_shard_is_text(shard))
                remaining = None if args.limit is None else args.limit - emitted
                if remaining is not None and remaining <= 0:
                    return
                for row in _iter_rows(reader, limit=remaining, on_error=args.on_error):
                    yield row
                    emitted += 1

        stats = _write(writer, all_rows(), destination, schema, args)
        print(f"{stats.rows:,} records from {len(shards)} shard(s) -> {stats.path}")
        return 0
    finally:
        if archive is not None:
            archive.close()


def _write(writer, rows, destination: Path, schema: Schema, args: argparse.Namespace):
    if args.format == "parquet":
        return writer(
            rows,
            destination,
            schema,
            compression=args.compression,
            decimal_as_float=args.float_decimals,
        )
    return writer(rows, destination, schema, delimiter=args.delimiter, encoding=args.encoding)


def cmd_head(args: argparse.Namespace) -> int:
    """Print the first few records, to eyeball that a schema fits."""
    path = Path(args.path)
    archive = _open_archive(path)
    try:
        schema = _resolve_schema(args, archive)
        if archive is not None:
            shards = archive.data_shards()
            if not shards:
                _eprint("no data shards in archive")
                return 1
            source: Any = archive.open_shard(shards[0])
            is_text = _shard_is_text(shards[0])
        else:
            source = path
            is_text = path.suffix.upper() == ".TXT"

        args.on_error = "skip"  # be forgiving when just eyeballing records
        reader = _make_reader(source, schema, args, is_text=is_text)
        for index, row in enumerate(reader):
            if index >= args.count:
                break
            print(f"--- record {index} ---")
            width = max(len(name) for name in schema.field_names)
            for name, value in row.items():
                print(f"  {name:<{width}}  {value!r}")
        return 0
    finally:
        if archive is not None:
            archive.close()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="sap-bin-py",
        description="Parse SAP fixed-width binary table exports into CSV or Parquet.",
        epilog="Made by eidox ai.",
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    subparsers = parser.add_subparsers(dest="command", required=True)

    def add_common(sub: argparse.ArgumentParser) -> None:
        sub.add_argument("path", help="a .BIN file, or the delivered .zip archive")
        sub.add_argument(
            "--schema",
            help="schema sidecar (DATA.0.TXT); defaults to the one inside the archive",
        )

    info = subparsers.add_parser("info", help="describe an export's schema and geometry")
    add_common(info)
    info.add_argument("--fields", action="store_true", help="list every field with its offset")
    info.set_defaults(func=cmd_info)

    probe = subparsers.add_parser("probe", help="rank candidate record sizes for a .BIN")
    add_common(probe)
    probe.add_argument("--sample", type=int, default=200, help="records to test per candidate")
    probe.set_defaults(func=cmd_probe)

    head = subparsers.add_parser("head", help="print the first few decoded records")
    add_common(head)
    head.add_argument("-n", "--count", type=int, default=5, help="records to print")
    head.add_argument("--record-size", type=int, help="override the schema's record size")
    head.add_argument(
        "--text-encoding",
        default="windows-1251",
        help="encoding of .TXT shards (default: windows-1251)",
    )
    head.set_defaults(func=cmd_head)

    convert = subparsers.add_parser("convert", help="convert an export to CSV or Parquet")
    add_common(convert)
    convert.add_argument(
        "-o", "--output", required=True, help="output file, or directory with --split"
    )
    convert.add_argument("-f", "--format", choices=("csv", "parquet"), default="csv")
    convert.add_argument("--split", action="store_true", help="one output file per shard")
    convert.add_argument("--limit", type=int, help="stop after this many records")
    convert.add_argument("--record-size", type=int, help="override the schema's record size")
    convert.add_argument("--delimiter", default=",", help="CSV delimiter (default: ,)")
    convert.add_argument("--encoding", default="utf-8", help="CSV encoding (default: utf-8)")
    convert.add_argument("--compression", default="zstd", help="Parquet codec (default: zstd)")
    convert.add_argument(
        "--text-encoding",
        default="windows-1251",
        help="encoding of .TXT shards (default: windows-1251)",
    )
    convert.add_argument(
        "--float-decimals",
        action="store_true",
        help="emit packed decimals as float64 rather than exact decimals",
    )
    convert.add_argument(
        "--on-error",
        choices=("stop", "skip"),
        default="stop",
        help="stop at the first bad record, or warn and continue (default: stop)",
    )
    convert.set_defaults(func=cmd_convert)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except (SchemaError, ArchiveError) as exc:
        _eprint(f"error: {exc}")
        return 2
    except (RecordError, DecodeError) as exc:
        _eprint(f"error: {exc}")
        _eprint(
            "hint: if this is the first record, the record size is probably wrong "
            "— try `sap-bin probe`."
        )
        return 1
    except FileNotFoundError as exc:
        _eprint(f"error: no such file: {exc.filename}")
        return 2
    except ModuleNotFoundError as exc:
        # e.g. Parquet output requested without the optional pyarrow extra.
        _eprint(f"error: {exc}")
        return 2
    except BrokenPipeError:  # pragma: no cover - e.g. piping into `head`
        return 0
    except KeyboardInterrupt:  # pragma: no cover
        _eprint("interrupted")
        return 130


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
