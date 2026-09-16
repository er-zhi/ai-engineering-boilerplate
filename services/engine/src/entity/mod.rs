// Everything under `engine::entity::` is created and kept in step by SeaORM's schema-sync: that
// module path is exactly what `get_schema_registry("engine::entity::*")` globs, so a new entity
// added here is synced with no further wiring.
//
// The fifth entity, `crate::execution_event`, is deliberately *outside* this module — its table is
// a partitioned parent that schema-sync can neither create nor leave alone. See the comment at the
// top of `execution_event.rs`.

pub mod checkpoint;
pub mod execution;
pub mod graph;
pub mod schedule;
