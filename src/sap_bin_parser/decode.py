"""Field-level decoding primitives for SAP flat-file exports.

Two encodings appear in these files:

* Character-ish fields (``C``, ``N``, ``D``, ``T``) are UTF-16 big-endian —
  two bytes per character, space padded on the right.
* Numeric fields (``P``) are packed decimal (COMP-3 / BCD): each byte carries
  two decimal digits, except the final byte, which carries one digit plus a
  sign nibble.
"""

from __future__ import annotations

from decimal import Decimal
from typing import Final

__all__ = [
    "DecodeError",
    "STRIP_CHARS",
    "unpack_packed_decimal",
    "pack_decimal",
    "decode_text",
    "decode_date",
    "decode_time",
]

# Packed-decimal sign nibbles. 0xB/0xD mean negative; 0xA/0xC/0xE are positive
# and 0xF is "unsigned", which SAP writes for quantity-like fields.
_NEGATIVE_SIGNS: Final[frozenset[int]] = frozenset({0x0B, 0x0D})
_VALID_SIGNS: Final[frozenset[int]] = frozenset({0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F})

# SAP writes an all-zero date as these, rather than leaving the field blank.
_NULL_DATES: Final[frozenset[str]] = frozenset({"00000000", "0000-00-00"})

# What counts as padding around a text field. Deliberately an explicit ASCII
# set rather than str.strip()'s Unicode notion of whitespace, so that every
# implementation of this format (this one, the Rust engine, anything else)
# agrees byte for byte on where a value ends.
STRIP_CHARS: Final[str] = "\x00\t\n\x0b\x0c\r "


def _is_ascii_digits(text: str) -> bool:
    return text.isascii() and text.isdigit()


class DecodeError(ValueError):
    """A field could not be decoded.

    Nearly always means the record size is wrong and fields are being read at
    the wrong offsets, rather than that the data itself is corrupt.
    """


def unpack_packed_decimal(data: bytes, decimals: int = 0) -> Decimal:
    """Decode a packed-decimal (COMP-3/BCD) field.

    ``decimals`` is the implied decimal-place count from the schema's ``DEC``
    column; the digits themselves carry no decimal point.

    A field of all zero bytes is treated as zero. That is not a valid COMP-3
    encoding (the sign nibble would be ``0x0``), but SAP emits it for nulls,
    and the notebook lineage this replaces silently produced garbage for it.
    The result carries the field's scale either way, so a null and a genuine
    zero both print as ``0.00`` in a two-decimal field.
    """
    if not data:
        raise DecodeError("packed decimal field is empty")

    # The hex rendering of a packed field *is* its digit string, with the sign
    # nibble as the last character. A valid field therefore has nothing but
    # decimal digits before it, which one str.isdigit() call checks in C.
    hexed = data.hex()
    digits, sign = hexed[:-1], hexed[-1]

    if sign == "0" and not any(data):
        return Decimal(0).scaleb(-decimals) if decimals else Decimal(0)

    sign_nibble = int(sign, 16)
    if sign_nibble not in _VALID_SIGNS:
        raise DecodeError(
            f"invalid packed-decimal sign nibble 0x{sign_nibble:X} in {hexed} "
            "(usually means the record size is wrong)"
        )
    if not digits.isdigit():
        raise DecodeError(
            f"non-decimal nibble in packed field {hexed} (usually means the record size is wrong)"
        )

    # Built from the string form rather than scaleb(), which would round a
    # 31-digit field to the context precision of 28. A negatively signed zero
    # is returned as plain zero.
    negative = sign_nibble in _NEGATIVE_SIGNS and bool(digits.strip("0"))
    return Decimal(f"{'-' if negative else ''}{digits}E-{decimals}")


def pack_decimal(value: Decimal | int | str, size: int, decimals: int = 0) -> bytes:
    """Encode a number as a packed-decimal field of ``size`` bytes.

    The inverse of :func:`unpack_packed_decimal`. Used to build test fixtures,
    and to round-trip a value when checking a schema against a real file.
    """
    if size < 1:
        raise ValueError("packed decimal size must be at least 1 byte")

    scaled = (Decimal(value) * (10**decimals)).to_integral_value()
    capacity = size * 2 - 1
    digits = str(abs(int(scaled)))
    if len(digits) > capacity:
        raise ValueError(
            f"{value} needs {len(digits)} digits but a {size}-byte field holds {capacity}"
        )

    nibbles = digits.zfill(capacity) + ("D" if scaled < 0 else "C")
    return bytes(int(nibbles[i : i + 2], 16) for i in range(0, len(nibbles), 2))


def decode_text(raw: bytes, *, errors: str = "strict") -> str:
    """Decode a UTF-16BE character field and strip its padding."""
    try:
        text = raw.decode("utf-16-be", errors=errors)
    except UnicodeDecodeError as exc:
        raise DecodeError(
            f"cannot decode {raw.hex()} as UTF-16BE (usually means the record size is wrong)"
        ) from exc
    return text.strip(STRIP_CHARS)


def decode_date(raw: bytes, *, errors: str = "strict") -> str | None:
    """Decode a ``D`` field (``YYYYMMDD``) to an ISO date, or ``None`` if null.

    Blank, all zeros (of any length) and ``0000-00-00`` are null. Eight ASCII
    digits become ``YYYY-MM-DD``. Anything else is passed through unchanged,
    so an odd value is surfaced rather than silently dropped.
    """
    text = decode_text(raw, errors=errors)
    if not text or text in _NULL_DATES or not text.strip("0"):
        return None
    if len(text) == 8 and _is_ascii_digits(text):
        return f"{text[0:4]}-{text[4:6]}-{text[6:8]}"
    return text


def decode_time(raw: bytes, *, errors: str = "strict") -> str | None:
    """Decode a ``T`` field (``HHMMSS``) to an ISO time, or ``None`` if blank.

    Unlike dates, ``000000`` is a real time (midnight), so only a blank field
    is null.
    """
    text = decode_text(raw, errors=errors)
    if not text:
        return None
    if len(text) == 6 and _is_ascii_digits(text):
        return f"{text[0:2]}:{text[2:4]}:{text[4:6]}"
    return text
