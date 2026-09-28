"""Read SAP exports straight out of their delivered zip archives.

An export arrives as a zip of zips::

    BSIS.QUERY.zip
      BSIS.QUERY/DATA.0.zip   -> DATA.0.TXT    the schema sidecar
      BSIS.QUERY/DATA.1.zip   -> DATA.1.BIN    a shard of records
      BSIS.QUERY/DATA.2.zip   -> DATA.2.BIN
      ...

Unpacking one costs tens of gigabytes on disk, so everything here reads through
the nesting in memory instead.
"""

from __future__ import annotations

import io
import re
import zipfile
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

from .schema import Schema, parse_schema

__all__ = ["ArchiveError", "Shard", "SapArchive"]

_SHARD_PATTERN = re.compile(r"DATA\.(\d+)\.(BIN|TXT)$", re.IGNORECASE)

# Enough of a zip local file header to recover the stored filename.
_LOCAL_HEADER_SIGNATURE = b"PK\x03\x04"
_LOCAL_HEADER_SIZE = 30
_UTF8_FLAG = 0x800


class ArchiveError(ValueError):
    """The archive did not have the expected shape."""


@dataclass(frozen=True, slots=True)
class Shard:
    """One data member of an export archive."""

    index: int
    name: str
    member: str
    size: int
    is_schema: bool

    @property
    def suffix(self) -> str:
        return Path(self.name).suffix.lstrip(".").upper()


class SapArchive:
    """Navigate a delivered SAP export archive without unpacking it.

    >>> with SapArchive("BSIS.QUERY.zip") as archive:   # doctest: +SKIP
    ...     schema = archive.schema()
    ...     for shard in archive.shards():
    ...         data = archive.read_shard(shard)
    """

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        try:
            self._zip = zipfile.ZipFile(self.path)
        except zipfile.BadZipFile as exc:
            raise ArchiveError(f"{self.path} is not a readable zip archive") from exc
        self._names: dict[str, str | None] = {}

    def __enter__(self) -> SapArchive:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()

    def close(self) -> None:
        self._zip.close()
        self._names.clear()

    def shards(self) -> list[Shard]:
        """Every ``DATA.N`` member, ordered by index.

        Shard 0 is always the schema sidecar. Shards from 1 up are data, in
        whichever format this export used — see :class:`Shard.format`.
        """
        found: list[Shard] = []
        for info in self._zip.infolist():
            if info.is_dir():
                continue
            inner = self._inner_name(info.filename)
            match = _SHARD_PATTERN.search(inner or info.filename)
            if not match:
                continue
            index = int(match.group(1))
            found.append(
                Shard(
                    index=index,
                    name=inner or Path(info.filename).name,
                    member=info.filename,
                    size=info.file_size,
                    # Only shard 0 is the sidecar. A DATA.N.TXT for N >= 1 is
                    # tab-separated data, not a schema.
                    is_schema=index == 0,
                )
            )
        found.sort(key=lambda s: s.index)
        return found

    def data_shards(self) -> list[Shard]:
        """The record-bearing shards, without the schema sidecar."""
        return [s for s in self.shards() if not s.is_schema]

    @property
    def format(self) -> str:
        """The export's data format: ``"bin"``, ``"text"``, or ``"empty"``.

        The same table is delivered either way — fixed-width binary, or
        tab-separated text with a header row — under an identical sidecar.
        """
        suffixes = {s.suffix for s in self.data_shards()}
        if not suffixes:
            return "empty"
        if suffixes == {"BIN"}:
            return "bin"
        if suffixes == {"TXT"}:
            return "text"
        raise ArchiveError(
            f"{self.path.name} mixes shard formats ({', '.join(sorted(suffixes))}); "
            "expected all .BIN or all .TXT"
        )

    def schema(self) -> Schema:
        """Parse the export's own schema sidecar.

        Raises :class:`ArchiveError` if the archive carries no sidecar, in
        which case the schema has to be supplied from elsewhere.
        """
        for shard in self.shards():
            if shard.is_schema:
                text = self.read_shard(shard).decode("utf-8-sig", errors="replace")
                return parse_schema(text, name=self.table_name)
        raise ArchiveError(
            f"{self.path.name} contains no DATA.*.TXT schema sidecar; "
            "supply a schema explicitly"
        )

    @property
    def table_name(self) -> str:
        """The table name, from the archive's top-level directory or filename."""
        for info in self._zip.infolist():
            head = info.filename.split("/")[0]
            if head:
                return head.replace(".QUERY", "").strip()
        return self.path.stem

    def read_shard(self, shard: Shard) -> bytes:
        """Read one shard's bytes, transparently opening the inner zip."""
        raw = self._zip.read(shard.member)
        if not shard.member.lower().endswith(".zip"):
            return raw
        with zipfile.ZipFile(io.BytesIO(raw)) as inner:
            names = inner.namelist()
            if not names:
                raise ArchiveError(f"{shard.member} is an empty zip")
            return inner.read(names[0])

    def open_shard(self, shard: Shard) -> io.BytesIO:
        """Open one shard as a binary stream."""
        return io.BytesIO(self.read_shard(shard))

    def iter_shard_streams(self) -> Iterator[tuple[Shard, io.BytesIO]]:
        """Yield each data shard with its stream, in index order."""
        for shard in self.data_shards():
            yield shard, self.open_shard(shard)

    def _inner_name(self, member: str) -> str | None:
        """The name of the single file inside a nested ``DATA.N.zip``.

        Reads only the inner zip's first local file header rather than
        decompressing the member, so listing a 344-shard archive stays cheap.
        """
        if not member.lower().endswith(".zip"):
            return None
        if member in self._names:
            return self._names[member]

        name: str | None = None
        try:
            with self._zip.open(member) as handle:
                header = handle.read(_LOCAL_HEADER_SIZE)
            if header[:4] == _LOCAL_HEADER_SIGNATURE:
                name_length = int.from_bytes(header[26:28], "little")
                flags = int.from_bytes(header[6:8], "little")
                with self._zip.open(member) as handle:
                    raw_name = handle.read(_LOCAL_HEADER_SIZE + name_length)[
                        _LOCAL_HEADER_SIZE : _LOCAL_HEADER_SIZE + name_length
                    ]
                encoding = "utf-8" if flags & _UTF8_FLAG else "cp437"
                name = raw_name.decode(encoding, errors="replace") or None
        except (zipfile.BadZipFile, KeyError, OSError):
            name = None

        self._names[member] = name
        return name
