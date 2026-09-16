fn main() -> Result<(), Box<dyn std::error::Error>> {
    connectrpc_build::Config::new()
        .files(&[
            "proto/crawler/v1/crawler.proto",
            "proto/engine/v1/engine.proto",
            "proto/knowledge_base/v1/knowledge_base.proto",
            "proto/llm_router/v1/llm_router.proto",
        ])
        .includes(&["proto"])
        .include_file("_connectrpc.rs")
        .compile()?;
    Ok(())
}
