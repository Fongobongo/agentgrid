//! Opening the control plane's sealed adapter-env entries (ADR 0028).
//!
//! The CP stores only ciphertext, so the plaintext exists on this side alone:
//! it is produced here, kept in `cfg.managed_adapter_env`, and handed to the
//! attempt's child process. Nothing writes it back.

use agentgrid_common::seal::{self, entry_aad};
use agentgrid_common::{AdapterEnvEntry, SealedAdapterEnvEntry};

/// Open every sealed entry, dropping (with a warning) the ones this node
/// cannot open.
///
/// A failure here has exactly two causes, and both are operator-visible: the
/// blob was sealed to a key this node no longer has (its credential file was
/// recreated), or it was sealed for a different entry. Either way the value is
/// unusable, and continuing without it is better than failing the poll — the
/// adapter falls back to whatever `AGENTGRID_ADAPTER_ENV` gave it locally.
pub fn open_all(
    node_id: &str,
    key_secret: &str,
    entries: &[SealedAdapterEnvEntry],
) -> Vec<AdapterEnvEntry> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        match seal::open(
            key_secret,
            &e.sealed,
            &entry_aad(node_id, &e.adapter, &e.key),
        ) {
            Ok(value) => out.push(AdapterEnvEntry {
                adapter: e.adapter.clone(),
                key: e.key.clone(),
                value,
            }),
            Err(err) => tracing::warn!(
                adapter = %e.adapter,
                key = %e.key,
                "cannot open a sealed adapter-env value ({err}); re-save it with \
                 `ag adapter-env set` so it is sealed to this node's current key"
            ),
        }
    }
    out
}

/// Open `entries` and install them as the node's managed adapter env.
///
/// Shared by the poll loop and the WS channel so the two transports cannot
/// drift on the one decision that matters: **empty + `complete` is a
/// revocation** (install nothing), while empty + not-complete — or a
/// non-empty list where every blob failed to open — keeps the last good
/// snapshot instead of wiping the node's config.
pub fn apply(
    managed: &std::sync::Mutex<Vec<AdapterEnvEntry>>,
    node_id: &str,
    key_secret: Option<&str>,
    entries: &[SealedAdapterEnvEntry],
    complete: bool,
) {
    let Some(secret) = key_secret else {
        if !entries.is_empty() {
            tracing::warn!(
                "control plane sent sealed adapter env, but this node has no sealing key \
                 — ignored (re-enroll to get one)"
            );
        }
        return;
    };
    let opened = open_all(node_id, secret, entries);
    if !opened.is_empty() || (complete && entries.is_empty()) {
        *managed.lock().unwrap() = opened;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_for(
        node: &str,
        adapter: &str,
        key: &str,
        value: &str,
        public: &str,
    ) -> SealedAdapterEnvEntry {
        SealedAdapterEnvEntry {
            adapter: adapter.into(),
            key: key.into(),
            sealed: seal::seal(public, value, &entry_aad(node, adapter, key)).unwrap(),
        }
    }

    #[test]
    fn a_node_opens_what_was_sealed_for_it() {
        let (public, private) = seal::generate_keypair().unwrap();
        let entries = vec![
            sealed_for("n1", "claude", "ANTHROPIC_API_KEY", "sk-ant-1", &public),
            sealed_for("n1", "*", "GITHUB_TOKEN", "ghp-2", &public),
        ];
        let opened = open_all("n1", &private, &entries);
        assert_eq!(opened.len(), 2);
        assert_eq!(opened[0].value, "sk-ant-1");
        assert_eq!(opened[1].adapter, "*");
    }

    #[test]
    fn blobs_sealed_for_another_node_are_skipped_not_fatal() {
        let (public_a, _private_a) = seal::generate_keypair().unwrap();
        let (_public_b, private_b) = seal::generate_keypair().unwrap();
        let entries = vec![
            sealed_for("n1", "claude", "ANTHROPIC_API_KEY", "for-a", &public_a),
            sealed_for("n1", "claude", "ANTHROPIC_BASE_URL", "https://x", &public_a),
        ];
        // Every entry fails: the result is empty, not a panic and not an
        // error that would take the poll loop down.
        assert!(open_all("n1", &private_b, &entries).is_empty());
    }

    #[test]
    fn a_complete_empty_list_is_a_revocation() {
        let managed = std::sync::Mutex::new(vec![AdapterEnvEntry {
            adapter: "*".into(),
            key: "K".into(),
            value: "stale".into(),
        }]);
        // complete + empty: the operator removed it — drop it.
        apply(&managed, "n1", Some("not-a-key"), &[], true);
        assert!(managed.lock().unwrap().is_empty());
    }

    #[test]
    fn an_incomplete_empty_list_keeps_the_last_good_snapshot() {
        let managed = std::sync::Mutex::new(vec![AdapterEnvEntry {
            adapter: "*".into(),
            key: "K".into(),
            value: "keep".into(),
        }]);
        // A protocol-1 CP sends an empty list on every poll; that is not a
        // revocation and must not wipe the node's config.
        apply(&managed, "n1", Some("not-a-key"), &[], false);
        assert_eq!(managed.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_replayed_entry_is_rejected() {
        let (public, private) = seal::generate_keypair().unwrap();
        // Sealed as ANTHROPIC_API_KEY, presented as GITHUB_TOKEN.
        let blob = sealed_for("n1", "claude", "ANTHROPIC_API_KEY", "v", &public);
        let replayed = vec![SealedAdapterEnvEntry {
            adapter: blob.adapter.clone(),
            key: "GITHUB_TOKEN".into(),
            sealed: blob.sealed.clone(),
        }];
        assert!(open_all("n1", &private, &replayed).is_empty());
    }
}
