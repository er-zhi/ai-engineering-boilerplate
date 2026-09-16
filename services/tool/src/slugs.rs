// Slugs of the four system tools this service seeds at startup (Task 11) and dispatches Execute
// to by name (Task 10). A slug not in this list belongs to a user-created tool, which today has
// no runnable implementation — see the spec's "Вне скоупа".

pub const WEB_SEARCH: &str = "web_search";
pub const WEB_FETCH: &str = "web_fetch";
pub const KB_SEARCH: &str = "kb_search";
pub const KB_READ_DOCUMENT: &str = "kb_read_document";
