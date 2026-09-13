// Validates crawler process configuration before any external connection is opened.

const DEFAULT_MAX_PAGES: u32 = 100;
const MAX_PAGES_CEILING: u32 = 10_000;

pub struct CrawlerConfig {
    max_pages: u32,
}

impl CrawlerConfig {
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let max_pages = parse_max_pages(std::env::var("CRAWL_MAX_PAGES"))?;
        require_spider_max_size(std::env::var("SPIDER_MAX_SIZE_BYTES"))?;
        Ok(Self { max_pages })
    }

    pub fn max_pages(&self) -> u32 {
        self.max_pages
    }
}

fn parse_max_pages(
    raw: Result<String, std::env::VarError>,
) -> Result<u32, Box<dyn std::error::Error>> {
    let max_pages = match raw {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|error| format!("CRAWL_MAX_PAGES: {error}"))?,
        Err(std::env::VarError::NotPresent) => DEFAULT_MAX_PAGES,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("CRAWL_MAX_PAGES is not valid Unicode".into());
        }
    };
    if max_pages == 0 || max_pages > MAX_PAGES_CEILING {
        return Err(format!("CRAWL_MAX_PAGES must be between 1 and {MAX_PAGES_CEILING}").into());
    }
    Ok(max_pages)
}

fn require_spider_max_size(
    raw: Result<String, std::env::VarError>,
) -> Result<(), Box<dyn std::error::Error>> {
    let raw = raw.map_err(|_| "SPIDER_MAX_SIZE_BYTES is not set to a valid Unicode value")?;
    let configured = raw
        .parse::<usize>()
        .map_err(|error| format!("SPIDER_MAX_SIZE_BYTES: {error}"))?;
    if configured != crate::links::MAX_PARSEABLE_HTML_BYTES {
        return Err(format!(
            "SPIDER_MAX_SIZE_BYTES must be {}",
            crate::links::MAX_PARSEABLE_HTML_BYTES
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spider_response_limit_must_match_the_html_memory_budget() {
        assert!(require_spider_max_size(Ok("5242880".to_owned())).is_ok());
        for invalid in ["", "not-a-number", "0", "5242881", "1073741824"] {
            assert!(
                require_spider_max_size(Ok(invalid.to_owned())).is_err(),
                "{invalid}"
            );
        }
        assert!(require_spider_max_size(Err(std::env::VarError::NotPresent)).is_err());
    }

    #[test]
    fn crawl_page_limit_defaults_and_rejects_invalid_values() {
        assert_eq!(
            parse_max_pages(Err(std::env::VarError::NotPresent)).unwrap(),
            DEFAULT_MAX_PAGES
        );
        assert_eq!(
            parse_max_pages(Ok(MAX_PAGES_CEILING.to_string())).unwrap(),
            MAX_PAGES_CEILING
        );
        for invalid in [
            String::new(),
            "not-a-number".to_owned(),
            "0".to_owned(),
            (MAX_PAGES_CEILING + 1).to_string(),
        ] {
            assert!(parse_max_pages(Ok(invalid.clone())).is_err(), "{invalid}");
        }
    }
}
