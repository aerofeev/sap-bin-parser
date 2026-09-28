//! Rank candidate record sizes for a file whose geometry is in doubt.
//!
//! The schema's own record size is normally right. This is for exports
//! where it is not, and replaces adjusting a hardcoded size by hand until the
//! output stops looking wrong: each candidate is scored by how many records
//! decode cleanly from the start of the data, and whether the data divides
//! into whole records.

use std::sync::Arc;

use serde::Serialize;

use crate::decode::{DecimalMode, Decoder};
use crate::schema::Schema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub record_size: usize,
    /// Records that decode cleanly, in a row, from the start of the sample.
    pub clean_records: usize,
    /// Whether the full data size is a whole multiple of this size; unknown
    /// when only a prefix of the data is at hand.
    pub divides_evenly: Option<bool>,
}

/// Score `payload .. payload + 8` against the first `sample_records`
/// records of `sample`, best first. `total_size` is the full size of the
/// data the sample was taken from, if known.
pub fn probe(
    sample: &[u8],
    schema: &Arc<Schema>,
    total_size: Option<u64>,
    sample_records: usize,
) -> Vec<Candidate> {
    let payload = schema.payload_size();
    let mut results: Vec<Candidate> = (payload..payload + 8)
        .map(|size| {
            let decoder = Decoder::new(schema.clone(), Some(size), DecimalMode::Exact, true)
                .expect("candidate sizes are at least the payload");
            let usable = (sample.len() / size).min(sample_records);
            let clean = match decoder.decode(&sample[..usable * size], 0) {
                Ok(_) => usable,
                Err(failure) => failure.row,
            };
            Candidate {
                record_size: size,
                clean_records: clean,
                divides_evenly: total_size.map(|t| t % size as u64 == 0),
            }
        })
        .collect();
    results
        .sort_by_key(|c| std::cmp::Reverse((c.divides_evenly.unwrap_or(false), c.clean_records)));
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{encode_bsis_records, BSIS_SIDECAR};

    #[test]
    fn ranks_the_true_record_size_first() {
        let schema = Arc::new(Schema::parse(BSIS_SIDECAR, None).unwrap());
        let mut data = Vec::new();
        encode_bsis_records(300, 1, &mut data);
        let ranked = probe(&data, &schema, Some(data.len() as u64), 200);
        assert_eq!(ranked[0].record_size, 126);
        assert_eq!(ranked[0].clean_records, 200);
        assert_eq!(ranked[0].divides_evenly, Some(true));
    }
}
