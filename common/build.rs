fn main() -> Result<(), Box<dyn std::error::Error>> {
    connectrpc_build::Config::new()
        .files(&[
            "proto/chat/v1/chat.proto",
            "proto/crawler/v1/crawler.proto",
            "proto/engine/v1/engine.proto",
            "proto/knowledge_base/v1/knowledge_base.proto",
            "proto/llm_router/v1/llm_router.proto",
            "proto/tools/v1/tools.proto",
        ])
        .includes(&["proto"])
        .include_file("_connectrpc.rs")
        .compile()?;
    Ok(())
}
