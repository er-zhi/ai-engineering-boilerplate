// Connect service stubs and message types generated from common/proto, shared by every service.

pub mod execution_input;
pub mod extract;
pub mod logging;
pub mod principal;
#[cfg(feature = "test-support")]
pub mod test_db;

#[allow(clippy::too_many_lines)]
pub mod proto {
    connectrpc::include_generated!();
}
