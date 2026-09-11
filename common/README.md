# Common

Cross-service shared code. Services depend on `common/` — never on each other's internal modules.

```
common/
├── proto/    # gRPC contracts (tonic/prost)
├── errors/   # shared error codes + gRPC status mapping
├── cache/    # CacheStore trait: Postgres now, Redis later
├── test/     # fixtures, mock builders, testcontainers setup
└── utils/    # pure helpers, no domain logic
```

Goes here only if 2+ services use it. Service-specific code stays in the service.

## Type Chain

```
proto message  ←→  generated Rust  ←→  entity
   canonical          never bypass      canonical
 across services                       within service
```

- **Entity is canonical inside a service** — repo, service layer, and mappers all use it. No parallel struct with the same fields.
- **Proto is canonical across services** — use generated types, never a hand-written mirror.
- **Compose, don't duplicate** — wrap or reference existing types instead of redefining fields.
- **One mapper module per service** for entity ↔ proto.

### Proto Reuses Messages

```protobuf
// proto/common.proto
message PageMetadata { string url = 1; string title = 2; int64 crawled_at = 3; }

// proto/crawler.proto
import "common.proto";
message CrawlPageRequest { PageMetadata metadata = 1; string main_content = 2; }
```

Never copy a field block into a new proto — `import` it. Proto changes are versioned; breaking changes need coordination.

## Errors

Shared codes in `common/errors/`. Service-specific errors map to them at the gRPC boundary. No duplicate error enums across services.

## Cache (Postgres, Redis-ready)

TTL keys, rate limits, and job locks use Postgres behind a trait. Services depend on the trait, never on the backend.

```rust
#[async_trait]
pub trait CacheStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError>;
    async fn set(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), CacheError>;
    async fn delete(&self, key: &str) -> Result<(), CacheError>;
}
```

Backend switches via `CACHE_BACKEND=postgres|redis`. Cache table lives in the service's own schema (e.g. `crawler.cache_entries`): `key varchar(512)`, `value bytea`, `expires_at timestamptz`.
