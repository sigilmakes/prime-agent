//! Bundled private Prime Inference templates and offline policy.
//! Discovery metadata and private authorization belong to pa-models.

use pa_types::ai::Model;

/// The bundled private model table is a template, not an entitlement grant.
#[must_use]
pub fn get_private_prime_inference_models() -> Vec<Model> {
    super::prime_inference::private_prime_inference_models()
}

/// `PI_OFFLINE=1/true/yes` disables network refreshes.
pub fn is_offline_mode_enabled() -> bool {
    match std::env::var("PI_OFFLINE") {
        Ok(value) => {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}
