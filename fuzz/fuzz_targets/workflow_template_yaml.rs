#![no_main]
//! Fuzz the workflow template YAML parser (audit phase C).
//! `WorkflowTemplate::from_yaml` runs on the CP's create-workflow route for
//! `content-type: */yaml*` bodies — directly reachable untrusted input over
//! the authenticated API. The parser (serde_yaml + template validation)
//! must survive arbitrary bytes without panicking or hanging.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(t) = agentgrid_common::WorkflowTemplate::from_yaml(s) {
            // Round-trip the accepted template through the JSON rendering
            // used by the API response path.
            let _ = serde_json::to_string(&t);
        }
    }
});
