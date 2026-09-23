// Connect service stubs and message types generated from common/proto, shared by every service.

pub mod agent_replies;
pub mod execution_input;
#[cfg(feature = "extract")]
pub mod extract;
pub mod logging;
#[cfg(feature = "partition")]
pub mod partition;
pub mod principal;
#[cfg(feature = "test-support")]
pub mod test_db;
pub mod tool_schema;

#[allow(clippy::too_many_lines)]
pub mod proto {
    connectrpc::include_generated!();
}
