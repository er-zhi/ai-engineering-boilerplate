// The TaskExecutor implementations engine can run a node with.

use std::time::Duration;

use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};

pub mod llm;
pub mod tool;

pub fn grpc_client<C>(
    url_var: &str,
    url: &str,
    timeout: Duration,
    build: impl Fn(HttpClient, ClientConfig) -> C,
) -> Result<C, String> {
    let target = url
        .parse()
        .map_err(|error| format!("could not parse {url_var} {url:?}: {error}"))?;
    Ok(build(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new(target)
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(timeout)
            .proto(),
    ))
}
