#[cfg(test)]
mod tests {
    use crate::evals::{case_command, probe_evals};
    use std::time::Duration;

    #[test]
    fn case_command_parses_plain_single_and_double_quoted() {
        let plain = "id: x\ncommand: cargo test --release\n";
        assert_eq!(case_command(plain).unwrap(), "cargo test --release");
        let single = "id: x\ncommand: 'cargo test'\n";
        assert_eq!(case_command(single).unwrap(), "cargo test");
        let double = "id: x\ncommand: \"cargo test\"\n";
        assert_eq!(case_command(double).unwrap(), "cargo test");
    }

    #[test]
    fn case_command_missing_is_error() {
        assert!(case_command("id: x\n").is_err());
    }

    #[tokio::test]
    async fn probe_evals_passes_when_no_cases() {
        let dir = std::env::temp_dir().join(format!(
            "ag-evals-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let out = probe_evals(&dir, Duration::from_millis(10)).await.unwrap();
        assert!(out.ok);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn probe_evals_failing_case_sets_ok_false() {
        // A `false` case fails the suite; output includes the case log tail.
        let dir = std::env::temp_dir().join(format!(
            "ag-evals-fail-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(dir.join(".agentgrid/evals"))
            .await
            .unwrap();
        tokio::fs::write(
            dir.join(".agentgrid/evals/case-false.yaml"),
            "id: x\ncommand: \"echo no && exit 1\"\n",
        )
        .await
        .unwrap();
        let out = probe_evals(&dir, Duration::from_secs(5)).await.unwrap();
        assert!(!out.ok, "failing case must mark the suite failed");
        assert!(out.log.contains("no"), "log keeps the command output");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn probe_evals_all_passing() {
        let dir = std::env::temp_dir().join(format!(
            "ag-evals-ok-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(dir.join(".agentgrid/evals"))
            .await
            .unwrap();
        tokio::fs::write(
            dir.join(".agentgrid/evals/case-a.yaml"),
            "id: a\ncommand: \"echo ok\"\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.join(".agentgrid/evals/case-b.yaml"),
            "id: b\ncommand: \"true\"\n",
        )
        .await
        .unwrap();
        let out = probe_evals(&dir, Duration::from_secs(5)).await.unwrap();
        assert!(out.ok, "all-passing suite returns ok=true: {}", out.log);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
