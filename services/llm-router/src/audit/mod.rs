// The llm_router schema: statistics that stay, payloads that expire, one pair of tables per class of model.

// All four tables grow with time rather than with entities, so all four are `PARTITION BY RANGE (created_at)`
// from their first version and their retention is a `DROP TABLE` of one child. Schema-sync cannot express a
// partitioned parent and is not inert against one, so none of these entities may sit under a registry glob:
// each carries its own literal `TABLE_STATEMENT`, run by `partition::create_parents` and kept by
// `partition::maintain`.

pub mod decision;
pub mod decision_payload;
pub mod payload;
pub mod request;

// Both statistics tables declare `model_used varchar(128)`, and the statistics and the payload of one call
// share a transaction: a longer name fails the insert and takes the whole audit record of that call with it,
// including the record of a failure the caller was still answered about. Every name is cut to the column
// here, wherever it came from — the provider's reply, TYPESAFE_AI_MODEL, or a tier's own slug — so no writer
// has to remember to. Keep in step with the varchar widths in request.rs and decision.rs.
pub const MODEL_NAME_BYTES: usize = 128;

pub fn bounded_model_name(mut name: String) -> String {
    if name.len() > MODEL_NAME_BYTES {
        let cut = (0..=MODEL_NAME_BYTES)
            .rev()
            .find(|at| name.is_char_boundary(*at))
            .unwrap_or_default();
        name.truncate(cut);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_the_column_holds_is_left_alone() {
        assert_eq!(bounded_model_name("jev-1.13.0".to_owned()), "jev-1.13.0");
    }

    #[test]
    fn a_name_longer_than_the_column_is_cut_on_a_character_boundary() {
        let cut = bounded_model_name("é".repeat(MODEL_NAME_BYTES));

        assert!(cut.len() <= MODEL_NAME_BYTES);
        assert_eq!(cut, "é".repeat(MODEL_NAME_BYTES / 2));
    }
}
