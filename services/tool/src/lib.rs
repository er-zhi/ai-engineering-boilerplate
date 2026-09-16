// tool as a library: everything main.rs assembles, exposed for unit- and integration-testing
// directly (main.rs stays a thin binary entry point). Each later task adds its own `pub mod`.

pub mod entity;
pub mod error;
pub mod policy;
pub mod providers;
pub mod service;
pub mod slugs;
#[cfg(feature = "test-support")]
pub mod test_db;
pub mod tools;
