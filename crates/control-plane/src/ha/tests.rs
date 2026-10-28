//! Phase 2 unit/integration tests: vote & heartbeat rules, snapshot round
//! trip into a second store, epoch/seq bookkeeping and manual promotion.

use std::sync::Arc;

use super::config::{HaConfig, HaMode, NodeRole, ReplBackend, RpoMode, S3Config};
use super::protocol::{HeartbeatRequest, VoteRequest};
use super::role::{HaRole, HaRuntime};
use super::{election, snapshot};
use crate::store::Store;

/// Throwaway cluster token shared by the in-process HA tests below. Never a
/// credential: it only authenticates processes this module starts on
/// 127.0.0.1. Declared once so the secret scanner has a single place to
/// allow (inline, because fingerprints are commit-scoped).
const TEST_CLUSTER_TOKEN: &str = "0123456789abcdef0123456789abcdef"; // gitleaks:allow

fn rt(node_id: &str) -> Arc<HaRuntime> {
    rt_with_election_timeout(node_id, std::time::Duration::from_millis(1500))
}

/// Same cluster, custom election timeout — the leader-stickiness window is one
/// election timeout, so a short one lets a test age leader contact without
/// sleeping for seconds.
fn rt_with_election_timeout(
    node_id: &str,
    election_timeout: std::time::Duration,
) -> Arc<HaRuntime> {
    let mut c = HaConfig::single();
    c.mode = HaMode::Ha;
    c.role = NodeRole::Data;
    c.node_id = node_id.to_string();
    c.cluster_id = "prod".into();
    c.cluster_token = TEST_CLUSTER_TOKEN.into();
    c.advertise_url = format!("http://{node_id}:7800");
    c.allow_plaintext = true;
    c.data_replicas = 3;
    c.election_timeout = election_timeout;
    c.peers = vec![
        super::config::HaPeer {
            id: "cp-b".into(),
            url: "http://cp-b:7800".into(),
        },
        super::config::HaPeer {
            id: "cp-c".into(),
            url: "http://cp-c:7800".into(),
        },
    ];
    HaRuntime::new(c).expect("runtime")
}

fn rt_s3(node_id: &str, endpoint: &str, rpo: RpoMode) -> Arc<HaRuntime> {
    let mut c = HaConfig::single();
    c.mode = HaMode::Ha;
    c.role = NodeRole::Data;
    c.node_id = node_id.to_string();
    c.cluster_id = "prod".into();
    c.cluster_token = TEST_CLUSTER_TOKEN.into();
    c.advertise_url = format!("http://{node_id}:7800");
    c.allow_plaintext = true;
    c.data_replicas = 3;
    c.rpo_mode = rpo;
    c.repl_backend = ReplBackend::S3;
    c.s3 = Some(S3Config {
        endpoint: endpoint.to_string(),
        bucket: "agentgrid".into(),
        region: "us-east-1".into(),
        prefix: "ha".into(),
        access_key: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
    });
    c.peers = vec![
        super::config::HaPeer {
            id: "cp-b".into(),
            url: "http://cp-b:7800".into(),
        },
        super::config::HaPeer {
            id: "cp-c".into(),
            url: "http://cp-c:7800".into(),
        },
    ];
    HaRuntime::new(c).expect("runtime")
}

/// Peer transport, incremental async replication, no object store: `publish`
/// keeps the changeset in the rewind buffer and the push to the (unresolvable)
/// peers fails, which is exactly the state a demoted leader has to repair.
#[cfg(feature = "ha-incremental")]
fn rt_async(node_id: &str) -> Arc<HaRuntime> {
    let mut c = HaConfig::single();
    c.mode = HaMode::Ha;
    c.role = NodeRole::Data;
    c.node_id = node_id.to_string();
    c.cluster_id = "prod".into();
    c.cluster_token = TEST_CLUSTER_TOKEN.into();
    c.advertise_url = format!("http://{node_id}:7800");
    c.allow_plaintext = true;
    c.data_replicas = 3;
    c.rpo_mode = RpoMode::Async;
    c.peers = vec![
        super::config::HaPeer {
            id: "cp-b".into(),
            url: "http://127.0.0.1:1".into(),
        },
        super::config::HaPeer {
            id: "cp-c".into(),
            url: "http://127.0.0.1:2".into(),
        },
    ];
    HaRuntime::new(c).expect("runtime")
}

fn vote_req(term: u64, candidate: &str, applied: u64) -> VoteRequest {
    VoteRequest {
        cluster_id: "prod".into(),
        term,
        candidate_id: candidate.into(),
        applied_seq: applied,
        pre_vote: false,
    }
}

fn pre_vote_req(term: u64, candidate: &str, applied: u64) -> VoteRequest {
    VoteRequest {
        pre_vote: true,
        ..vote_req(term, candidate, applied)
    }
}

fn hb(term: u64, leader: &str, epoch: u64) -> HeartbeatRequest {
    HeartbeatRequest {
        cluster_id: "prod".into(),
        term,
        epoch,
        leader_id: leader.into(),
        leader_url: format!("http://{leader}:7800"),
        commit_seq: 0,
    }
}

#[tokio::test]
async fn vote_is_granted_once_per_term() {
    let rt = rt("cp-a");
    let r = election::handle_vote(&rt, None, 5, vote_req(1, "cp-b", 5)).await;
    assert!(r.granted);
    assert_eq!(r.term, 1);
    // Same term, different candidate: denied (durable vote).
    let r = election::handle_vote(&rt, None, 5, vote_req(1, "cp-c", 5)).await;
    assert!(!r.granted);
    // Same candidate may retry.
    let r = election::handle_vote(&rt, None, 5, vote_req(1, "cp-b", 5)).await;
    assert!(r.granted);
    // Higher term resets the vote.
    let r = election::handle_vote(&rt, None, 5, vote_req(2, "cp-c", 5)).await;
    assert!(r.granted);
    assert_eq!(r.term, 2);
}

#[tokio::test]
async fn stale_candidate_is_refused_by_data_voter() {
    let rt = rt("cp-a");
    let r = election::handle_vote(&rt, None, 10, vote_req(1, "cp-b", 9)).await;
    assert!(!r.granted);
    // Equal freshness is acceptable.
    let r = election::handle_vote(&rt, None, 10, vote_req(1, "cp-b", 10)).await;
    assert!(r.granted);
}

#[tokio::test]
async fn old_term_vote_is_rejected() {
    let rt = rt("cp-a");
    let r = election::handle_vote(&rt, None, 1, vote_req(3, "cp-b", 1)).await;
    assert!(r.granted);
    let r = election::handle_vote(&rt, None, 1, vote_req(2, "cp-c", 1)).await;
    assert!(!r.granted);
    assert_eq!(r.term, 3);
}

#[tokio::test]
async fn pre_vote_never_bumps_the_term_or_topples_a_live_leader() {
    let rt = rt("cp-a");
    // cp-b won term 1 and its heartbeat just landed here.
    election::handle_heartbeat(&rt, None, 1, hb(1, "cp-b", 1)).await;
    // cp-c's election timer fired a moment later and it probes for term 2.
    let r = election::handle_vote(&rt, None, 1, pre_vote_req(2, "cp-c", 1)).await;
    assert!(!r.granted, "a live leader must not be disrupted by a probe");
    assert_eq!(
        r.term, 1,
        "a probe must not report the probed term as adopted"
    );
    assert_eq!(rt.shared.term(), 1, "a probe must not persist the term");
    assert_eq!(
        rt.shared.leader().map(|l| l.id),
        Some("cp-b".into()),
        "the incumbent must survive the probe"
    );
}

#[tokio::test]
async fn pre_vote_is_granted_once_the_leader_goes_quiet() {
    let rt = rt_with_election_timeout("cp-a", std::time::Duration::from_millis(50));
    election::handle_heartbeat(&rt, None, 1, hb(1, "cp-b", 1)).await;
    assert!(
        !election::handle_vote(&rt, None, 1, pre_vote_req(2, "cp-c", 1))
            .await
            .granted,
        "contact is fresh: the probe must be refused"
    );
    // A dead leader stops refreshing contact; after one election timeout the
    // probe is granted so the cluster can move on instead of deadlocking.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let r = election::handle_vote(&rt, None, 1, pre_vote_req(2, "cp-c", 1)).await;
    assert!(r.granted, "stickiness must expire with the leader");
    assert_eq!(rt.shared.term(), 1, "even a granted probe persists nothing");
}

#[tokio::test]
async fn pre_vote_refuses_a_candidate_that_is_behind() {
    let rt = rt("cp-a");
    // No leader known, so stickiness does not apply: replication freshness is
    // the only reason to refuse — and it is enough to stop the doomed bid.
    let r = election::handle_vote(&rt, None, 10, pre_vote_req(1, "cp-b", 9)).await;
    assert!(!r.granted);
    let r = election::handle_vote(&rt, None, 10, pre_vote_req(1, "cp-b", 10)).await;
    assert!(r.granted);
}

#[tokio::test]
async fn a_granted_pre_vote_does_not_consume_the_durable_vote() {
    let rt = rt("cp-a");
    assert!(
        election::handle_vote(&rt, None, 1, pre_vote_req(1, "cp-b", 1))
            .await
            .granted
    );
    // A probe that burned the durable vote would deadlock the very round it
    // exists to protect: the candidate would then be refused in its real round.
    assert_eq!(rt.shared.term(), 0, "the probe must not persist a term");
    let r = election::handle_vote(&rt, None, 1, vote_req(1, "cp-c", 1)).await;
    assert!(r.granted, "the real vote must still be available");
}

#[tokio::test]
async fn heartbeat_adopts_leader_and_rejects_older_term() {
    let rt = rt("cp-a");
    let r = election::handle_heartbeat(&rt, None, 4, hb(1, "cp-b", 7)).await;
    assert!(r.ok);
    assert_eq!(rt.shared.leader().unwrap().id, "cp-b");
    assert_eq!(rt.gate.epoch(), 7);
    let r = election::handle_heartbeat(&rt, None, 4, hb(0, "cp-c", 6)).await;
    assert!(!r.ok);
    assert_eq!(rt.shared.leader().unwrap().id, "cp-b");
}

#[tokio::test]
async fn same_term_winner_heartbeat_ends_candidacy() {
    let rt = rt("cp-a");
    // cp-a voted for itself in term 1; cp-b still wins the majority and its
    // heartbeat must be accepted (otherwise both nodes campaign forever).
    let r = election::handle_vote(&rt, None, 1, vote_req(1, "cp-a", 1)).await;
    assert!(r.granted);
    let r = election::handle_heartbeat(&rt, None, 1, hb(1, "cp-b", 1)).await;
    assert!(r.ok);
    assert_eq!(rt.shared.leader().unwrap().id, "cp-b");
}

#[tokio::test]
async fn wrong_cluster_is_rejected() {
    let rt = rt("cp-a");
    let mut req = vote_req(1, "cp-b", 1);
    req.cluster_id = "other".into();
    assert!(!election::handle_vote(&rt, None, 1, req).await.granted);
    let mut h = hb(1, "cp-b", 1);
    h.cluster_id = "other".into();
    assert!(!election::handle_heartbeat(&rt, None, 1, h).await.ok);
}

fn temp_db(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ag-ha-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("control-plane.db")
}

async fn open_store(tag: &str) -> (Store, std::path::PathBuf) {
    let path = temp_db(tag);
    let store = Store::open(path.to_str().unwrap()).await.expect("store");
    (store, path)
}

fn cleanup(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test]
async fn epoch_and_seq_bookkeeping() {
    let (store, path) = open_store("epoch").await;
    assert_eq!(store.ha_epoch().await.unwrap(), 0);
    assert_eq!(store.ha_repl_seq().await.unwrap(), 0);
    let seq = store.promote_epoch(3, 2, "cp-a", true).await.unwrap();
    assert_eq!(seq, 1);
    assert_eq!(store.ha_epoch().await.unwrap(), 3);
    assert_eq!(store.ha_term().await.unwrap(), 2);
    assert_eq!(store.next_repl_seq().await.unwrap(), 2);
    store.pool.close().await;
    cleanup(&path);
}

#[tokio::test]
async fn snapshot_round_trips_into_a_second_store() {
    let (src, src_path) = open_store("snap-src").await;
    src.create_user("alice", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    // The leader persists its epoch before snapshotting; the restored copy
    // must carry that epoch.
    src.promote_epoch(1, 1, "cp-a", true).await.unwrap();
    let runtime = rt("cp-a");
    let manifest = snapshot::create(&runtime, &src).await.unwrap();
    assert!(manifest.size > 0);
    let file = snapshot::snapshot_path(src.data_dir(), &manifest.snapshot_id).unwrap();
    assert!(file.exists());
    let check = Store::verify_snapshot_file(&file).await.unwrap();
    assert_eq!(check.schema_version, src.schema_version().await.unwrap());
    assert_eq!(
        snapshot::read_manifest(src.data_dir(), &manifest.snapshot_id)
            .unwrap()
            .sha256,
        manifest.sha256
    );

    let (dst, dst_path) = open_store("snap-dst").await;
    assert_eq!(dst.user_count().await.unwrap(), 0);
    dst.restore_snapshot(file.clone()).await.unwrap();
    assert_eq!(dst.user_count().await.unwrap(), 1);
    assert_eq!(dst.ha_epoch().await.unwrap(), 1);
    assert_eq!(dst.ha_repl_seq().await.unwrap(), manifest.seq);

    dst.pool.close().await;
    src.pool.close().await;
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(
        src.data_dir()
            .join(snapshot::manifest_file_name(&manifest.snapshot_id)),
    );
    cleanup(&src_path);
    cleanup(&dst_path);
}

#[tokio::test]
async fn snapshot_round_trips_through_object_storage() {
    use super::backend::s3::mock::MockS3;

    let (mock, endpoint) = MockS3::start().await;
    let src_rt = rt_s3("cp-a", &endpoint, RpoMode::Snapshot);
    let (src, src_path) = open_store("s3-snap-src").await;
    src.create_user("alice", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    src.promote_epoch(1, 1, "cp-a", true).await.unwrap();
    let manifest = snapshot::create(&src_rt, &src).await.unwrap();
    assert!(
        mock.keys()
            .iter()
            .any(|k| k.ends_with("snapshots/latest.json")),
        "latest pointer missing: {:?}",
        mock.keys()
    );
    assert!(
        mock.keys()
            .iter()
            .any(|k| k.ends_with(&format!("{}.db", manifest.snapshot_id))),
        "snapshot database missing: {:?}",
        mock.keys()
    );

    let dst_rt = rt_s3("cp-b", &endpoint, RpoMode::Snapshot);
    let (dst, dst_path) = open_store("s3-snap-dst").await;
    assert_eq!(dst.user_count().await.unwrap(), 0);
    assert!(snapshot::apply_from_s3(&dst_rt, &dst).await.unwrap());
    assert_eq!(dst.user_count().await.unwrap(), 1);
    assert_eq!(dst.ha_epoch().await.unwrap(), 1);
    assert_eq!(dst.ha_repl_seq().await.unwrap(), manifest.seq);
    // Nothing newer: a second poll is a no-op, not a restore.
    assert!(!snapshot::apply_from_s3(&dst_rt, &dst).await.unwrap());

    // The snapshot this replica was built from is kept, which is what lets a
    // condemned node rebuild itself when there is no leader left to ask.
    assert!(
        snapshot::base_snapshot_path(&dst).exists(),
        "the installed snapshot should be kept as the base"
    );
    dst.create_user("bob", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    dst_rt.shared.set_need_full_snapshot();
    assert!(snapshot::restore_local_base(&dst_rt, &dst).await.unwrap());
    assert!(!dst_rt.shared.need_full_snapshot());
    assert_eq!(
        dst.user_count().await.unwrap(),
        1,
        "the write that came after the base is gone, and the replica can be elected"
    );
    assert_eq!(dst.ha_repl_seq().await.unwrap(), manifest.seq);

    dst.pool.close().await;
    src.pool.close().await;
    cleanup(&src_path);
    cleanup(&dst_path);
}

#[cfg(feature = "ha-incremental")]
#[tokio::test]
async fn object_storage_changesets_apply_in_order() {
    use super::backend::s3::mock::MockS3;
    use super::incremental::{apply_s3_pending, Replicator, Session};

    let (_mock, endpoint) = MockS3::start().await;
    let src_rt = rt_s3("cp-a", &endpoint, RpoMode::Async);
    let dst_rt = rt_s3("cp-b", &endpoint, RpoMode::Async);
    let (src, src_path) = open_store("s3-inc-src").await;
    let (dst, dst_path) = open_store("s3-inc-dst").await;
    let repl = Replicator::new(src_rt.clone(), &src.pool).await.unwrap();

    let mut tx = src.write_txn().await.unwrap();
    let mut session = Session::attach(&mut tx, repl.tables()).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, role, created_at) \
         VALUES ('u1', 'alice', 'x', 'admin', '2026-01-01T00:00:00Z')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let bytes = session
        .take_changeset(&mut tx)
        .await
        .unwrap()
        .expect("session recorded the insert");
    drop(session);
    tx.commit().await.unwrap();

    src_rt
        .s3
        .as_ref()
        .unwrap()
        .put_changeset(1, repl.schema_version(), 1, 1, &bytes)
        .await
        .unwrap();
    assert_eq!(apply_s3_pending(&dst_rt, &dst).await.unwrap(), 1);
    assert_eq!(dst.user_count().await.unwrap(), 1);
    assert_eq!(dst.ha_repl_seq().await.unwrap(), 1);
    // Already at the watermark: no duplicate apply and no error.
    assert_eq!(apply_s3_pending(&dst_rt, &dst).await.unwrap(), 0);
    assert_eq!(dst.user_count().await.unwrap(), 1);

    dst.pool.close().await;
    src.pool.close().await;
    cleanup(&src_path);
    cleanup(&dst_path);
}

#[tokio::test]
async fn promoted_standby_replicates_and_fences_old_leader() {
    let leader_rt = rt("cp-a");
    let leader_path = temp_db("fo-leader");
    let leader = Store::open_with_gate(leader_path.to_str().unwrap(), Some(leader_rt.gate.clone()))
        .await
        .unwrap();
    leader.promote_epoch(1, 1, "cp-a", true).await.unwrap();
    leader_rt
        .gate
        .promote_for(1, std::time::Duration::from_secs(30));
    leader
        .create_user("bob", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();

    let standby_rt = rt("cp-b");
    let standby_path = temp_db("fo-standby");
    let standby = Store::open_with_gate(
        standby_path.to_str().unwrap(),
        Some(standby_rt.gate.clone()),
    )
    .await
    .unwrap();
    assert!(standby.write_txn().await.is_err());

    let manifest = snapshot::create(&leader_rt, &leader).await.unwrap();
    let file = snapshot::snapshot_path(leader.data_dir(), &manifest.snapshot_id).unwrap();
    standby.restore_snapshot(file.clone()).await.unwrap();
    assert_eq!(standby.user_count().await.unwrap(), 1);

    // The standby takes over at a higher epoch and can write.
    let seq = standby.promote_epoch(2, 2, "cp-b", true).await.unwrap();
    standby_rt
        .gate
        .promote_for(2, std::time::Duration::from_secs(30));
    assert!(standby.write_txn().await.is_ok());
    assert!(seq >= manifest.seq);
    assert_eq!(standby.ha_epoch().await.unwrap(), 2);
    assert_eq!(standby.ha_term().await.unwrap(), 2);

    // The old leader is fenced: its lease/role no longer permits writes, and
    // the persisted epoch inside its copy cannot match the new epoch.
    leader_rt.gate.demote();
    let err = match leader.write_txn().await {
        Ok(_) => panic!("demoted leader accepted a write"),
        Err(e) => e,
    };
    assert!(
        err.downcast_ref::<super::role::FencedError>().is_some(),
        "expected FencedError, got {err}"
    );

    standby.pool.close().await;
    leader.pool.close().await;
    cleanup(&leader_path);
    cleanup(&standby_path);
}

#[tokio::test]
async fn manual_promotion_flips_gate_and_writes_epoch() {
    let runtime = rt("cp-a");
    let gate = runtime.gate.clone();
    let path = temp_db("promote");
    let store = Store::open_with_gate(path.to_str().unwrap(), Some(gate.clone()))
        .await
        .unwrap();
    assert_eq!(gate.role(), HaRole::Standby);
    assert!(store.write_txn().await.is_err());

    // Simulate the promotion the election loop performs.
    store.promote_epoch(1, 1, "cp-a", true).await.unwrap();
    gate.promote_for(1, std::time::Duration::from_secs(5));
    assert_eq!(gate.role(), HaRole::Leader);
    assert!(store.write_txn().await.is_ok());

    gate.demote();
    assert!(store.write_txn().await.is_err());
    store.pool.close().await;
    cleanup(&path);
}

#[tokio::test]
async fn incremental_promotion_keeps_repl_seq() {
    let (store, path) = open_store("promote-inc").await;
    // Snapshot mode reserves a sequence for the delta the new leader ships.
    assert_eq!(store.promote_epoch(1, 1, "cp-a", true).await.unwrap(), 1);
    // Incremental modes keep it: a bump would open a gap the standbys cannot
    // close without a snapshot.
    assert_eq!(store.promote_epoch(2, 2, "cp-b", false).await.unwrap(), 1);
    assert_eq!(store.ha_repl_seq().await.unwrap(), 1);
    assert_eq!(store.ha_epoch().await.unwrap(), 2);
    store.pool.close().await;
    cleanup(&path);
}

#[cfg(feature = "ha-incremental")]
fn rt_incremental(node_id: &str) -> Arc<HaRuntime> {
    let mut c = HaConfig::single();
    c.mode = HaMode::Ha;
    c.role = NodeRole::Data;
    c.node_id = node_id.to_string();
    c.cluster_id = "prod".into();
    c.cluster_token = TEST_CLUSTER_TOKEN.into();
    c.advertise_url = format!("http://{node_id}:7800");
    c.allow_plaintext = true;
    c.data_replicas = 3;
    c.rpo_mode = super::config::RpoMode::Async;
    c.peers = vec![];
    HaRuntime::new(c).expect("runtime")
}

#[cfg(feature = "ha-incremental")]
#[tokio::test]
async fn changeset_batch_round_trip_is_idempotent() {
    use super::incremental::Session;
    use crate::store::ApplyOutcome;

    let runtime = rt_incremental("cp-a");
    let (src, src_path) = open_store("inc-src").await;
    let repl = super::incremental::Replicator::new(runtime, &src.pool)
        .await
        .unwrap();
    let (dst, dst_path) = open_store("inc-dst").await;

    let mut tx = src.write_txn().await.unwrap();
    let mut session = Session::attach(&mut tx, repl.tables()).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, role, created_at) \
         VALUES ('u1', 'alice', 'x', 'admin', '2026-01-01T00:00:00Z')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let bytes = session
        .take_changeset(&mut tx)
        .await
        .unwrap()
        .expect("session recorded the insert");
    drop(session);
    tx.commit().await.unwrap();

    let schema = repl.schema_version();
    assert_eq!(
        dst.apply_changeset_batch(1, 1, schema, &bytes)
            .await
            .unwrap(),
        ApplyOutcome::Applied { seq: 1 }
    );
    assert_eq!(dst.user_count().await.unwrap(), 1);
    assert_eq!(dst.ha_repl_seq().await.unwrap(), 1);

    // Replay is a no-op, not an error (peers retry after a lost ack).
    assert_eq!(
        dst.apply_changeset_batch(1, 1, schema, &bytes)
            .await
            .unwrap(),
        ApplyOutcome::Duplicate
    );
    // A gap and a schema mismatch are rejected with the local watermark.
    assert!(matches!(
        dst.apply_changeset_batch(3, 3, schema, &bytes)
            .await
            .unwrap(),
        ApplyOutcome::Rejected { applied_seq: 1, .. }
    ));
    assert!(matches!(
        dst.apply_changeset_batch(2, 2, schema + 1, &bytes)
            .await
            .unwrap(),
        ApplyOutcome::Rejected { .. }
    ));

    dst.pool.close().await;
    src.pool.close().await;
    cleanup(&src_path);
    cleanup(&dst_path);
}

#[cfg(feature = "ha-incremental")]
#[tokio::test]
async fn writes_record_changesets_and_bump_seq() {
    let runtime = rt_incremental("cp-a");
    let (mut src, src_path) = open_store("inc-commit").await;
    let repl = super::incremental::Replicator::new(runtime.clone(), &src.pool)
        .await
        .unwrap();
    src.attach_replicator(repl);

    src.create_user("alice", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    assert_eq!(src.ha_repl_seq().await.unwrap(), 1);
    // Async mode advances the applied watermark on enqueue.
    assert_eq!(runtime.shared.applied_seq(), 1);

    src.set_shared_context("grp", "key", "value").await.unwrap();
    assert_eq!(src.ha_repl_seq().await.unwrap(), 2);
    assert_eq!(runtime.shared.applied_seq(), 2);

    // Reads and zero-row writes record nothing and do not burn a sequence.
    assert_eq!(src.user_count().await.unwrap(), 1);
    src.set_node_drained("missing-node", true).await.unwrap();
    assert_eq!(src.ha_repl_seq().await.unwrap(), 2);

    src.pool.close().await;
    cleanup(&src_path);
}

#[cfg(feature = "ha-incremental")]
#[tokio::test]
async fn a_demoted_leader_undoes_its_unreplicated_tail() {
    use super::incremental::Replicator;

    let runtime = rt_async("cp-a");
    let (mut store, path) = open_store("rewind").await;
    // Incremental modes do not bump the sequence at promotion, so the cluster
    // position the leader inherits is whatever it already applied.
    let base = store.promote_epoch(1, 1, "cp-a", false).await.unwrap();
    runtime
        .gate
        .promote_for(1, std::time::Duration::from_secs(60));
    runtime.shared.set_applied_seq(base);
    runtime.shared.advance_last_replicated_seq(base);
    let repl = Replicator::new(runtime.clone(), &store.pool).await.unwrap();
    store.attach_replicator(repl.clone());

    // Neither write can reach a peer, so both exist only on this host.
    store
        .create_user("alice", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    store
        .create_user("bob", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    assert_eq!(store.user_count().await.unwrap(), 2);
    let local = store.ha_repl_seq().await.unwrap();
    assert_eq!(local, base + 2);
    assert_eq!(
        runtime.shared.last_replicated_seq(),
        base,
        "nothing was acknowledged by a data quorum"
    );

    let rewound = repl
        .rewind_to(&store, runtime.shared.last_replicated_seq())
        .await
        .expect("the tail is still in the rewind buffer");
    assert_eq!(rewound, base);
    assert_eq!(store.ha_repl_seq().await.unwrap(), base);
    assert_eq!(
        store.user_count().await.unwrap(),
        0,
        "the writes no data quorum saw are gone, so this copy is a valid prefix again"
    );

    store.pool.close().await;
    cleanup(&path);
}

#[cfg(feature = "ha-incremental")]
#[tokio::test]
async fn a_rewind_to_the_current_position_changes_nothing() {
    use super::incremental::Replicator;

    let runtime = rt_async("cp-a");
    let (mut store, path) = open_store("rewind-noop").await;
    let base = store.promote_epoch(1, 1, "cp-a", false).await.unwrap();
    runtime
        .gate
        .promote_for(1, std::time::Duration::from_secs(60));
    runtime.shared.set_applied_seq(base);
    runtime.shared.advance_last_replicated_seq(base);
    let repl = Replicator::new(runtime.clone(), &store.pool).await.unwrap();
    store.attach_replicator(repl.clone());

    store
        .create_user("alice", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    let local = store.ha_repl_seq().await.unwrap();
    // A quorum ack trims the buffer: the write is durable elsewhere, so it must
    // not be undone even if a demotion asks for the current position.
    runtime.shared.advance_last_replicated_seq(local);

    assert_eq!(repl.rewind_to(&store, local).await.unwrap(), local);
    assert_eq!(store.user_count().await.unwrap(), 1);

    store.pool.close().await;
    cleanup(&path);
}

#[cfg(feature = "ha-incremental")]
#[tokio::test]
async fn a_rewind_below_an_evicted_changeset_is_refused() {
    use super::incremental::Replicator;

    let runtime = rt_async("cp-a");
    let (mut store, path) = open_store("rewind-evicted").await;
    let base = store.promote_epoch(1, 1, "cp-a", false).await.unwrap();
    runtime
        .gate
        .promote_for(1, std::time::Duration::from_secs(60));
    runtime.shared.set_applied_seq(base);
    runtime.shared.advance_last_replicated_seq(base);
    let repl = Replicator::new(runtime.clone(), &store.pool).await.unwrap();
    store.attach_replicator(repl.clone());

    // Push far more changesets than the buffer keeps, so the earliest ones are
    // evicted and a full rewind to the base is no longer possible.
    for seq in 1..=600u64 {
        repl.publish(seq, vec![0u8; 8]).await.unwrap();
    }
    store
        .create_user("alice", "secret123", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap();
    assert!(store.ha_repl_seq().await.unwrap() > base);
    let err = repl.rewind_to(&store, base).await.unwrap_err().to_string();
    assert!(
        err.contains("evicted"),
        "expected an eviction refusal, got: {err}"
    );

    store.pool.close().await;
    cleanup(&path);
}
