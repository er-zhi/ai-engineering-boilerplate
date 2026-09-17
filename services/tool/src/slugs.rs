// Slugs of the system tools this service seeds at startup and dispatches Execute to by name.

pub const WEB_SEARCH: &str = "web_search";
pub const WEB_FETCH: &str = "web_fetch";
pub const KB_SEARCH: &str = "kb_search";
pub const KB_READ_DOCUMENT: &str = "kb_read_document";

pub const RESERVED_FOR_SYSTEM_TOOLS: [&str; 4] =
    [WEB_SEARCH, WEB_FETCH, KB_SEARCH, KB_READ_DOCUMENT];
