// One JSON line per event on stderr, with the crate that logged it as `target`, so a service is told apart by name in shared logs.

pub fn init() {
    tracing_subscriber::fmt()
        .json()
        .with_target(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .with_writer(std::io::stderr)
        .init();
}
