---
name: gate-common
description: Use when adding entities, proto messages, shared errors, test helpers, or utilities in this boilerplate, or when the user mentions duplicate types across services or cross-service contracts.
---

# Common

Types gate. One canonical definition per concept; compose from what exists.

Conventions and the `CacheStore` trait live in [common/README.md](../../../common/README.md). Report findings using the template in [code-review](../SKILL.md).

## Where Code Belongs

```
Used by 2+ services?  → common/ (proto | errors | cache | test | utils)
Crosses a boundary?   → common/proto/
Persists to DB?       → entity in the owning service
Otherwise             → stays in that service
```

## Rules

1. Entity is canonical within a service; proto is canonical across services.
2. New types wrap or reference existing ones — never redefine the same fields.
3. Use generated tonic/prost types, never a hand-written mirror struct.
4. New `.proto` files `import` existing messages instead of copying field blocks.
5. One mapper module per service for entity ↔ proto.

## Critical Findings

| Symptom | Fix |
|---|---|
| Struct duplicating an entity's fields | Return the entity, or compose: `struct PageResponse { page: PageEntity }` |
| Hand-written struct matching a proto message | Use the generated type |
| Copied field block in a new proto | `import` the existing message |
| Same struct defined in two services | Move to `common/` or route via proto |
| Service importing a sibling's module | gRPC + `common/` |
| Test setup copy-pasted across services | Extract to `common/test/` |

## Example

```rust
// Good — entity flows through every layer
pub async fn get_page(&self, url: &str) -> Result<PageEntity, ServiceError> {
    self.repository.find_by_url(url).await
}

// Bad — PageDto restates every entity field, then copies them by hand
```
