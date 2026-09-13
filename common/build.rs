fn main() {
    connectrpc_build::Config::new()
        .files(&[
            "proto/crawler.proto",
            "proto/knowledge_base.proto",
            "proto/llm_router.proto",
        ])
        .includes(&["proto"])
        .include_file("_connectrpc.rs")
        .compile()
        .unwrap();
}
