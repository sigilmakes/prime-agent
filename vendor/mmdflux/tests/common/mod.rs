//! Common integration-test setup.

use std::sync::OnceLock;

use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

static INIT: OnceLock<()> = OnceLock::new();

#[ctor::ctor]
fn init_tracing_for_tests() {
    INIT.get_or_init(|| {
        // The ctor reads RUST_LOG once when the test process starts. Tests that
        // need scoped subscribers should still use with_default explicitly.
        let filter = std::env::var_os("RUST_LOG")
            .map(|_| EnvFilter::from_default_env())
            .unwrap_or_else(|| EnvFilter::new("off"));

        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_test_writer())
            .try_init();
    });
}
