# Crawler Page Storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every in-scope page a crawl fetches is saved to `crawler.pages` as extracted main text, and `pages_crawled` counts the rows actually written.

**Architecture:** `crawl.rs` stays database-free and hands each counted page (url, html, status) to its caller. `extract.rs` turns HTML into title + main text + SHA-256. `store.rs` defines a `PageStore` trait and its Postgres implementation, which upserts by URL through a SeaORM 2 entity. `jobs.rs` runs the crawl and a store loop side by side and bumps the count after each write. `main.rs` connects and runs entity-first schema sync on startup.

**Tech Stack:** Rust 1.98 (edition 2024), spider 2.53, SeaORM 2.0 (entity-first schema sync), dom_smoothie 0.18, sha2 0.11, chrono 0.4, testcontainers 0.28, cargo-nextest, Docker Compose, Postgres 18 + pgvector.

**Spec:** [`docs/superpowers/specs/2026-09-11-crawler-page-storage-design.md`](../specs/2026-09-11-crawler-page-storage-design.md)

## Global Constraints

- Rust 1.98, edition 2024; every dependency's MSRV ≤ 1.98 (SeaORM 2.0.2: 1.94, testcontainers 0.28: 1.88, dom_smoothie 0.18: 1.75).
- Table `crawler.pages` columns, exactly: `id bigint` (auto), `url varchar(2048) NOT NULL UNIQUE`, `title varchar(512) NOT NULL`, `main_text text NOT NULL`, `content_hash char(64) NOT NULL`, `http_status smallint NOT NULL`, `crawled_at timestamptz NOT NULL`.
- Raw HTML is never stored. Logs go to stderr, never to the database.
- A page whose write fails is not counted; the job still ends `DONE`. `FAILED` only when no page could be fetched.
- The crawl tests and the job tests that don't need Postgres stay database-free.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` stay clean after every task.
- All commands run through `scripts/test.sh` (created in Task 1): the host has no Rust toolchain.

---

### Task 1: Test runner, and crawl hands full pages to its caller

**Files:**
- Create: `scripts/test.Dockerfile`, `scripts/test.sh`
- Modify: `services/crawler/src/crawl.rs` (callback type, `CountedPage`, tests)

**Interfaces:**
- Produces: `crawl::CountedPage { pub url: String, pub html: String, pub status: u16 }` and `crawl::crawl(base_url: &str, scope: &Scope, limits: Limits, on_counted: impl FnMut(CountedPage) + Send) -> Result<(), Unreachable>`.

- [ ] **Step 1: Create the test runner**

`scripts/test.Dockerfile`:

```dockerfile
# Test runner image: the workspace toolchain plus cargo-nextest, clippy, and rustfmt.
FROM rust:1.98-alpine3.24
RUN apk add --no-cache musl-dev build-base protoc protobuf-dev curl \
    && rustup component add clippy rustfmt \
    && curl -LsSf "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.144/cargo-nextest-0.9.144-$(uname -m)-unknown-linux-musl.tar.gz" \
       | tar zxf - -C /usr/local/cargo/bin
```

`scripts/test.sh` (then `chmod +x scripts/test.sh`):

```sh
#!/bin/sh
# Runs cargo in the test image, so no local Rust toolchain is needed.
# Database tests start Postgres with testcontainers, so the container gets the Docker socket
# and host networking to reach the ports testcontainers maps.
# Usage: scripts/test.sh [cargo args...]   (default: nextest run --workspace)
set -eu
cd "$(dirname "$0")/.."
docker build -q -t ai-boilerplate-test -f scripts/test.Dockerfile scripts >/dev/null
[ "$#" -eq 0 ] && set -- nextest run --workspace
exec docker run --rm --network host \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v "$PWD":/w -w /w \
  -v ai-boilerplate-test-target:/target -e CARGO_TARGET_DIR=/target \
  -v ai-boilerplate-cargo-registry:/usr/local/cargo/registry \
  ai-boilerplate-test cargo "$@"
```

- [ ] **Step 2: Write the failing test** — append to the `tests` module in `crawl.rs`:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn counted_pages_carry_their_html_and_status() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;

        let mut pages = Vec::new();
        crawl(&site.url("/"), &everything(), LIMITS, |page| pages.push(page))
            .await
            .unwrap();

        assert!(
            pages
                .iter()
                .any(|page| page.status == 200 && page.html.contains(r#"<a href="/docs/a">"#)),
            "no counted page carried the root's HTML"
        );
    }
```

And change the closure in `counts_in_scope_pages_reached_through_out_of_scope_pages` from `|url| counted.push(url.to_owned())` to `|page| counted.push(page.url)`.

- [ ] **Step 3: Run it to verify it fails**

Run: `scripts/test.sh nextest run -p crawler`
Expected: compile error `E0609: no field 'status' on type '&str'` (the callback still receives `&str`).

- [ ] **Step 4: Implement** — in `crawl.rs`, add below `Unreachable`:

```rust
/// A fetched page the scope counts, handed to the caller as soon as it arrives.
pub struct CountedPage {
    pub url: String,
    pub html: String,
    pub status: u16,
}
```

Change the signature's last parameter to `mut on_counted: impl FnMut(CountedPage) + Send`, update its doc comment to "calls `on_counted` with each successfully fetched page the scope counts", and change the body of `handle`:

```rust
    let mut handle = |page: Page| {
        if page.status_code.is_success() {
            any_fetched = true;
            if scope.counts(page.get_url()) {
                on_counted(CountedPage {
                    url: page.get_url().to_owned(),
                    html: page.get_html(),
                    status: page.status_code.as_u16(),
                });
            }
        }
    };
```

`jobs.rs` keeps compiling unchanged: its closure is `|_| ...`.

- [ ] **Step 5: Run the tests, clippy, and fmt**

Run: `scripts/test.sh nextest run -p crawler` — Expected: 21 passed.
Run: `scripts/test.sh clippy --workspace --all-targets -- -D warnings` — Expected: no warnings.
Run: `scripts/test.sh fmt --all -- --check` — Expected: no diff (if there is one, run `scripts/test.sh fmt --all`).

- [ ] **Step 6: Commit**

```bash
git add scripts/ services/crawler/src/crawl.rs
git commit -m "Add Docker test runner; crawl hands counted pages with HTML and status"
```

---

### Task 2: Main-text extraction

**Files:**
- Create: `services/crawler/src/extract.rs`
- Modify: `Cargo.toml` (workspace deps), `services/crawler/Cargo.toml`, `services/crawler/src/main.rs` (add `mod extract;`)

**Interfaces:**
- Produces: `extract::Extracted { pub title: String, pub main_text: String, pub content_hash: String }` and `extract::extract(url: &str, html: &str) -> Extracted`.

- [ ] **Step 1: Add dependencies**

Workspace `Cargo.toml` `[workspace.dependencies]`, alphabetical with the others:

```toml
dom_smoothie = "0.18"
sha2 = "0.11"
```

`services/crawler/Cargo.toml` `[dependencies]`:

```toml
dom_smoothie = { workspace = true }
sha2 = { workspace = true }
```

`main.rs`: add `mod extract;` to the module list.

- [ ] **Step 2: Write the failing tests** — `services/crawler/src/extract.rs`:

```rust
// Turns fetched HTML into what the crawler keeps: a title, the main text, and a hash of that text.
// Raw HTML never leaves this module.

/// `crawler.pages.title` is varchar(512).
const MAX_TITLE_CHARS: usize = 512;

#[derive(Debug, PartialEq)]
pub struct Extracted {
    pub title: String,
    pub main_text: String,
    /// Lowercase hex SHA-256 of `main_text`.
    pub content_hash: String,
}

/// Pages without a recognizable article keep their `<title>` and get empty text.
pub fn extract(url: &str, html: &str) -> Extracted {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE_PAGE: &str = r#"<!doctype html><html><head><title>Rust Ownership Guide</title></head><body>
<nav><a href="/">Home</a> <a href="/pricing">Pricing</a> Sign in</nav>
<article><h1>Rust Ownership Guide</h1>
<p>Ownership is a set of rules that govern how a Rust program manages memory. Each value has exactly one owner, and the value is dropped when its owner goes out of scope.</p>
<p>Borrowing lets code use a value without taking ownership of it. References must always be valid, which the borrow checker enforces at compile time.</p>
<p>Moves transfer ownership between variables, so the original binding can no longer be used after the move happens in the program.</p>
</article>
<footer>Copyright 2026 Example Corp. Privacy policy. Cookie settings.</footer>
</body></html>"#;

    const EMPTY_PAGE: &str = "<!doctype html><html><head><title>Empty</title></head><body></body></html>";

    #[test]
    fn keeps_the_article_text_and_drops_navigation_and_footer() {
        let page = extract("https://example.com/guide", ARTICLE_PAGE);

        assert_eq!(page.title, "Rust Ownership Guide");
        assert!(page.main_text.contains("Each value has exactly one owner"), "{}", page.main_text);
        assert!(page.main_text.contains("borrow checker enforces"), "{}", page.main_text);
        for boilerplate in ["Pricing", "Sign in", "Cookie settings"] {
            assert!(!page.main_text.contains(boilerplate), "kept {boilerplate:?}: {}", page.main_text);
        }
    }

    #[test]
    fn page_without_an_article_keeps_its_title_and_has_no_text() {
        let page = extract("https://example.com/", EMPTY_PAGE);

        assert_eq!(page.title, "Empty");
        assert_eq!(page.main_text, "");
    }

    #[test]
    fn content_hash_is_the_sha256_of_the_main_text() {
        // Published SHA-256 test vectors, so the expected values don't come from our own code.
        assert_eq!(
            extract("https://example.com/", EMPTY_PAGE).content_hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn titles_longer_than_the_column_are_cut_to_512_characters() {
        let long = "é".repeat(600);
        let html = format!("<html><head><title>{long}</title></head><body></body></html>");

        let page = extract("https://example.com/", &html);

        assert_eq!(page.title.chars().count(), 512);
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `scripts/test.sh nextest run -p crawler extract`
Expected: compile error `cannot find function 'sha256_hex'` (and, once that exists, 4 failures panicking with `not yet implemented`).

- [ ] **Step 4: Implement** — replace the `todo!()` body and add the helper plus imports at the top of `extract.rs`:

```rust
use dom_smoothie::Readability;
use sha2::{Digest, Sha256};
```

```rust
pub fn extract(url: &str, html: &str) -> Extracted {
    let (title, text) = match Readability::new(html, Some(url), None) {
        Ok(mut readability) => {
            let fallback_title = readability.get_article_title().to_string();
            match readability.parse() {
                Ok(article) => (article.title, article.text_content.to_string()),
                Err(_) => (fallback_title, String::new()),
            }
        }
        Err(_) => (String::new(), String::new()),
    };
    let main_text = tidy(&text);

    Extracted {
        title: title.trim().chars().take(MAX_TITLE_CHARS).collect(),
        content_hash: sha256_hex(&main_text),
        main_text,
    }
}

/// Trims each line and drops blank ones, so whitespace churn alone doesn't change the hash.
fn tidy(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
```

- [ ] **Step 5: Run the tests, clippy, and fmt**

Run: `scripts/test.sh nextest run -p crawler` — Expected: 25 passed.
Run clippy and fmt as in Task 1 Step 5.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock services/crawler/Cargo.toml services/crawler/src/extract.rs services/crawler/src/main.rs
git commit -m "Extract page title and main text with dom_smoothie"
```

---

### Task 3: `crawler.pages` entity and the Postgres page store

**Files:**
- Create: `services/crawler/src/entity/mod.rs`, `services/crawler/src/entity/page.rs`, `services/crawler/src/store.rs`, `services/crawler/src/test_db.rs`
- Modify: `Cargo.toml`, `services/crawler/Cargo.toml`, `services/crawler/src/main.rs` (add `mod entity; mod store; #[cfg(test)] mod test_db;`)

**Interfaces:**
- Consumes: `crawl::CountedPage` (Task 1), `extract::extract` (Task 2).
- Produces: `store::PageStore` trait — `fn save(&self, page: CountedPage) -> impl Future<Output = Result<(), sea_orm::DbErr>> + Send`, bound `Clone + Send + Sync + 'static`; `store::PgPages::new(db: DatabaseConnection) -> PgPages`; `entity::page::{Entity, Model, Column, ActiveModel}`; `test_db::start() -> TestDb` with `pub db: DatabaseConnection`.

- [ ] **Step 1: Add dependencies**

Workspace `Cargo.toml`:

```toml
chrono = { version = "0.4", default-features = false, features = ["clock"] }
sea-orm = { version = "2.0", default-features = false, features = ["macros", "with-chrono", "sqlx-postgres", "runtime-tokio-rustls", "schema-sync", "entity-registry"] }
testcontainers = "0.28"
```

`services/crawler/Cargo.toml`: add `chrono = { workspace = true }` and `sea-orm = { workspace = true }` to `[dependencies]`, and `testcontainers = { workspace = true }` to `[dev-dependencies]`.

- [ ] **Step 2: Write the entity** — `services/crawler/src/entity/mod.rs`:

```rust
// Database entities. Schema sync creates and extends their tables on startup.

pub mod page;
```

`services/crawler/src/entity/page.rs`:

```rust
// crawler.pages: one row per crawled URL, holding extracted text only, never raw HTML.
// Schema sync builds the table from this struct, so these field types are the column types.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "pages", schema_name = "crawler")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique, column_type = "String(StringLen::N(2048))")]
    pub url: String,
    #[sea_orm(column_type = "String(StringLen::N(512))")]
    pub title: String,
    #[sea_orm(column_type = "Text")]
    pub main_text: String,
    #[sea_orm(column_type = "Char(Some(64))")]
    pub content_hash: String,
    pub http_status: i16,
    pub crawled_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
```

- [ ] **Step 3: Write the test database helper** — `services/crawler/src/test_db.rs`:

```rust
// Test-only Postgres: a throwaway pgvector container per test, with the crawler's tables synced in.
// Needs Docker; scripts/test.sh provides the socket and host networking.

use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

pub struct TestDb {
    pub db: DatabaseConnection,
    /// Dropping the handle removes the container.
    _container: ContainerAsync<GenericImage>,
}

pub async fn start() -> TestDb {
    let container = GenericImage::new("pgvector/pgvector", "pg18")
        .with_exposed_port(5432.tcp())
        // Postgres logs this twice: once for the init-time server and once for the real one.
        .with_wait_for(WaitFor::log(
            LogWaitStrategy::stderr("database system is ready to accept connections").with_times(2),
        ))
        .with_env_var("POSTGRES_PASSWORD", "test")
        .start()
        .await
        .expect("start Postgres; database tests need Docker (see scripts/test.sh)");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();

    let db = Database::connect(format!("postgres://postgres:test@{host}:{port}/postgres"))
        .await
        .unwrap();
    // In Compose, infra/postgres/init.sh creates the schema; here the superuser does.
    db.execute_unprepared("CREATE SCHEMA crawler").await.unwrap();
    db.get_schema_registry("crawler::entity::*")
        .sync(&db)
        .await
        .unwrap();

    TestDb {
        db,
        _container: container,
    }
}
```

- [ ] **Step 4: Write the store skeleton and the failing tests** — `services/crawler/src/store.rs`:

```rust
// Persists counted pages: extracts the main text, then inserts or updates the row for the URL.

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, DatabaseConnection, DbErr, EntityTrait};

use crate::crawl::CountedPage;
use crate::entity::page;
use crate::extract::extract;

/// Where counted pages go. Jobs depend on this trait, so job tests can run without a database.
pub trait PageStore: Clone + Send + Sync + 'static {
    fn save(&self, page: CountedPage) -> impl Future<Output = Result<(), DbErr>> + Send;
}

#[derive(Clone)]
pub struct PgPages {
    db: DatabaseConnection,
}

impl PgPages {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl PageStore for PgPages {
    async fn save(&self, page: CountedPage) -> Result<(), DbErr> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use super::*;
    use crate::test_db;

    fn counted(url: &str, title: &str) -> CountedPage {
        CountedPage {
            url: url.to_owned(),
            html: format!("<html><head><title>{title}</title></head><body></body></html>"),
            status: 200,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_sync_creates_pages_with_the_designed_column_types() {
        let test = test_db::start().await;

        let rows = test
            .db
            .query_all(Statement::from_string(
                DbBackend::Postgres,
                "SELECT column_name::text, data_type::text, character_maximum_length::int, is_nullable::text \
                 FROM information_schema.columns \
                 WHERE table_schema = 'crawler' AND table_name = 'pages' ORDER BY column_name",
            ))
            .await
            .unwrap();
        let columns: Vec<(String, String, Option<i32>, String)> = rows
            .iter()
            .map(|row| {
                (
                    row.try_get("", "column_name").unwrap(),
                    row.try_get("", "data_type").unwrap(),
                    row.try_get("", "character_maximum_length").unwrap(),
                    row.try_get("", "is_nullable").unwrap(),
                )
            })
            .collect();

        let expected: Vec<(String, String, Option<i32>, String)> = [
            ("content_hash", "character", Some(64)),
            ("crawled_at", "timestamp with time zone", None),
            ("http_status", "smallint", None),
            ("id", "bigint", None),
            ("main_text", "text", None),
            ("title", "character varying", Some(512)),
            ("url", "character varying", Some(2048)),
        ]
        .into_iter()
        .map(|(name, kind, len)| (name.to_owned(), kind.to_owned(), len, "NO".to_owned()))
        .collect();
        assert_eq!(columns, expected);

        let unique_url_indexes: i32 = test
            .db
            .query_one(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*)::int AS n FROM pg_indexes \
                 WHERE schemaname = 'crawler' AND tablename = 'pages' \
                 AND indexdef LIKE 'CREATE UNIQUE INDEX%(url)'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap();
        assert_eq!(unique_url_indexes, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn storing_a_url_again_updates_its_row() {
        let test = test_db::start().await;
        let store = PgPages::new(test.db.clone());

        store.save(counted("https://example.com/a", "First")).await.unwrap();
        store.save(counted("https://example.com/a", "Second")).await.unwrap();

        let rows = page::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Second");
        assert_eq!(rows[0].http_status, 200);
        assert_eq!(rows[0].content_hash.len(), 64);
    }
}
```

Add `mod entity;`, `mod store;` and `#[cfg(test)] mod test_db;` to `main.rs`.

- [ ] **Step 5: Run to verify they fail**

Run: `scripts/test.sh nextest run -p crawler store`
Expected: `schema_sync_creates_pages_with_the_designed_column_types` PASSES (sync already works — it is the schema contract), `storing_a_url_again_updates_its_row` FAILS with `not yet implemented`.

- [ ] **Step 6: Implement `save`**

```rust
impl PageStore for PgPages {
    async fn save(&self, page: CountedPage) -> Result<(), DbErr> {
        let extracted = extract(&page.url, &page.html);
        let row = page::ActiveModel {
            url: Set(page.url),
            title: Set(extracted.title),
            main_text: Set(extracted.main_text),
            content_hash: Set(extracted.content_hash),
            http_status: Set(i16::try_from(page.status).expect("HTTP status codes fit in smallint")),
            crawled_at: Set(Utc::now()),
            ..Default::default()
        };

        page::Entity::insert(row)
            .on_conflict(
                OnConflict::column(page::Column::Url)
                    .update_columns([
                        page::Column::Title,
                        page::Column::MainText,
                        page::Column::ContentHash,
                        page::Column::HttpStatus,
                        page::Column::CrawledAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await?;
        Ok(())
    }
}
```

- [ ] **Step 7: Run the tests, clippy, and fmt**

Run: `scripts/test.sh nextest run -p crawler` — Expected: 27 passed.
Run clippy and fmt as in Task 1 Step 5. (`PgPages`/`PageStore` are unused outside tests until Task 5; if clippy reports dead code, add `#[cfg_attr(not(test), allow(dead_code))]` on the item and remove it in Task 5.)

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock services/crawler/
git commit -m "Store crawled pages in crawler.pages via a SeaORM entity with schema sync"
```

---

### Task 4: Jobs count stored pages

**Files:**
- Modify: `services/crawler/src/jobs.rs`

**Interfaces:**
- Consumes: `store::PageStore`, `store::PgPages` (Task 3), `crawl::CountedPage` (Task 1), `test_db::start` (Task 3).
- Produces: `jobs::Jobs<S: PageStore>` with `Jobs::new(store: S) -> Jobs<S>`; `create`, `get`, `run` keep their signatures.

- [ ] **Step 1: Write the failing tests** — in the `jobs.rs` tests module, add these imports and test stores:

```rust
    use sea_orm::{DbErr, EntityTrait};

    use crate::crawl::CountedPage;
    use crate::entity::page;
    use crate::store::{PageStore, PgPages};
    use crate::test_db;

    /// Keeps saved URLs in memory, so job tests run without a database.
    #[derive(Clone, Default)]
    struct MemoryPages(Arc<Mutex<Vec<String>>>);

    impl PageStore for MemoryPages {
        async fn save(&self, page: CountedPage) -> Result<(), DbErr> {
            self.0.lock().unwrap().push(page.url);
            Ok(())
        }
    }

    /// Rejects every write, like a database that went away mid-crawl.
    #[derive(Clone)]
    struct FailingPages;

    impl PageStore for FailingPages {
        async fn save(&self, _page: CountedPage) -> Result<(), DbErr> {
            Err(DbErr::Custom("database is down".into()))
        }
    }
```

Replace every `Jobs::default()` in the existing tests with `Jobs::new(MemoryPages::default())`, then add:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn job_counts_exactly_the_pages_the_store_accepted() {
        let site = TestSite::start([
            ("/", "/docs/a /docs/b /blog/x"),
            ("/docs/a", ""),
            ("/docs/b", ""),
            ("/blog/x", ""),
        ])
        .await;
        let store = MemoryPages::default();
        let jobs = Jobs::new(store.clone());
        let id = jobs.create(&site.url("/"));
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        });

        jobs.run(&id, &site.url("/"), scope, LIMITS).await;

        let mut saved = store.0.lock().unwrap().clone();
        saved.sort();
        assert_eq!(saved, [site.url("/docs/a"), site.url("/docs/b")]);
        assert_eq!(jobs.get(&id).unwrap().pages_crawled, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pages_that_fail_to_store_are_not_counted_and_the_job_still_finishes() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;
        let jobs = Jobs::new(FailingPages);
        let id = jobs.create(&site.url("/"));

        jobs.run(&id, &site.url("/"), Scope::new(&CrawlScope::default()), LIMITS)
            .await;

        let job = jobs.get(&id).unwrap();
        assert_eq!(job.status, CrawlStatus::Done);
        assert_eq!(job.pages_crawled, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crawl_stores_exactly_the_in_scope_pages_in_postgres() {
        let test = test_db::start().await;
        let site = TestSite::start([("/", "/docs/a /blog/x"), ("/docs/a", ""), ("/blog/x", "")]).await;
        let jobs = Jobs::new(PgPages::new(test.db.clone()));
        let id = jobs.create(&site.url("/"));
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        });

        jobs.run(&id, &site.url("/"), scope, LIMITS).await;

        let urls: Vec<String> = page::Entity::find()
            .all(&test.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.url)
            .collect();
        assert_eq!(urls, [site.url("/docs/a")]);
        assert_eq!(jobs.get(&id).unwrap().pages_crawled, 1);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `scripts/test.sh nextest run -p crawler jobs`
Expected: compile error `no function or associated item named 'new' found for struct 'Jobs'`.

- [ ] **Step 3: Implement** — in `jobs.rs`:

Imports:

```rust
use tokio::sync::mpsc;

use crate::crawl::{self, CountedPage, Limits, Unreachable};
use crate::scope::Scope;
use crate::store::PageStore;
```

Struct and constructor (replace `#[derive(Clone, Default)] pub struct Jobs { ... }`):

```rust
#[derive(Clone)]
pub struct Jobs<S> {
    jobs: Arc<Mutex<HashMap<String, CrawlJob>>>,
    next_id: Arc<AtomicU64>,
    store: S,
}

impl<S: PageStore> Jobs<S> {
    pub fn new(store: S) -> Self {
        Self {
            jobs: Arc::default(),
            next_id: Arc::default(),
            store,
        }
    }
```

(`create`, `get`, and `update` keep their bodies inside this `impl<S: PageStore> Jobs<S>` block.)

`run`, replacing the old body:

```rust
    /// Crawls for the job: RUNNING while it runs, DONE when it finishes, FAILED if the site is unreachable.
    /// Each counted page is stored; `pages_crawled` rises only after its row is written.
    pub async fn run(&self, job_id: &str, base_url: &str, scope: Scope, limits: Limits) {
        self.update(job_id, |job| job.status = CrawlStatus::Running);

        let (counted, mut to_store) = mpsc::unbounded_channel::<CountedPage>();
        let crawling = async move {
            // The sender moves into the callback, so the channel closes as soon as the crawl ends.
            crawl::crawl(base_url, &scope, limits, move |page| {
                let _ = counted.send(page);
            })
            .await
        };
        let storing = async {
            while let Some(page) = to_store.recv().await {
                let url = page.url.clone();
                match self.store.save(page).await {
                    Ok(()) => self.update(job_id, |job| job.pages_crawled += 1),
                    Err(error) => eprintln!("crawler: could not store {url}: {error}"),
                }
            }
        };
        let (outcome, ()) = tokio::join!(crawling, storing);

        let status = match outcome {
            Ok(()) => CrawlStatus::Done,
            Err(Unreachable) => CrawlStatus::Failed,
        };
        self.update(job_id, |job| job.status = status);
    }
```

`main.rs` still builds `Jobs::default()`; change it to `Jobs::new(...)` in Task 5. Until then, give `main.rs` a compiling placeholder-free bridge by doing Task 5 Step 1 now if the build fails (the `Crawler` struct needs a concrete store type).

- [ ] **Step 4: Run the tests, clippy, and fmt**

Run: `scripts/test.sh nextest run -p crawler` — Expected: 30 passed.
Run clippy and fmt as in Task 1 Step 5.

- [ ] **Step 5: Commit**

```bash
git add services/crawler/src/jobs.rs services/crawler/src/main.rs
git commit -m "Jobs store each counted page and count only written rows"
```

---

### Task 5: Wire the service to Postgres and verify end to end

**Files:**
- Modify: `services/crawler/src/main.rs`, `docker-compose.yml`, `.env.example`, `services/crawler/README.md`, `README.md`

**Interfaces:**
- Consumes: `store::PgPages`, `jobs::Jobs::new`, `entity` module (Tasks 3–4).

- [ ] **Step 1: Connect, sync, and build jobs with the Postgres store** — in `main.rs`:

```rust
use sea_orm::Database;

use crate::store::PgPages;
```

```rust
struct Crawler {
    jobs: Jobs<PgPages>,
    limits: Limits,
}
```

In `main()`, before building `crawler`:

```rust
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let db = Database::connect(&database_url).await?;
    // Creates missing tables and columns from the entities and never drops anything.
    // Replace with migration files before this holds data that matters.
    db.get_schema_registry("crawler::entity::*").sync(&db).await?;
```

and build the jobs with `jobs: Jobs::new(PgPages::new(db)),`. Remove any `allow(dead_code)` added in Task 3.

- [ ] **Step 2: Give the crawler its database in Compose** — in `docker-compose.yml`, the `crawler` service:

```yaml
    environment:
      CRAWL_MAX_PAGES: ${CRAWL_MAX_PAGES:-100}
      DATABASE_URL: postgres://crawler_user:${CRAWLER_DB_PASSWORD:?set it in .env, see .env.example}@postgres:5432/${POSTGRES_DB:-app}
    depends_on:
      postgres:
        condition: service_healthy
```

In `.env.example`, change the Postgres comment's last line to `# Generate real values with: openssl rand -hex 24 (hex keeps them safe inside DATABASE_URL)`.

- [ ] **Step 3: Update the docs**

`services/crawler/README.md`, "What Runs Today": replace "The first slice crawls for real but stores nothing yet:" with "Crawls for real and stores what it finds:"; add the bullet "Each counted page is saved to `crawler.pages` (title, main text via dom_smoothie, SHA-256, status, time), updated in place on re-crawl. `pages_crawled` counts rows written; a failed write is logged to stderr and skipped."; replace "Jobs live in memory and vanish on restart. Nothing past **Fetch** in the pipeline below runs yet." with "Jobs still live in memory. Schema sync runs on every start; replace it with migrations before real data. Denoise and Store run; Enrich, Embed, and Search do not yet."; replace the Tests line with "Tests: `scripts/test.sh nextest run -p crawler`. Crawl tests use a local site; storage tests start Postgres with testcontainers, so they need Docker."

Root `README.md` status table: replace the `Postgres 18 + pgvector …` and `Page storage …` rows with:

```markdown
| Postgres 18 + pgvector, one schema and role per service | Done |
| Crawler page storage: title, main text, content hash in `crawler.pages` | Done |
| Enrichment, embeddings, search, `CacheStore` | Next |
```

- [ ] **Step 4: Full check**

Run: `scripts/test.sh nextest run --workspace` — Expected: all pass (30 crawler tests).
Run clippy and fmt as in Task 1 Step 5.

- [ ] **Step 5: Verify in the running stack**

```bash
docker compose up -d --build --wait            # dev mode: Postgres published on 127.0.0.1:5432
curl -s -X POST localhost:8080/crawler.v1.CrawlerService/StartCrawl \
  -H 'content-type: application/json' -H 'connect-protocol-version: 1' \
  -d '{"baseUrl":"https://quotes.toscrape.com","scope":{"includePatterns":["/tag/*"]}}'
# poll GetCrawlJob {"jobId":"job-1"} until CRAWL_STATUS_DONE, note pagesCrawled
docker compose exec -T postgres psql -U postgres -d app -tAc \
  "select count(*), count(distinct url), min(length(content_hash)), max(length(title)) from crawler.pages"
```

Expected: `count` equals `pagesCrawled`, `count = count(distinct url)`, hash length 64. Run the same crawl again: row count unchanged, `max(crawled_at)` newer. Then the browser check (headless Chromium) of the same crawl.

- [ ] **Step 6: Commit**

```bash
git add services/crawler/src/main.rs docker-compose.yml .env.example services/crawler/README.md README.md
git commit -m "Crawler saves pages to Postgres on every crawl"
```
