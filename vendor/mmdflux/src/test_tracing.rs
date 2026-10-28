//! Test-only tracing subscriber initialization for crate-local tests.

use std::sync::OnceLock;

use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

static INIT: OnceLock<()> = OnceLock::new();

#[ctor::ctor]
fn init_tracing_for_tests() {
    INIT.get_or_init(|| {
        // The ctor runs once at process startup, so per-test RUST_LOG changes
        // cannot reconfigure this subscriber. Scoped with_default subscribers
        // are also thread-local and remain separate from this global default.
        let filter = std::env::var_os("RUST_LOG")
            .map(|_| EnvFilter::from_default_env())
            .unwrap_or_else(|| EnvFilter::new("off"));

        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_test_writer())
            .try_init();
    });
}
