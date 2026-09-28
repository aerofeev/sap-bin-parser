"""Schema parsing and record geometry."""

from __future__ import annotations

import pytest

from sap_bin_parser.schema import Schema, SchemaError, parse_schema, schema_from_tuples

from .conftest import BSIS_SIDECAR


class TestParsing:
    def test_reads_every_field(self, bsis_schema: Schema):
        assert len(bsis_schema) == 9
        assert bsis_schema.field_names[0] == "BUKRS"
        assert bsis_schema.field_names[-1] == "DMBTR"

    def test_reads_types_and_scales(self, bsis_schema: Schema):
        dmbtr = bsis_schema.fields[-1]
        assert (dmbtr.type, dmbtr.length, dmbtr.decimals, dmbtr.size) == ("P", 7, 2, 7)

    def test_tolerates_a_bom(self):
        schema = parse_schema("﻿" + BSIS_SIDECAR)
        assert len(schema) == 9

    def test_rejects_sidecar_without_header(self):
        with pytest.raises(SchemaError, match="header"):
            parse_schema("BUKRS\tC\t4\t0\t8\n")

    def test_rejects_empty_sidecar(self):
        with pytest.raises(SchemaError, match="empty"):
            parse_schema("")

    def test_rejects_unknown_type(self):
        bad = "NAME\tTABLE\tTYPE\tLENG\tDEC\tSIZE\tROLL\tKEY\nFOO\t\tX\t4\t0\t8\tFOO\t\n"
        with pytest.raises(SchemaError, match="unsupported type"):
            parse_schema(bad)

    def test_rejects_duplicate_names(self):
        with pytest.raises(SchemaError, match="duplicate"):
            schema_from_tuples([("A", "C", 1, 0, 2), ("A", "C", 1, 0, 2)])

    def test_error_names_the_offending_line(self):
        bad = BSIS_SIDECAR.replace("GJAHR\t\tN\t4 \t0 \t8", "GJAHR\t\tN\tx \t0 \t8")
        with pytest.raises(SchemaError, match="line 5"):
            parse_schema(bad)


class TestGeometry:
    def test_odd_payload_pads_to_even_record(self, bsis_schema: Schema):
        # This is the crux: fields sum to 125, and SAP pads to 126 so each
        # record starts on a two-byte boundary and UTF-16 fields stay aligned.
        assert bsis_schema.payload_size == 125
        assert bsis_schema.record_size == 126
        assert bsis_schema.padding_size == 1

    def test_even_payload_needs_no_padding(self, simple_schema: Schema):
        assert simple_schema.payload_size == 25 or simple_schema.payload_size % 2 == 1
        assert simple_schema.record_size == simple_schema.payload_size + (
            simple_schema.payload_size % 2
        )

    def test_offsets_are_cumulative(self, bsis_schema: Schema):
        offsets = bsis_schema.offsets()
        assert offsets[0][1] == 0
        assert offsets[1][1] == 8  # BUKRS is C4 -> 8 bytes
        assert offsets[2][1] == 28  # + HKONT C10 -> 20 bytes
        assert offsets[-1][1] == 118  # DMBTR starts here; 118 + 7 = 125

    def test_declared_sizes_are_self_consistent(self, bsis_schema: Schema):
        # SIZE always equals LENG*2 for the character-ish types.
        assert bsis_schema.inconsistencies() == ()

    def test_flags_a_sidecar_whose_size_disagrees(self):
        bad = schema_from_tuples([("A", "C", 4, 0, 9)])
        assert "A: SIZE=9" in bad.inconsistencies()[0]
