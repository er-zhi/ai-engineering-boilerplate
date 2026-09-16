// Slugs of the four system tools this service seeds at startup (Task 11) and dispatches Execute
// to by name (Task 10). A slug not in this list belongs to a user-created tool, which today has
// no runnable implementation — see the spec's "Вне скоупа".

pub const WEB_SEARCH: &str = "web_search";
pub const WEB_FETCH: &str = "web_fetch";
pub const KB_SEARCH: &str = "kb_search";
pub const KB_READ_DOCUMENT: &str = "kb_read_document";

/// Every reserved system slug — `create_tool` rejects a user-owned row claiming one of these
/// (see its call site), so the four constants above stay the only names `run_system_tool`
/// dispatches on.
pub const ALL: [&str; 4] = [WEB_SEARCH, WEB_FETCH, KB_SEARCH, KB_READ_DOCUMENT];
