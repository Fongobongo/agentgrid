//! Provisioning from the panel: the gate, and the fact that it is closed.
//!
//! The install itself spawns ssh/scp, so the happy path is not exercised here —
//! it is verified against a live control plane (see `docs/agent-ops.md`). What
//! this file pins is the part that must never regress: with the flag unset the
//! endpoint refuses, and it says which flag to set. A control plane that can
//! ssh into hosts is a different threat model from one that cannot, so "off"
//! has to be the default and it has to be visible in the API, not only in a doc.

use crate::helpers::*;

fn body_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap()
}

#[tokio::test]
async fn provisioning_is_disabled_by_default_and_says_so() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/provisioning", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let info: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(info["enabled"], serde_json::json!(false));
    // The UI renders this verbatim, so it must name the variable to set.
    let reason = info["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("AGENTGRID_PROVISION_ENABLED"),
        "reason must tell the operator what to set, got: {reason:?}"
    );
}

#[tokio::test]
async fn install_is_refused_with_403_when_disabled() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(post_auth(
            "/v1/node-installs",
            body_json(&serde_json::json!({
                "host": "root@10.0.0.5",
                "server": "http://cp.example.com:7800"
            })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let err: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("AGENTGRID_PROVISION_ENABLED"),
        "403 must explain the gate, got: {err}"
    );
}

/// A job id that never existed must not read as "still running" — the UI polls
/// until it sees a terminal state, so a silent 200 with an empty body would
/// leave a spinner forever.
#[tokio::test]
async fn unknown_install_job_is_404() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(get_auth("/v1/node-installs/does-not-exist", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The list endpoint is what the panel shows after a reload; with nothing ever
/// installed it must be an empty array, not a 404 or a null.
#[tokio::test]
async fn install_list_is_empty_and_not_an_error() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(get_auth("/v1/node-installs", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let jobs: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(jobs, serde_json::json!([]));
}

/// `GET /v1/cluster/status` is the operator's way to the peer table: the peer
/// RPC route is behind the cluster token, which no panel has. Single mode has
/// no peers, and the route must not leak the cluster's state to an
/// unauthenticated caller.
#[tokio::test]
async fn cluster_status_is_operator_readable_and_empty_in_single_mode() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/cluster/status", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(v["role"], serde_json::json!("single"));
    assert_eq!(v["peers"], serde_json::json!([]));

    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/v1/cluster/status")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A host that was configured and will never arrive has to be removable. An
/// `issued` row is exempt from retention on purpose — it is the answer to "was
/// this host ever configured?" — so without this endpoint the only way to clear
/// a decommissioned host's record is hand-written SQL, and the panel keeps a
/// row nobody can act on.
#[tokio::test]
async fn an_install_record_can_be_forgotten() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    // The mint is what records the intent: naming the host is the whole point.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/enrollment-token",
            body_json(&serde_json::json!({
                "host": "root@10.0.0.44",
                "name": "dead-1"
            })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/node-installs", &tok))
        .await
        .unwrap();
    let jobs: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let id = jobs[0]["id"].as_str().unwrap().to_string();
    assert_eq!(jobs[0]["state"], serde_json::json!("issued"));
    assert_eq!(jobs[0]["name"], serde_json::json!("dead-1"));

    let resp = app
        .clone()
        .oneshot(delete_auth(&format!("/v1/node-installs/{id}"), &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(get_auth(&format!("/v1/node-installs/{id}"), &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/node-installs", &tok))
        .await
        .unwrap();
    let jobs: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(jobs, serde_json::json!([]));
}

/// Deleting something that was never there is a 404, not a 204: the CLI and the
/// panel both tell the operator "removed" or "no such record", and a silent
/// success on a typo would leave the row they meant to delete in place.
#[tokio::test]
async fn forgetting_an_unknown_record_is_404() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(delete_auth("/v1/node-installs/no-such-id", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The command an operator pastes on the new host. Two properties matter and
/// neither is cosmetic: it has to name the token it minted, and it must not
/// carry a `--server` when the host can reach the control plane — the script's
/// own default is the URL this control plane serves itself with, which is the
/// right answer until a relay is involved.
#[tokio::test]
async fn a_join_command_names_the_control_plane_when_no_relay_is_asked_for() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;

    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/join-command",
            body_json(&json!({ "name": "web-3", "host": "root@10.0.0.7" })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let out: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let token = out["token"].as_str().expect("a token");
    let command = out["command"].as_str().expect("a command");
    assert!(command.contains("/v1/node/join.sh"), "got: {command}");
    assert!(
        command.contains(token),
        "the command must spend its own token"
    );
    assert!(
        !command.contains("--server"),
        "a direct join takes the script's default server, got: {command}"
    );
    assert_eq!(out["via"], serde_json::Value::Null);
    assert_eq!(out["server"], out["server"].as_str().unwrap_or(""));

    // The record exists: a host that is configured and never arrives has to be
    // a fact in the database, not a memory of the request.
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT name, state FROM node_installs")
        .fetch_all(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "web-3");
    assert_eq!(rows[0].1, "issued");
}

/// A host that cannot reach the control plane is joined through a sibling.
/// The relay is the server for **both** halves of the line — the script is
/// fetched through it and the daemon keeps dialling it — and `--server` is
/// spelled out even though the fetched script would default to it, because
/// `AGENTGRID_PUBLIC_URL` overrides that default with the one address the new
/// host cannot reach.
#[tokio::test]
async fn a_join_through_a_relay_names_the_relay_as_the_server() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;
    let (node_id, _cred) = enroll(&app, "relay-a", vec!["mock".into()], vec!["*".into()]).await;
    sqlx::query("UPDATE nodes SET relay_url = ? WHERE id = ?")
        .bind("http://node-a.internal:7900")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/join-command",
            body_json(&json!({ "name": "behind-nat", "via": "relay-a" })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let out: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    assert_eq!(out["server"], "http://node-a.internal:7900");
    assert_eq!(out["via"]["name"], "relay-a");
    assert_eq!(out["via"]["relay_url"], "http://node-a.internal:7900");
    let command = out["command"].as_str().unwrap();
    assert_eq!(
        command.matches("http://node-a.internal:7900").count(),
        2,
        "the relay is both where the script is fetched and what the daemon dials: {command}"
    );
    assert!(
        command.contains("--server http://node-a.internal:7900"),
        "an explicit --server survives AGENTGRID_PUBLIC_URL: {command}"
    );
    assert!(
        !command.contains("/v1/nodes/join-command"),
        "the command must fetch the script, not this endpoint: {command}"
    );
}

/// `probe: true` puts `--probe` in the line, and nothing else changes.
///
/// It is the cheapest way to find out that a relay does not actually forward,
/// so it has to be reachable without editing the command by hand — but it is
/// asked for, never assumed: it costs a round trip and a bundle checksum on a
/// path that is often already known to work.
#[tokio::test]
async fn a_join_can_ask_the_host_to_probe_first() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;

    let ask = |app: axum::Router, probe: bool| {
        let body = if probe {
            json!({ "name": "web-3", "probe": true })
        } else {
            json!({ "name": "web-3" })
        };
        let tok = tok.clone();
        async move {
            let resp = app
                .oneshot(post_auth("/v1/nodes/join-command", body_json(&body), &tok))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let out: serde_json::Value =
                serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            out["command"].as_str().unwrap().to_string()
        }
    };

    let asked = ask(app.clone(), true).await;
    assert!(
        asked.ends_with(" --probe"),
        "the flag has to be the last argument, after --token: {asked}"
    );
    assert_eq!(asked.matches("--probe").count(), 1);

    let plain = ask(app, false).await;
    assert!(
        !plain.contains("--probe"),
        "a probe is a choice, not a default: {plain}"
    );
}

/// Asking for a relay that cannot relay is the one case where falling back to
/// a direct join would be actively harmful: the operator asked *because* the
/// host cannot reach the control plane, so a silently direct command is a
/// command that fails on a machine they cannot inspect.
#[tokio::test]
async fn a_node_that_is_not_a_relay_cannot_carry_a_join() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;
    let (_id, _cred) = enroll(&app, "plain-node", vec!["mock".into()], vec!["*".into()]).await;
    // `enroll` mints and spends a token of its own; the count has to be read
    // before the request to mean anything.
    let tokens_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM enrollment_tokens")
        .fetch_one(&state.store.pool)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/join-command",
            body_json(&json!({ "name": "behind-nat", "via": "plain-node" })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = String::from_utf8(
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        body.contains("AGENTGRID_RELAY_LISTEN"),
        "the refusal must name what to set: {body}"
    );

    // Nothing was minted: a token that is spent nowhere is worse than a refusal.
    let tokens_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM enrollment_tokens")
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM node_installs")
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a refused join must not leave an install record");
    assert_eq!(
        tokens_after, tokens_before,
        "a refused join must not leave a live token"
    );
}

/// A revoked node's credential is dead, so it cannot reach this control plane —
/// which means it cannot carry anyone else's traffic either. Building the
/// command anyway moves the failure onto the host the operator cannot log into,
/// which is the same reason an unknown `--via` is not quietly replaced by a
/// direct join.
#[tokio::test]
async fn a_revoked_relay_cannot_carry_a_join() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;
    let (node_id, _cred) = enroll(&app, "relay-a", vec!["mock".into()], vec!["*".into()]).await;
    sqlx::query("UPDATE nodes SET relay_url = ?, status = 'revoked' WHERE id = ?")
        .bind("http://node-a.internal:7900")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/join-command",
            body_json(&json!({ "name": "behind-nat", "via": "relay-a" })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = String::from_utf8(
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        body.contains("revoked"),
        "the refusal must say why, not just 'not a relay': {body}"
    );
    assert!(
        body.contains("ag nodes rekey"),
        "a revoked node comes back by re-keying, so name the repair: {body}"
    );
}

/// "Not a relay" and "a relay nobody can dial" need different fixes. Telling
/// the operator to set `AGENTGRID_RELAY_LISTEN` when they already did is how a
/// half-configured relay stays half-configured: the variable is set, the daemon
/// is running, and nothing works.
#[tokio::test]
async fn a_relay_that_listens_but_announces_nothing_says_so() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;
    let (node_id, _cred) = enroll(&app, "relay-a", vec!["mock".into()], vec!["*".into()]).await;
    sqlx::query("UPDATE nodes SET relay_listen = ? WHERE id = ?")
        .bind("0.0.0.0:7900")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/join-command",
            body_json(&json!({ "name": "behind-nat", "via": "relay-a" })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = String::from_utf8(
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        body.contains("0.0.0.0:7900"),
        "the refusal should quote the bind address it saw: {body}"
    );
    assert!(
        body.contains("AGENTGRID_RELAY_ADVERTISE"),
        "the missing variable is the advertise one: {body}"
    );
    assert!(
        !body.contains("set AGENTGRID_RELAY_LISTEN"),
        "sending them to the variable they already set is the bug this fixes: {body}"
    );
}

/// `offline` is transient — a relay that just went quiet may be back by the time
/// the line is pasted — so it is a warning, not a refusal. But it has to reach
/// the client: the command is minted for a host nobody can inspect.
#[tokio::test]
async fn an_offline_relay_is_carried_with_its_status() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;
    let (node_id, _cred) = enroll(&app, "relay-a", vec!["mock".into()], vec!["*".into()]).await;
    sqlx::query("UPDATE nodes SET relay_url = ?, status = 'offline' WHERE id = ?")
        .bind("http://node-a.internal:7900")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/join-command",
            body_json(&json!({ "name": "behind-nat", "via": "relay-a" })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an offline relay is still offered: it may be back before the line is used"
    );
    let out: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(out["via"]["status"], "offline");
    assert!(
        !out["via"]["last_heartbeat_at"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "the warning needs the age to be useful: {out}"
    );
}

#[tokio::test]
async fn a_join_through_a_node_that_does_not_exist_is_404() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(post_auth(
            "/v1/nodes/join-command",
            body_json(&json!({ "name": "behind-nat", "via": "no-such-node" })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The name is what enrollment matches the install record on. A nameless join
/// would mint a token and leave a row nobody can close when the host arrives,
/// which is the permanently-open row this table was fixed to stop producing.
#[tokio::test]
async fn a_join_command_without_a_name_is_refused() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;

    for body in [json!({}), json!({ "name": "  " })] {
        let resp = app
            .clone()
            .oneshot(post_auth("/v1/nodes/join-command", body_json(&body), &tok))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "body: {body}");
    }
}
