#![no_main]
//! Fuzz the external-policy verdict parser (audit phase C). `PolicyVerdict`
//! JSON arrives from the STDOUT OF AN EXTERNAL BINARY the operator configured
//! (`ExternalPolicyProvider::evaluate`, policy.rs) — attacker-influenced when
//! a compromised policy provider or a tampered binary emits garbage. The
//! parser must handle arbitrary bytes without panicking, hanging, or
//! allocating unboundedly.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The provider path parses the raw stdout bytes directly.
    let _ = serde_json::from_slice::<agentgrid_common::PolicyVerdict>(data);
    // The string-based entry (used by tests / future callers) must agree on
    // well-formed UTF-8.
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = serde_json::from_str::<agentgrid_common::PolicyVerdict>(s);
    }
});
