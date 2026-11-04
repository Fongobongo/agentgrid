//! Boot-level regression for `AGENTGRID_ENV` validation.
//!
//! This lives in its own integration-test binary on purpose: the variable is
//! process-global, so mutating it inside `tests/api/main.rs` would race the
//! dozens of tests in that binary which read it.

use agentgrid_control_plane::AppState;

#[tokio::test]
async fn a_misspelled_deploy_env_refuses_to_boot_but_valid_modes_still_work() {
    // One test function, sequential: the harness runs tests in parallel and
    // these assertions share the process environment.
    std::env::set_var("AGENTGRID_ENV", "prodution");
    // `Arc<AppState>` has no `Debug`, so `expect_err` will not compile here.
    let err = AppState::open_temp().await.err().expect(
        "a misspelled AGENTGRID_ENV must stop boot instead of silently disabling \
         the /metrics policy, Secure cookies and the weak-JWT-secret gate",
    );
    assert!(
        err.to_string().contains("AGENTGRID_ENV"),
        "the startup error should name the offending variable: {err:#}"
    );

    // A valid production boot still works (it also needs a stable JWT secret).
    std::env::set_var("AGENTGRID_ENV", "production");
    std::env::set_var(
        "AGENTGRID_JWT_SECRET",
        "a-stable-32-byte-minimum-test-secret",
    );
    AppState::open_temp()
        .await
        .expect("production is a valid AGENTGRID_ENV value");

    std::env::remove_var("AGENTGRID_ENV");
    std::env::remove_var("AGENTGRID_JWT_SECRET");
}
