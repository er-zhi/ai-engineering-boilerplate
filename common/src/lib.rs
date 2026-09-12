// Connect service stubs and message types generated from common/proto, shared by every service.

pub mod llm;
pub mod logging;
#[cfg(feature = "test-support")]
pub mod test_db;

pub mod proto {
    connectrpc::include_generated!();
}
