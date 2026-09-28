"""Field-level decoding."""

from __future__ import annotations

from decimal import Decimal

import pytest

from sap_bin_parser.decode import (
    DecodeError,
    decode_date,
    decode_text,
    decode_time,
    pack_decimal,
    unpack_packed_decimal,
)


class TestPackedDecimal:
    @pytest.mark.parametrize(
        ("raw", "decimals", "expected"),
        [
            # 0.50 and 90.90 as a DMBTR(7,2) field: 13 digits plus a sign
            # nibble fill the 7 bytes, as the exporter writes them.
            (bytes.fromhex("0000000000050C"), 2, Decimal("0.50")),
            (bytes.fromhex("0000000009090C"), 2, Decimal("90.90")),
            (bytes.fromhex("000C"), 0, Decimal("0")),
            (bytes.fromhex("123C"), 0, Decimal("123")),
            (bytes.fromhex("123D"), 0, Decimal("-123")),
            (bytes.fromhex("123B"), 0, Decimal("-123")),
            # 0xA, 0xE and 0xF are all positive signs.
            (bytes.fromhex("123A"), 0, Decimal("123")),
            (bytes.fromhex("123E"), 0, Decimal("123")),
            (bytes.fromhex("123F"), 0, Decimal("123")),
        ],
    )
    def test_decodes_known_values(self, raw, decimals, expected):
        assert unpack_packed_decimal(raw, decimals) == expected

    def test_applies_implied_decimal_places(self):
        assert unpack_packed_decimal(bytes.fromhex("12345C"), 3) == Decimal("12.345")
        assert unpack_packed_decimal(bytes.fromhex("12345C"), 0) == Decimal("12345")

    def test_all_zero_bytes_are_null_not_garbage(self):
        # SAP writes an all-zero field for a null. The sign nibble is 0x0,
        # which is not a valid COMP-3 sign, so this must be special-cased.
        assert unpack_packed_decimal(b"\x00" * 7, 2) == Decimal("0")

    def test_rejects_invalid_sign_nibble(self):
        with pytest.raises(DecodeError, match="sign nibble"):
            unpack_packed_decimal(bytes.fromhex("1231"), 0)

    def test_rejects_non_decimal_nibble(self):
        # Valid sign nibble (0xC), but 0xA is not a decimal digit.
        with pytest.raises(DecodeError, match="non-decimal"):
            unpack_packed_decimal(bytes.fromhex("1A2C"), 0)

    def test_rejects_empty(self):
        with pytest.raises(DecodeError):
            unpack_packed_decimal(b"", 0)

    def test_preserves_precision_a_float_would_lose(self):
        # 0.07 has no exact float representation; Decimal keeps it exact.
        raw = pack_decimal(Decimal("0.07"), 7, 2)
        assert unpack_packed_decimal(raw, 2) == Decimal("0.07")

    @pytest.mark.parametrize(
        "value", ["0", "1", "-1", "0.50", "-1234.56", "99999.99", "-0.01"]
    )
    def test_round_trips(self, value):
        raw = pack_decimal(Decimal(value), 7, 2)
        assert unpack_packed_decimal(raw, 2) == Decimal(value)

    def test_pack_rejects_overflow(self):
        with pytest.raises(ValueError, match="digits"):
            pack_decimal(Decimal("1" * 20), 3, 0)


class TestText:
    def test_strips_utf16_padding(self):
        assert decode_text("0100".encode("utf-16-be")) == "0100"
        assert decode_text("PR        ".encode("utf-16-be")) == "PR"

    def test_blank_field_becomes_empty_string(self):
        assert decode_text(" ".encode("utf-16-be") * 10) == ""

    def test_decodes_non_ascii(self):
        assert decode_text("Пример".encode("utf-16-be")) == "Пример"

    def test_odd_length_is_an_error_not_silent_truncation(self):
        with pytest.raises(DecodeError, match="UTF-16BE"):
            decode_text(b"\x00")


class TestDateTime:
    def test_formats_date_as_iso(self):
        assert decode_date("20250616".encode("utf-16-be")) == "2025-06-16"

    @pytest.mark.parametrize("null", ["00000000", "        "])
    def test_null_dates_become_none(self, null):
        assert decode_date(null.encode("utf-16-be")) is None

    def test_formats_time_as_iso(self):
        assert decode_time("143005".encode("utf-16-be")) == "14:30:05"

    def test_blank_time_is_none(self):
        assert decode_time("      ".encode("utf-16-be")) is None

    def test_unexpected_shape_passes_through(self):
        # Better to surface an odd value than to silently drop it.
        assert decode_date("2025-06".encode("utf-16-be")) == "2025-06"
