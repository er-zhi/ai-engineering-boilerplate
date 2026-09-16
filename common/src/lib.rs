// Connect service stubs and message types generated from common/proto, shared by every service.

pub mod cache;
pub mod extract;
pub mod llm;
pub mod logging;
pub mod principal;
#[cfg(feature = "test-support")]
pub mod test_db;

#[allow(clippy::too_many_lines)]
pub mod proto {
    connectrpc::include_generated!();
}
