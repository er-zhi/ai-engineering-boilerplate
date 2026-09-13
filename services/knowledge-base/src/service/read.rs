// Reads one immutable document revision in bounded character pages.

use common::proto::knowledge_base::v1::{DocumentRef, ReadDocumentRequest, ReadDocumentResponse};
use connectrpc::ConnectError;

use super::{MAX_SOURCE_CHARS, MAX_SOURCE_ID_CHARS};
use crate::store::DocumentStore;

const DEFAULT_PAGE_CHARS: usize = 16_000;
const MAX_PAGE_CHARS: usize = 50_000;
const MAX_CURSOR_CHARS: usize = 24;
const CURSOR_PREFIX: &str = "kb1:";

pub(super) async fn run(
    documents: &impl DocumentStore,
    request: ReadDocumentRequest,
) -> Result<ReadDocumentResponse, ConnectError> {
    let reference = request
        .document
        .into_option()
        .ok_or_else(|| ConnectError::invalid_argument("document is required"))?;
    validate_reference(&reference)?;
    let offset = cursor_offset(&request.cursor)?;
    let page_chars = page_chars(request.max_chars)?;
    let document = documents
        .document(&reference.source, &reference.source_id)
        .await
        .map_err(|error| {
            tracing::error!("document read failed: {error}");
            ConnectError::unavailable("the document store is not available")
        })?
        .ok_or_else(|| ConnectError::not_found("document was not found"))?;
    if document.content_hash != reference.version {
        return Err(ConnectError::failed_precondition(
            "document changed; search again before reading it",
        ));
    }

    let total_chars = document.content.chars().count();
    if offset > total_chars {
        return Err(ConnectError::invalid_argument(
            "cursor is past the document end",
        ));
    }
    let content: String = document
        .content
        .chars()
        .skip(offset)
        .take(page_chars)
        .collect();
    let next_offset = offset + content.chars().count();
    let next_cursor = if next_offset < total_chars {
        format!("{CURSOR_PREFIX}{next_offset:x}")
    } else {
        String::new()
    };
    let total_chars = u32::try_from(total_chars).map_err(|_| {
        tracing::error!("stored document exceeds the response size type");
        ConnectError::internal("stored document is invalid")
    })?;

    Ok(ReadDocumentResponse {
        document: reference.into(),
        title: document.title,
        content,
        next_cursor,
        total_chars,
        ..Default::default()
    })
}

fn validate_reference(reference: &DocumentRef) -> Result<(), ConnectError> {
    if reference.source.trim().is_empty() || reference.source.chars().count() > MAX_SOURCE_CHARS {
        return Err(ConnectError::invalid_argument("document source is invalid"));
    }
    if reference.source_id.trim().is_empty()
        || reference.source_id.chars().count() > MAX_SOURCE_ID_CHARS
    {
        return Err(ConnectError::invalid_argument(
            "document source_id is invalid",
        ));
    }
    if reference.version.len() != 64
        || !reference
            .version
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ConnectError::invalid_argument(
            "document version must be a SHA-256 hash",
        ));
    }
    Ok(())
}

fn cursor_offset(cursor: &str) -> Result<usize, ConnectError> {
    if cursor.is_empty() {
        return Ok(0);
    }
    if cursor.len() > MAX_CURSOR_CHARS {
        return Err(ConnectError::invalid_argument("document cursor is invalid"));
    }
    cursor
        .strip_prefix(CURSOR_PREFIX)
        .and_then(|value| usize::from_str_radix(value, 16).ok())
        .ok_or_else(|| ConnectError::invalid_argument("document cursor is invalid"))
}

fn page_chars(requested: u32) -> Result<usize, ConnectError> {
    match requested as usize {
        0 => Ok(DEFAULT_PAGE_CHARS),
        value if value > MAX_PAGE_CHARS => Err(ConnectError::invalid_argument(format!(
            "max_chars is above {MAX_PAGE_CHARS}"
        ))),
        value => Ok(value),
    }
}
