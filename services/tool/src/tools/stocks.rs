// stock_quote: the last price of a stock or index from two keyless public endpoints raced against
// each other (see `tools::race`). Stooq serves a one-row CSV and is generous with anonymous
// clients but uses its own symbol spelling; Yahoo's chart endpoint uses the familiar symbols and
// also carries the previous close (so a change % is only available from it), but rate-limits
// anonymous callers hard. Racing the two is what makes the pair usable without an API key.

use serde::Deserialize;
use serde::Serialize;

use crate::tools::race;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Quote {
    pub symbol: String,
    pub name: Option<String>,
    pub price: f64,
    pub previous_close: Option<f64>,
    pub change_pct: Option<f64>,
    pub currency: Option<String>,
    pub as_of: String,
    pub source: String,
}

pub struct StockTool {
    client: reqwest::Client,
    stooq_base: String,
    yahoo_base: String,
}

impl Default for StockTool {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

impl StockTool {
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            stooq_base: "https://stooq.com/q/l/".to_owned(),
            yahoo_base: "https://query1.finance.yahoo.com/v8/finance/chart".to_owned(),
        }
    }

    pub async fn quote(&self, symbol: &str) -> Result<Quote, String> {
        let symbol = symbol.trim().to_ascii_uppercase();
        if symbol.is_empty() {
            return Err("symbol is required".to_owned());
        }
        let sources = vec![
            race::source("stooq", || self.via_stooq(&symbol)),
            race::source("yahoo", || self.via_yahoo(&symbol)),
        ];
        race::first_success(&sources).await
    }

    async fn via_stooq(&self, symbol: &str) -> Result<Quote, String> {
        let url = format!(
            "{}?s={}&f=sd2t2ohlcv&h&e=csv",
            self.stooq_base,
            stooq_symbol(symbol)
        );
        let csv = get_text(&self.client, &url).await?;
        let row = parse_stooq_csv(&csv)?;
        Ok(Quote {
            symbol: symbol.to_owned(),
            name: None,
            price: row.close,
            previous_close: None,
            change_pct: None,
            currency: None,
            as_of: row.as_of,
            source: "stooq".to_owned(),
        })
    }

    async fn via_yahoo(&self, symbol: &str) -> Result<Quote, String> {
        let url = format!("{}/{symbol}?range=1d&interval=1d", self.yahoo_base);
        let body: ChartResponse = get_json(&self.client, &url).await?;
        let meta = body
            .chart
            .and_then(|chart| chart.result)
            .unwrap_or_default()
            .into_iter()
            .next()
            .map(|result| result.meta)
            .ok_or_else(|| format!("no chart data for {symbol}"))?;
        let price = meta
            .regular_market_price
            .ok_or_else(|| format!("no regularMarketPrice for {symbol}"))?;
        let previous_close = meta.previous_close.or(meta.chart_previous_close);
        Ok(Quote {
            symbol: meta.symbol.unwrap_or_else(|| symbol.to_owned()),
            name: meta.short_name.or(meta.long_name),
            price,
            previous_close,
            change_pct: previous_close
                .filter(|p| *p != 0.0)
                .map(|p| (price - p) / p * 100.0),
            currency: meta.currency,
            as_of: meta.regular_market_time.map_or_else(
                || chrono::Utc::now().to_rfc3339(),
                |seconds| {
                    chrono::DateTime::from_timestamp(seconds, 0)
                        .map_or_else(|| seconds.to_string(), |t| t.to_rfc3339())
                },
            ),
            source: "yahoo".to_owned(),
        })
    }
}

/// Stooq spells the big US indices with its own tickers, and US equities with a `.us` suffix —
/// `AAPL` alone resolves to nothing there.
#[must_use]
pub fn stooq_symbol(symbol: &str) -> String {
    match symbol {
        "^IXIC" | "^NDQ" => return "^ndq".to_owned(),
        "^GSPC" | "^SPX" => return "^spx".to_owned(),
        "^DJI" => return "^dji".to_owned(),
        _ => {}
    }
    let lowered = symbol.to_ascii_lowercase();
    if lowered.starts_with('^') || lowered.contains('.') || lowered.contains('=') {
        // Already an index or an explicitly-suffixed/foreign symbol: pass it through untouched
        // rather than guessing at a market.
        lowered
    } else {
        format!("{lowered}.us")
    }
}

struct StooqRow {
    close: f64,
    as_of: String,
}

/// Stooq's `h` flag prefixes the data with a header row; a symbol it doesn't know still returns
/// 200 with `N/D` in the price columns, which has to read as a failure so the race can prefer
/// Yahoo's answer.
fn parse_stooq_csv(csv: &str) -> Result<StooqRow, String> {
    let mut lines = csv.lines().filter(|line| !line.trim().is_empty());
    let header = lines.next().ok_or_else(|| "empty CSV".to_owned())?;
    let row = lines
        .next()
        .ok_or_else(|| "CSV had no data row".to_owned())?;
    let index_of = |name: &str| {
        header
            .split(',')
            .position(|column| column.trim().eq_ignore_ascii_case(name))
    };
    let cells: Vec<&str> = row.split(',').map(str::trim).collect();
    let cell = |name: &str| index_of(name).and_then(|i| cells.get(i).copied());
    let close = cell("Close")
        .and_then(|value| value.parse::<f64>().ok())
        .ok_or_else(|| format!("no usable Close in {row:?}"))?;
    let date = cell("Date").unwrap_or_default();
    let time = cell("Time").unwrap_or_default();
    Ok(StooqRow {
        close,
        as_of: format!("{date} {time}").trim().to_owned(),
    })
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .header("User-Agent", "ai-engineering-boilerplate-tool/0.1")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("returned {}", response.status()));
    }
    response.text().await.map_err(|e| e.to_string())
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let text = get_text(client, url).await?;
    serde_json::from_str(&text).map_err(|e| format!("unexpected response shape: {e}"))
}

#[derive(Deserialize)]
struct ChartResponse {
    chart: Option<Chart>,
}

#[derive(Deserialize)]
struct Chart {
    result: Option<Vec<ChartResult>>,
}

#[derive(Deserialize)]
struct ChartResult {
    meta: ChartMeta,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChartMeta {
    symbol: Option<String>,
    short_name: Option<String>,
    long_name: Option<String>,
    currency: Option<String>,
    regular_market_price: Option<f64>,
    previous_close: Option<f64>,
    chart_previous_close: Option<f64>,
    regular_market_time: Option<i64>,
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

    fn tool_pointing_at(stooq: &str, yahoo: &str) -> StockTool {
        let mut tool = StockTool::new(reqwest::Client::new());
        tool.stooq_base = stooq.to_owned();
        tool.yahoo_base = yahoo.to_owned();
        tool
    }

    #[test]
    fn index_and_ticker_symbols_are_translated_to_stooq_spelling() {
        assert_eq!(stooq_symbol("^IXIC"), "^ndq");
        assert_eq!(stooq_symbol("^GSPC"), "^spx");
        assert_eq!(stooq_symbol("^DJI"), "^dji");
        assert_eq!(stooq_symbol("AAPL"), "aapl.us");
        assert_eq!(stooq_symbol("EURUSD=X"), "eurusd=x");
        assert_eq!(stooq_symbol("CDR.PL"), "cdr.pl");
    }

    #[tokio::test]
    async fn the_stooq_csv_row_becomes_a_quote() {
        let stooq = serve(axum::Router::new().route(
            "/q/l/",
            get(|| async {
                "Symbol,Date,Time,Open,High,Low,Close,Volume\n^NDQ,2026-09-15,22:00:04,25990.1,26150.0,25960.2,26108.46,0\n"
            }),
        ))
        .await;
        let tool = tool_pointing_at(&format!("{stooq}/q/l/"), DEAD);

        let quote = tool.quote("^IXIC").await.expect("quote");

        assert_eq!(quote.symbol, "^IXIC");
        assert!((quote.price - 26108.46).abs() < 1e-9);
        assert_eq!(quote.as_of, "2026-09-15 22:00:04");
        assert_eq!(quote.source, "stooq");
    }

    #[tokio::test]
    async fn a_stooq_row_with_no_price_falls_through_to_yahoo() {
        let stooq = serve(axum::Router::new().route(
            "/q/l/",
            get(|| async {
                "Symbol,Date,Time,Open,High,Low,Close,Volume\nAAPL.US,N/D,N/D,N/D,N/D,N/D,N/D,N/D\n"
            }),
        ))
        .await;
        let yahoo = serve(axum::Router::new().route(
            "/chart/AAPL",
            get(|| async {
                Json(json!({"chart": {"result": [{"meta": {
                    "symbol": "AAPL",
                    "shortName": "Apple Inc.",
                    "currency": "USD",
                    "regularMarketPrice": 220.0,
                    "previousClose": 200.0,
                    "regularMarketTime": 1_789_000_000,
                }}]}}))
            }),
        ))
        .await;
        let tool = tool_pointing_at(&format!("{stooq}/q/l/"), &format!("{yahoo}/chart"));

        let quote = tool.quote("AAPL").await.expect("quote");

        assert_eq!(quote.source, "yahoo");
        assert_eq!(quote.name.as_deref(), Some("Apple Inc."));
        assert_eq!(quote.previous_close, Some(200.0));
        assert!(quote.change_pct.is_some_and(|c| (c - 10.0).abs() < 1e-9));
        assert_eq!(quote.currency.as_deref(), Some("USD"));
        assert!(quote.as_of.starts_with("2026-"), "{}", quote.as_of);
    }

    #[tokio::test]
    async fn both_sources_failing_names_both() {
        let tool = tool_pointing_at(DEAD, DEAD);

        let error = tool.quote("AAPL").await.unwrap_err();

        assert!(error.contains("stooq"), "{error}");
        assert!(error.contains("yahoo"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_symbol_is_rejected_before_any_request() {
        assert!(StockTool::default().quote(" ").await.is_err());
    }
}
