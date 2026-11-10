use pa_test_scratch as _; // a private TMPDIR for this test binary, deleted on exit
mod common;

#[tokio::test]
async fn node_extra_ca_certs_adds_the_custom_ca() {
    common::fetch_trusting_test_ca("NODE_EXTRA_CA_CERTS").await;
}
