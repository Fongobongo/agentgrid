#![no_main]
//! Fuzz the handoff-package JSON parser (audit phase C). `HandoffPackage`
//! payloads flow between workflow steps through the message mailbox
//! (workflow.rs: `serde_json::from_str::<HandoffPackage>(&m.payload)`) and
//! originate from agent-produced plan/output events — untrusted upstream
//! input. The parse-and-render round trip must not panic on arbitrary bytes.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(p) = serde_json::from_str::<agentgrid_common::HandoffPackage>(s) {
            let _ = serde_json::to_string(&p);
        }
    }
});
