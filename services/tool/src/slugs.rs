// Slugs of the system tools this service seeds at startup (Task 11) and dispatches Execute
// to by name (Task 10). A slug not in this list belongs to a user-created tool, which today has
// no runnable implementation — see the spec's "Вне скоупа".

pub const WEB_SEARCH: &str = "web_search";
pub const WEB_FETCH: &str = "web_fetch";
pub const KB_SEARCH: &str = "kb_search";
pub const KB_READ_DOCUMENT: &str = "kb_read_document";
pub const WEATHER: &str = "weather";
pub const FX_RATE: &str = "fx_rate";
pub const STOCK_QUOTE: &str = "stock_quote";

/// Every reserved system slug — `create_tool` rejects a user-owned row claiming one of these
/// (see its call site), so the constants above stay the only names `run_system_tool`
/// dispatches on.
pub const ALL: [&str; 7] = [
    WEB_SEARCH,
    WEB_FETCH,
    KB_SEARCH,
    KB_READ_DOCUMENT,
    WEATHER,
    FX_RATE,
    STOCK_QUOTE,
];
