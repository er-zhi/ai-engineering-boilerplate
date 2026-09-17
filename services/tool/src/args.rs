// The arguments each system tool takes: the one declaration its JSON Schema and its reader share.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub const MIN_SEARCH_LIMIT: u8 = 1;
pub const DEFAULT_SEARCH_LIMIT: u8 = 5;
pub const MAX_SEARCH_LIMIT: u8 = 20;
pub const MAX_URLS: usize = 5;

pub fn input_schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| {
        debug_assert!(false, "a derived JSON Schema must serialize");
        Value::Object(serde_json::Map::new())
    })
}

pub fn parse<T: DeserializeOwned>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|error| error.to_string())
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub query: String,
    pub limit: Option<u8>,
}

impl Search {
    pub fn checked_limit(&self) -> Result<u8, String> {
        let Some(limit) = self.limit else {
            return Ok(DEFAULT_SEARCH_LIMIT);
        };
        if !(MIN_SEARCH_LIMIT..=MAX_SEARCH_LIMIT).contains(&limit) {
            return Err(format!(
                "limit must be between {MIN_SEARCH_LIMIT} and {MAX_SEARCH_LIMIT}, got {limit}"
            ));
        }
        Ok(limit)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    pub url: Option<String>,
    pub urls: Option<Vec<String>>,
}

impl Fetch {
    pub fn checked_urls(self) -> Result<Vec<String>, String> {
        let urls = self.urls.unwrap_or_else(|| self.url.into_iter().collect());
        if urls.is_empty() {
            return Err("url is required".to_owned());
        }
        if urls.len() > MAX_URLS {
            return Err(format!("at most {MAX_URLS} urls can be fetched at once"));
        }
        Ok(urls)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadDocument {
    pub source: String,
    pub source_id: String,
    pub version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_argument_the_type_does_not_declare_is_refused_by_name() {
        let error = parse::<Search>(&json!({"q": "nasdaq"})).expect_err("`q` is not a field");

        assert!(error.contains('q'), "{error}");
    }

    #[test]
    fn a_missing_required_argument_is_refused_by_name() {
        let error = parse::<Search>(&json!({"limit": 5})).expect_err("query is required");

        assert!(error.contains("query"), "{error}");
    }

    #[test]
    fn a_limit_outside_the_range_names_both_bounds() {
        let parsed = parse::<Search>(&json!({"query": "rust", "limit": 99})).expect("parses");
        let error = parsed.checked_limit().expect_err("99 is over the maximum");

        assert!(error.contains(&MAX_SEARCH_LIMIT.to_string()), "{error}");
        assert!(error.contains("99"), "{error}");
    }

    #[test]
    fn a_limit_that_is_not_a_number_is_refused_by_the_type() {
        parse::<Search>(&json!({"query": "rust", "limit": "five"})).expect_err("not a u8");
    }

    #[test]
    fn one_url_and_many_urls_are_both_accepted() {
        let one = parse::<Fetch>(&json!({"url": "https://example.com"}))
            .expect("parses")
            .checked_urls()
            .expect("one url");
        assert_eq!(one.len(), 1);

        let many = parse::<Fetch>(&json!({"urls": ["https://a.example", "https://b.example"]}))
            .expect("parses")
            .checked_urls()
            .expect("two urls");
        assert_eq!(many.len(), 2);
    }

    #[test]
    fn fetching_with_no_url_at_all_is_refused() {
        parse::<Fetch>(&json!({}))
            .expect("parses")
            .checked_urls()
            .expect_err("neither url nor urls");
    }

    #[test]
    fn more_urls_than_the_cap_are_refused_naming_it() {
        let too_many: Vec<String> = (0..=MAX_URLS)
            .map(|i| format!("https://{i}.example"))
            .collect();

        let error = parse::<Fetch>(&json!({"urls": too_many}))
            .expect("parses")
            .checked_urls()
            .expect_err("over the cap");

        assert!(error.contains(&MAX_URLS.to_string()), "{error}");
    }
}
