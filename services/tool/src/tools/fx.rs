// fx_rate: a current foreign-exchange rate from two keyless public APIs raced against each other
// (see `tools::race`). Frankfurter publishes ECB reference rates; open.er-api.com publishes its
// own daily set — either is good enough for "what's USD to EUR", and racing them means a rate
// limit or an outage on one is invisible to the caller.

use serde::Deserialize;
use serde::Serialize;

use crate::tools::race;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FxRate {
    pub base: String,
    pub quote: String,
    pub rate: f64,
    pub amount: Option<f64>,
    pub converted: Option<f64>,
    pub date: String,
    pub source: String,
}

pub struct FxTool {
    client: reqwest::Client,
    frankfurter_base: String,
    er_api_base: String,
}

impl Default for FxTool {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

impl FxTool {
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            frankfurter_base: "https://api.frankfurter.app".to_owned(),
            er_api_base: "https://open.er-api.com/v6".to_owned(),
        }
    }

    /// Races both providers for `base`->`quote`; `amount`, when given, is multiplied into
    /// `converted` so the caller does not have to do arithmetic itself.
    pub async fn rate(
        &self,
        base: &str,
        quote: &str,
        amount: Option<f64>,
    ) -> Result<FxRate, String> {
        let base = normalise_code(base)?;
        let quote = normalise_code(quote)?;
        let sources = vec![
            race::source("frankfurter", || self.via_frankfurter(&base, &quote)),
            race::source("open.er-api.com", || self.via_er_api(&base, &quote)),
        ];
        let mut rate = race::first_success(&sources).await?;
        rate.amount = amount;
        rate.converted = amount.map(|a| a * rate.rate);
        Ok(rate)
    }

    async fn via_frankfurter(&self, base: &str, quote: &str) -> Result<FxRate, String> {
        let url = format!("{}/latest?from={base}&to={quote}", self.frankfurter_base);
        let body: LatestResponse = get_json(&self.client, &url).await?;
        let rate = pick_rate(&body.rates, quote, "frankfurter")?;
        Ok(FxRate {
            base: base.to_owned(),
            quote: quote.to_owned(),
            rate,
            amount: None,
            converted: None,
            date: body.date.unwrap_or_else(today),
            source: "frankfurter".to_owned(),
        })
    }

    async fn via_er_api(&self, base: &str, quote: &str) -> Result<FxRate, String> {
        let url = format!("{}/latest/{base}", self.er_api_base);
        let body: ErApiResponse = get_json(&self.client, &url).await?;
        if body.result.as_deref().is_some_and(|r| r != "success") {
            return Err(format!(
                "returned result={:?}",
                body.result.unwrap_or_default()
            ));
        }
        let rate = pick_rate(&body.rates, quote, "open.er-api.com")?;
        Ok(FxRate {
            base: base.to_owned(),
            quote: quote.to_owned(),
            rate,
            amount: None,
            converted: None,
            date: body.time_last_update_utc.unwrap_or_else(today),
            source: "open.er-api.com".to_owned(),
        })
    }
}

fn pick_rate(
    rates: &Option<std::collections::HashMap<String, f64>>,
    quote: &str,
    who: &str,
) -> Result<f64, String> {
    rates
        .as_ref()
        .and_then(|rates| rates.get(quote).copied())
        .ok_or_else(|| format!("{who} did not quote {quote}"))
}

/// Currency codes are ISO-4217 three-letter symbols; anything else is a caller mistake worth
/// reporting before spending a request on it.
fn normalise_code(raw: &str) -> Result<String, String> {
    let code = raw.trim().to_ascii_uppercase();
    if code.len() == 3 && code.chars().all(|c| c.is_ascii_uppercase()) {
        Ok(code)
    } else {
        Err(format!(
            "{raw:?} is not a three-letter currency code like USD or EUR"
        ))
    }
}

fn today() -> String {
    chrono::Utc::now().date_naive().to_string()
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("returned {}", response.status()));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("unexpected response shape: {e}"))
}

#[derive(Deserialize)]
struct LatestResponse {
    date: Option<String>,
    rates: Option<std::collections::HashMap<String, f64>>,
}

#[derive(Deserialize)]
struct ErApiResponse {
    result: Option<String>,
    time_last_update_utc: Option<String>,
    rates: Option<std::collections::HashMap<String, f64>>,
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

    const DEAD: &str = "http://127.0.0.1:1";

    fn tool_pointing_at(frankfurter: &str, er_api: &str) -> FxTool {
        let mut tool = FxTool::new(reqwest::Client::new());
        tool.frankfurter_base = frankfurter.to_owned();
        tool.er_api_base = er_api.to_owned();
        tool
    }

    #[tokio::test]
    async fn frankfurter_rates_are_mapped_and_the_amount_converted() {
        let frankfurter = serve(axum::Router::new().route(
            "/latest",
            get(|| async { Json(json!({"amount": 1.0, "base": "USD", "date": "2026-09-15", "rates": {"EUR": 0.9}})) }),
        ))
        .await;
        let tool = tool_pointing_at(&frankfurter, DEAD);

        let fx = tool.rate("usd", "eur", Some(250.0)).await.expect("rate");

        assert_eq!((fx.base, fx.quote), ("USD".to_owned(), "EUR".to_owned()));
        assert!((fx.rate - 0.9).abs() < 1e-9);
        assert_eq!(fx.amount, Some(250.0));
        assert!(fx.converted.is_some_and(|c| (c - 225.0).abs() < 1e-9));
        assert_eq!(fx.date, "2026-09-15");
        assert_eq!(fx.source, "frankfurter");
    }

    #[tokio::test]
    async fn er_api_wins_when_frankfurter_is_down() {
        let frankfurter = serve(
            axum::Router::new()
                .fallback(get(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS })),
        )
        .await;
        let er_api = serve(axum::Router::new().route(
            "/latest/USD",
            get(|| async {
                Json(json!({
                    "result": "success",
                    "time_last_update_utc": "Tue, 15 Sep 2026 00:02:31 +0000",
                    "rates": {"EUR": 0.91, "GBP": 0.78},
                }))
            }),
        ))
        .await;
        let tool = tool_pointing_at(&frankfurter, &er_api);

        let fx = tool.rate("USD", "EUR", None).await.expect("rate");

        assert!((fx.rate - 0.91).abs() < 1e-9);
        assert_eq!(fx.source, "open.er-api.com");
        assert_eq!(fx.converted, None);
    }

    #[tokio::test]
    async fn a_provider_that_does_not_quote_the_currency_is_an_error_not_a_zero() {
        let frankfurter = serve(axum::Router::new().route(
            "/latest",
            get(|| async { Json(json!({"date": "2026-09-15", "rates": {"GBP": 0.78}})) }),
        ))
        .await;
        let tool = tool_pointing_at(&frankfurter, DEAD);

        let error = tool.rate("USD", "EUR", None).await.unwrap_err();

        assert!(error.contains("did not quote EUR"), "{error}");
    }

    #[tokio::test]
    async fn a_nonsense_currency_code_is_rejected_before_any_request() {
        let error = FxTool::default()
            .rate("dollars", "EUR", None)
            .await
            .unwrap_err();
        assert!(error.contains("three-letter"), "{error}");
    }
}
