---
name: gate-common
description: Use when adding entities, proto messages, shared errors, test helpers, or utilities in this boilerplate, or when the user mentions duplicate types across services or cross-service contracts.
---

# Common

Types gate. One canonical definition per concept; compose from what exists.

The rules — where shared code goes, the type chain, proto reuse, and the `CacheStore` trait — live in [common/README.md](../../../../common/README.md); this gate checks a diff against them. Report findings using the template in [code-review](../SKILL.md).

## Critical Findings

| Symptom | Fix |
|---|---|
| Struct duplicating an entity's fields | Return the entity, or compose: `struct PageResponse { page: PageEntity }` |
| Hand-written struct matching a proto message | Use the generated type |
| Copied field block in a new proto | `import` the existing message |
| Same struct defined in two services | Move to `common/` or route via proto |
| Service importing a sibling's module | gRPC + `common/` |
| Test setup copy-pasted across services | Extract to `common/src/test_db.rs` behind the `test-support` feature |

## Example

```rust
pub async fn fetch_page_by_url(&self, url: &str) -> Result<PageEntity, ServiceError> {
    self.repository.find_by_url(url).await
}
```

The entity flows through every layer inside the service, and the mapper turns it into proto at the boundary, so callers never see the row. The version to reject declares a `PageDto` that restates every entity field and copies them over by hand.
