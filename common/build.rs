fn main() {
    connectrpc_build::Config::new()
        .files(&["proto/crawler.proto"])
        .includes(&["proto"])
        .include_file("_connectrpc.rs")
        .compile()
        .unwrap();
}
