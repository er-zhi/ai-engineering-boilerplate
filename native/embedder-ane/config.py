"""Shared between server.py (serving) and convert.py (one-time conversion). These must never
drift independently — a mismatched SEQ_LEN desyncs the compiled .mlpackage's fixed input shape
from what the server sends, and a mismatched prefix silently hurts retrieval ranking. Neither
failure mode raises an error."""

SEQ_LEN = 128
QUERY_PREFIX = "task: search result | query: "
DOCUMENT_PREFIX = "title: none | text: "
