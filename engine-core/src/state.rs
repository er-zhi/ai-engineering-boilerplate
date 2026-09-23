// Applies a node's output to execution state through one of four reducers.

use serde_json::{Map, Value};

use crate::graph::Reducer;

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
        Reducer::AppendEach => {
            let items = match value {
                Value::Array(items) => items,
                one => vec![one],
            };
            for item in items {
                apply_reducer(state, key, Reducer::Append, item);
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
    fn append_each_adds_every_item_of_a_list_as_its_own_entry() {
        let mut state = json!({"records": ["first"]});
        apply_reducer(
            &mut state,
            "records",
            Reducer::AppendEach,
            json!(["second", "third"]),
        );
        assert_eq!(
            state,
            json!({"records": ["first", "second", "third"]}),
            "several lookups made in one step are several entries, never one entry holding a list"
        );
    }

    #[test]
    fn append_each_adds_a_value_that_is_not_a_list_as_one_entry() {
        let mut state = json!({});
        apply_reducer(
            &mut state,
            "records",
            Reducer::AppendEach,
            json!({"one": 1}),
        );
        assert_eq!(state, json!({"records": [{"one": 1}]}));
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
