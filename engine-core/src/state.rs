// Applying a node's output to execution state: three reducers (Replace/Append/Merge) over
// top-level keys, matching the spec's "State" section. Parallel branches from the same
// super-step never race here — step() applies every output sequentially within one call.

use serde_json::{Map, Value};

use crate::graph::Reducer;

/// Writes `value` into `state[key]` according to `reducer`. `Append` requires the existing value
/// at `key` (if any) to already be an array — non-array existing values are replaced with a new
/// single-element array rather than silently dropped, so an Append reducer on a fresh key
/// behaves the same as on one already holding a list.
pub fn apply_reducer(state: &mut Value, key: &str, reducer: Reducer, value: Value) {
    let object = state
        .as_object_mut()
        .expect("execution state is always a JSON object");
    match reducer {
        Reducer::Replace => {
            object.insert(key.to_owned(), value);
        }
        Reducer::Append => {
            let entry = object
                .entry(key.to_owned())
                .or_insert_with(|| Value::Array(vec![]));
            match entry {
                Value::Array(items) => items.push(value),
                other => *other = Value::Array(vec![other.take(), value]),
            }
        }
        Reducer::Merge => {
            let entry = object
                .entry(key.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let (Value::Object(existing), Value::Object(incoming)) = (entry, value) {
                existing.extend(incoming);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replace_overwrites_the_key() {
        let mut state = json!({"answer": "old"});
        apply_reducer(&mut state, "answer", Reducer::Replace, json!("new"));
        assert_eq!(state, json!({"answer": "new"}));
    }

    #[test]
    fn append_grows_an_array_key() {
        let mut state = json!({"messages": ["hi"]});
        apply_reducer(&mut state, "messages", Reducer::Append, json!("there"));
        assert_eq!(state, json!({"messages": ["hi", "there"]}));
    }

    #[test]
    fn append_on_a_missing_key_starts_a_new_array() {
        let mut state = json!({});
        apply_reducer(&mut state, "messages", Reducer::Append, json!("hi"));
        assert_eq!(state, json!({"messages": ["hi"]}));
    }

    #[test]
    fn merge_combines_object_fields() {
        let mut state = json!({"usage": {"tokens": 10}});
        apply_reducer(
            &mut state,
            "usage",
            Reducer::Merge,
            json!({"tool_calls": 1}),
        );
        assert_eq!(state, json!({"usage": {"tokens": 10, "tool_calls": 1}}));
    }
}
