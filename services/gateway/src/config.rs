// Validates Gateway environment configuration before startup performs I/O.

use std::path::PathBuf;
use std::time::Duration;

use axum::http::Uri;

const DEFAULT_CRAWLER_URL: &str = "http://127.0.0.1:8081";
const DEFAULT_KNOWLEDGE_BASE_URL: &str = "http://127.0.0.1:8084";
const DEFAULT_FRONTEND_DIR: &str = "/frontend";
const DEFAULT_SESSION_TTL_HOURS: u64 = 24;
const MAX_SESSION_TTL_HOURS: u64 = 24 * 365;
const MAX_ENDPOINT_URL_CHARS: usize = 2_048;
const MAX_DATABASE_URL_CHARS: usize = 4_096;
const MAX_FRONTEND_PATH_CHARS: usize = 4_096;
const MAX_AUTH_PASSWORD_CHARS: usize = 1_024;

pub struct GatewayConfig {
    crawler_url: Uri,
    knowledge_base_url: Uri,
    frontend_dir: PathBuf,
    database_url: String,
    password: String,
    session_ttl: Duration,
}

impl GatewayConfig {
    pub fn crawler_url(&self) -> &Uri {
        &self.crawler_url
    }

    pub fn knowledge_base_url(&self) -> &Uri {
        &self.knowledge_base_url
    }

    pub fn frontend_dir(&self) -> &PathBuf {
        &self.frontend_dir
    }

    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn session_ttl(&self) -> Duration {
        self.session_ttl
    }

    #[cfg(test)]
    pub fn for_test(frontend_dir: PathBuf) -> Self {
        Self {
            crawler_url: Uri::from_static(DEFAULT_CRAWLER_URL),
            knowledge_base_url: Uri::from_static(DEFAULT_KNOWLEDGE_BASE_URL),
            frontend_dir,
            database_url: "unused in route tests".to_owned(),
            password: "correct horse battery staple".to_owned(),
            session_ttl: Duration::from_secs(60),
        }
    }
}

pub fn gateway_config() -> Result<GatewayConfig, Box<dyn std::error::Error>> {
    let crawler_url = endpoint(
        "CRAWLER_URL",
        read_or_default("CRAWLER_URL", DEFAULT_CRAWLER_URL)?,
    )?;
    let knowledge_base_url = endpoint(
        "KNOWLEDGE_BASE_URL",
        read_or_default("KNOWLEDGE_BASE_URL", DEFAULT_KNOWLEDGE_BASE_URL)?,
    )?;
    let frontend_dir = bounded(
        "FRONTEND_DIST_DIR",
        read_or_default("FRONTEND_DIST_DIR", DEFAULT_FRONTEND_DIR)?,
        MAX_FRONTEND_PATH_CHARS,
    )?;
    let database_url = bounded(
        "DATABASE_URL",
        read_required("DATABASE_URL")?,
        MAX_DATABASE_URL_CHARS,
    )?;
    validate_database_url(&database_url)?;
    let password = bounded(
        "GATEWAY_AUTH_PASSWORD",
        read_required("GATEWAY_AUTH_PASSWORD")?,
        MAX_AUTH_PASSWORD_CHARS,
    )?;
    let session_ttl = session_ttl_from(read_optional("GATEWAY_SESSION_TTL_HOURS")?)?;

    Ok(GatewayConfig {
        crawler_url,
        knowledge_base_url,
        frontend_dir: PathBuf::from(frontend_dir),
        database_url,
        password,
        session_ttl,
    })
}

fn read_required(name: &str) -> Result<String, String> {
    read_optional(name)?.ok_or_else(|| format!("{name} is not set"))
}

fn read_or_default(name: &str, default: &str) -> Result<String, String> {
    Ok(read_optional(name)?.unwrap_or_else(|| default.to_owned()))
}

fn read_optional(name: &str) -> Result<Option<String>, String> {
    optional_value(name, std::env::var(name))
}

fn optional_value(
    name: &str,
    value: Result<String, std::env::VarError>,
) -> Result<Option<String>, String> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} contains non-Unicode data")),
    }
}

fn bounded(name: &str, value: String, max_chars: usize) -> Result<String, String> {
    let length = value.chars().count();
    if length == 0 || length > max_chars {
        return Err(format!(
            "{name} must contain between 1 and {max_chars} characters"
        ));
    }
    Ok(value)
}

fn endpoint(name: &str, value: String) -> Result<Uri, String> {
    let value = bounded(name, value, MAX_ENDPOINT_URL_CHARS)?;
    let uri = value
        .parse::<Uri>()
        .map_err(|error| format!("{name} is not a valid URI: {error}"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.authority().is_none() {
        return Err(format!("{name} must be an absolute HTTP or HTTPS URI"));
    }
    Ok(uri)
}

fn validate_database_url(value: &str) -> Result<(), String> {
    let parsed = url::Url::parse(value).map_err(|error| format!("DATABASE_URL: {error}"))?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql") || parsed.host_str().is_none() {
        return Err("DATABASE_URL must be an absolute postgres or postgresql URL".to_owned());
    }
    Ok(())
}

fn session_ttl_from(value: Option<String>) -> Result<Duration, String> {
    let hours = match value {
        Some(value) => value
            .parse::<u64>()
            .map_err(|error| format!("GATEWAY_SESSION_TTL_HOURS: {error}"))?,
        None => DEFAULT_SESSION_TTL_HOURS,
    };
    if hours == 0 || hours > MAX_SESSION_TTL_HOURS {
        return Err(format!(
            "GATEWAY_SESSION_TTL_HOURS must be between 1 and {MAX_SESSION_TTL_HOURS}"
        ));
    }
    Ok(Duration::from_secs(hours * 3_600))
}

#[cfg(test)]
mod tests {
    include!("config_tests.rs");
}
