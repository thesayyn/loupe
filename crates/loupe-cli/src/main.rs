//! `loupe` CLI — debugging entry point.
//!
//! Phase 0 only: the binary exists and links against `loupe-core`.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("loupe=info")),
        )
        .init();
    Ok(())
}
