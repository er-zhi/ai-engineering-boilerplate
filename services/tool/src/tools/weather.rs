// weather: current conditions for a place, from keyless public APIs raced against each other
// (see `tools::race`). wttr.in answers a free-text location in one request; Open-Meteo needs a
// geocoding hop first but is the more reliable of the two, so neither is a strict fallback of the
// other — whichever answers first wins.
//
// Base URLs live on the struct so the tests below point them at a local axum fake instead of the
// real internet (the same shape `providers::you` uses for its own base URL).

use serde::Deserialize;
use serde::Serialize;
use url::Url;

use crate::tools::race;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Weather {
    pub location: String,
    pub temp_c: f64,
    pub temp_f: f64,
    pub conditions: String,
    pub humidity: Option<f64>,
    pub wind_kmh: Option<f64>,
    pub source: String,
    pub observed_at: String,
}

pub struct WeatherTool {
    client: reqwest::Client,
    wttr_base: String,
    geocode_base: String,
    forecast_base: String,
}

impl Default for WeatherTool {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

impl WeatherTool {
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            wttr_base: "https://wttr.in".to_owned(),
            geocode_base: "https://geocoding-api.open-meteo.com/v1/search".to_owned(),
            forecast_base: "https://api.open-meteo.com/v1/forecast".to_owned(),
        }
    }

    /// Races wttr.in against Open-Meteo and returns whichever answers first.
    pub async fn current(&self, location: &str) -> Result<Weather, String> {
        let location = location.trim();
        if location.is_empty() {
            return Err("location is required".to_owned());
        }
        let sources = vec![
            race::source("wttr.in", || self.via_wttr(location)),
            race::source("open-meteo", || self.via_open_meteo(location)),
        ];
        race::first_success(&sources).await
    }

    async fn via_wttr(&self, location: &str) -> Result<Weather, String> {
        let url = format!(
            "{}/{}?format=j1",
            self.wttr_base,
            utf8_percent_path(location)
        );
        let body: WttrResponse = get_json(&self.client, &url).await?;
        let current = body
            .current_condition
            .into_iter()
            .next()
            .ok_or_else(|| "wttr.in returned no current_condition".to_owned())?;
        let temp_c = parse_number(&current.temp_c).ok_or("wttr.in returned no temp_C")?;
        let temp_f = parse_number(&current.temp_f).unwrap_or_else(|| celsius_to_fahrenheit(temp_c));
        let name = body
            .nearest_area
            .into_iter()
            .next()
            .and_then(|area| area.area_name.into_iter().next())
            .map_or_else(|| location.to_owned(), |value| value.value);
        Ok(Weather {
            location: name,
            temp_c,
            temp_f,
            conditions: current
                .weather_desc
                .into_iter()
                .next()
                .map_or_else(|| "unknown".to_owned(), |d| d.value.trim().to_owned()),
            humidity: current.humidity.as_deref().and_then(parse_number),
            wind_kmh: current.windspeed_kmph.as_deref().and_then(parse_number),
            source: "wttr.in".to_owned(),
            observed_at: current
                .local_obs_date_time
                .or(current.observation_time)
                .unwrap_or_else(now_utc),
        })
    }

    async fn via_open_meteo(&self, location: &str) -> Result<Weather, String> {
        let place = self.geocode(location).await?;
        let mut url = Url::parse(&self.forecast_base).map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("latitude", &place.latitude.to_string())
            .append_pair("longitude", &place.longitude.to_string())
            .append_pair(
                "current",
                "temperature_2m,weather_code,wind_speed_10m,relative_humidity_2m",
            );
        let body: ForecastResponse = get_json(&self.client, url.as_str()).await?;
        let current = body
            .current
            .ok_or_else(|| "open-meteo returned no current block".to_owned())?;
        let temp_c = current
            .temperature_2m
            .ok_or("open-meteo returned no temperature_2m")?;
        Ok(Weather {
            location: place.name,
            temp_c,
            temp_f: celsius_to_fahrenheit(temp_c),
            conditions: weather_code_description(current.weather_code).to_owned(),
            humidity: current.relative_humidity_2m,
            wind_kmh: current.wind_speed_10m,
            source: "open-meteo".to_owned(),
            observed_at: current.time.unwrap_or_else(now_utc),
        })
    }

    async fn geocode(&self, location: &str) -> Result<Place, String> {
        let mut url = Url::parse(&self.geocode_base).map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("name", location)
            .append_pair("count", "1");
        let body: GeocodeResponse = get_json(&self.client, url.as_str()).await?;
        body.results
            .unwrap_or_default()
            .into_iter()
            .next()
            .ok_or_else(|| format!("open-meteo could not geocode {location:?}"))
    }
}

/// wttr.in takes the place name as a path segment; a space or accent there has to be escaped or
/// the request is a 404 (or, worse, a malformed URL reqwest refuses outright).
fn utf8_percent_path(location: &str) -> String {
    let mut out = String::with_capacity(location.len());
    for byte in location.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b',') {
            out.push(char::from(*byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        // wttr.in serves an ASCII-art page (not JSON) to clients it reads as a browser, and
        // rejects some default agents outright.
        .header("User-Agent", "ai-engineering-boilerplate-tool/0.1")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("returned {}", response.status()));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("unexpected response shape: {e}"))
}

fn parse_number(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok()
}

fn celsius_to_fahrenheit(celsius: f64) -> f64 {
    celsius * 9.0 / 5.0 + 32.0
}

fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// WMO weather interpretation codes, as documented by Open-Meteo, collapsed to the short phrase
/// a spoken answer wants.
pub fn weather_code_description(code: Option<i64>) -> &'static str {
    match code {
        Some(0) => "clear sky",
        Some(1) => "mainly clear",
        Some(2) => "partly cloudy",
        Some(3) => "overcast",
        Some(45 | 48) => "fog",
        Some(51 | 53 | 55) => "drizzle",
        Some(56 | 57) => "freezing drizzle",
        Some(61 | 63 | 65) => "rain",
        Some(66 | 67) => "freezing rain",
        Some(71 | 73 | 75 | 77) => "snow",
        Some(80..=82) => "rain showers",
        Some(85 | 86) => "snow showers",
        Some(95) => "thunderstorm",
        Some(96 | 99) => "thunderstorm with hail",
        _ => "unknown",
    }
}

#[derive(Deserialize)]
struct WttrResponse {
    #[serde(default)]
    current_condition: Vec<WttrCurrent>,
    #[serde(default)]
    nearest_area: Vec<WttrArea>,
}

#[derive(Deserialize)]
struct WttrCurrent {
    #[serde(rename = "temp_C")]
    temp_c: String,
    #[serde(rename = "temp_F", default)]
    temp_f: String,
    #[serde(rename = "weatherDesc", default)]
    weather_desc: Vec<WttrValue>,
    humidity: Option<String>,
    #[serde(rename = "windspeedKmph")]
    windspeed_kmph: Option<String>,
    #[serde(rename = "localObsDateTime")]
    local_obs_date_time: Option<String>,
    #[serde(rename = "observation_time")]
    observation_time: Option<String>,
}

#[derive(Deserialize)]
struct WttrValue {
    value: String,
}

#[derive(Deserialize)]
struct WttrArea {
    #[serde(rename = "areaName", default)]
    area_name: Vec<WttrValue>,
}

#[derive(Deserialize)]
struct GeocodeResponse {
    results: Option<Vec<Place>>,
}

#[derive(Deserialize)]
struct Place {
    name: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct ForecastResponse {
    current: Option<ForecastCurrent>,
}

#[derive(Deserialize)]
struct ForecastCurrent {
    time: Option<String>,
    temperature_2m: Option<f64>,
    weather_code: Option<i64>,
    wind_speed_10m: Option<f64>,
    relative_humidity_2m: Option<f64>,
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::routing::get;
    use serde_json::json;

    use super::*;

    async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
        format!("http://{address}")
    }

    fn wttr_body() -> serde_json::Value {
        json!({
            "current_condition": [{
                "temp_C": "17",
                "temp_F": "63",
                "weatherDesc": [{"value": "Partly cloudy "}],
                "humidity": "72",
                "windspeedKmph": "14",
                "localObsDateTime": "2026-09-16 10:30 AM",
            }],
            "nearest_area": [{"areaName": [{"value": "San Francisco"}]}],
        })
    }

    fn tool_pointing_at(wttr: &str, meteo: &str) -> WeatherTool {
        let mut tool = WeatherTool::new(reqwest::Client::new());
        tool.wttr_base = wttr.to_owned();
        tool.geocode_base = format!("{meteo}/v1/search");
        tool.forecast_base = format!("{meteo}/v1/forecast");
        tool
    }

    /// Open-Meteo pointed at a port nothing listens on: the race has to fall through to wttr.
    const DEAD: &str = "http://127.0.0.1:1";

    #[tokio::test]
    async fn wttr_json_is_mapped_onto_the_tool_output() {
        let wttr = serve(
            axum::Router::new().route("/San%20Francisco", get(|| async { Json(wttr_body()) })),
        )
        .await;
        let tool = tool_pointing_at(&wttr, DEAD);

        let weather = tool.current("San Francisco").await.expect("weather");

        assert_eq!(weather.location, "San Francisco");
        assert!((weather.temp_c - 17.0).abs() < f64::EPSILON);
        assert!((weather.temp_f - 63.0).abs() < f64::EPSILON);
        assert_eq!(weather.conditions, "Partly cloudy");
        assert_eq!(weather.humidity, Some(72.0));
        assert_eq!(weather.wind_kmh, Some(14.0));
        assert_eq!(weather.source, "wttr.in");
        assert_eq!(weather.observed_at, "2026-09-16 10:30 AM");
    }

    #[tokio::test]
    async fn open_meteo_geocodes_then_reads_the_current_block() {
        let meteo = serve(
            axum::Router::new()
                .route(
                    "/v1/search",
                    get(|| async {
                        Json(json!({"results": [
                            {"name": "Berlin", "latitude": 52.52, "longitude": 13.41}
                        ]}))
                    }),
                )
                .route(
                    "/v1/forecast",
                    get(|| async {
                        Json(json!({"current": {
                            "time": "2026-09-16T10:30",
                            "temperature_2m": 21.4,
                            "weather_code": 61,
                            "wind_speed_10m": 9.0,
                            "relative_humidity_2m": 55.0,
                        }}))
                    }),
                ),
        )
        .await;
        let tool = tool_pointing_at(DEAD, &meteo);

        let weather = tool.current("Berlin").await.expect("weather");

        assert_eq!(weather.location, "Berlin");
        assert!((weather.temp_c - 21.4).abs() < 1e-9);
        assert!((weather.temp_f - 70.52).abs() < 1e-9, "{}", weather.temp_f);
        assert_eq!(weather.conditions, "rain");
        assert_eq!(weather.source, "open-meteo");
    }

    /// The point of racing: wttr answering with a 500 must not cost the caller an answer when
    /// Open-Meteo is healthy.
    #[tokio::test]
    async fn source_a_failing_lets_source_b_win() {
        let wttr = serve(axum::Router::new().fallback(get(|| async {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        })))
        .await;
        let meteo = serve(
            axum::Router::new()
                .route(
                    "/v1/search",
                    get(|| async {
                        Json(json!({"results": [
                            {"name": "Paris", "latitude": 48.85, "longitude": 2.35}
                        ]}))
                    }),
                )
                .route(
                    "/v1/forecast",
                    get(|| async {
                        Json(json!({"current": {"temperature_2m": 12.0, "weather_code": 3}}))
                    }),
                ),
        )
        .await;
        let tool = tool_pointing_at(&wttr, &meteo);

        let weather = tool.current("Paris").await.expect("weather");

        assert_eq!(weather.source, "open-meteo");
        assert_eq!(weather.conditions, "overcast");
    }

    #[tokio::test]
    async fn every_source_failing_reports_both() {
        let tool = tool_pointing_at(DEAD, DEAD);

        let error = tool.current("Nowhere").await.unwrap_err();

        assert!(error.contains("wttr.in"), "{error}");
        assert!(error.contains("open-meteo"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_location_is_rejected_before_any_request() {
        assert!(WeatherTool::default().current("  ").await.is_err());
    }

    #[test]
    fn unknown_weather_codes_degrade_to_a_word_rather_than_a_number() {
        assert_eq!(weather_code_description(Some(0)), "clear sky");
        assert_eq!(weather_code_description(Some(1234)), "unknown");
        assert_eq!(weather_code_description(None), "unknown");
    }
}
