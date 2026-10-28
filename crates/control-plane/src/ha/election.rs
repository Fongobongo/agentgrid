//! Leader election, quorum lease renewal and snapshot apply loops.
//!
//! This is Raft-shaped but deliberately small: terms and votes are persisted
//! outside SQLite, a leader holds a quorum-renewed lease, and a candidate must
//! be at least as fresh as the data replicas that vote for it. SQLite stays
//! single-writer: only the elected leader flips the epoch row and accepts
//! mutations.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::Instant;

use super::config::{NodeRole, RpoMode};
use super::protocol::{HeartbeatRequest, HeartbeatResponse, VoteRequest, VoteResponse};
use super::quorum::VoterSet;
use super::role::{mono_ms, HaRole, HaRuntime};
use super::shared::LeaderInfo;
use super::snapshot;
use super::transport;
use crate::store::Store;
use crate::AppState;

/// Standby poll cadence for leader status / new snapshots.
const APPLY_TICK: Duration = Duration::from_secs(2);

fn voters_of(rt: &HaRuntime) -> VoterSet {
    VoterSet::new(rt.config.data_replicas, rt.config.witnesses_count)
}

fn leader_contact_fresh(rt: &HaRuntime) -> bool {
    // Any valid contact from the current term (heartbeat or vote request)
    // counts: without this, a follower that adopts a candidate's term can
    // fire its already-nearly-expired timer before the winner's first
    // heartbeat, and two data nodes campaign in lockstep forever.
    mono_ms()
        < rt.shared
            .last_contact_ms()
            .saturating_add(rt.config.election_timeout.as_millis() as u64)
}

fn randomized_timeout(rt: &HaRuntime) -> Duration {
    let base = rt.config.election_timeout.as_millis().max(1) as u64;
    let jitter = rand::random::<u64>() % base;
    Duration::from_millis(base + jitter)
}

/// Start HA loops on a data node and adopt the durable position from disk.
pub async fn start(state: Arc<AppState>) -> Result<()> {
    let rt = &state.ha;
    if !rt.is_ha() || rt.config.role != NodeRole::Data {
        return Ok(());
    }
    let vote_path = state.store.data_dir().join("ha-state.json");
    rt.init_data_state(vote_path)?;
    rt.gate.set_epoch(state.store.ha_epoch().await?);
    rt.shared.set_applied_seq(state.store.ha_repl_seq().await?);
    tracing::info!(
        node = %rt.config.node_id,
        term = rt.shared.term(),
        epoch = rt.gate.epoch(),
        applied_seq = rt.shared.applied_seq(),
        "HA data node starting as standby"
    );
    tokio::spawn(election_loop(state.clone()));
    tokio::spawn(leader_loop(state.clone()));
    tokio::spawn(apply_loop(state));
    Ok(())
}

/// Voter side of an election. `applied_seq` is this node's replication
/// position (0 for a witness). A data voter refuses a candidate that is behind
/// it, so an elected leader has at least the data the voters already applied.
pub async fn handle_vote(
    rt: &HaRuntime,
    state: Option<&Arc<AppState>>,
    applied_seq: u64,
    req: VoteRequest,
) -> VoteResponse {
    let deny = |term: u64| VoteResponse {
        term,
        granted: false,
        applied_seq,
    };
    if req.cluster_id != rt.config.cluster_id {
        return deny(rt.shared.term());
    }
    let term = rt.shared.term();
    if req.term < term {
        return deny(term);
    }
    if req.term > term {
        if let Err(e) = rt.shared.set_term_and_vote(req.term, None) {
            tracing::warn!("persist vote failed: {e}");
        }
        if rt.is_leader() {
            if let Some(state) = state {
                demote(state, "vote request with a newer term").await;
            }
        }
    }
    let current = rt.shared.term();
    // A candidate in the current term is alive: give it a full election
    // timeout to win and send heartbeats before this node campaigns too.
    rt.shared.touch_leader();
    if applied_seq > req.applied_seq {
        tracing::info!(
            candidate = %req.candidate_id,
            candidate_seq = req.applied_seq,
            local_seq = applied_seq,
            "refusing vote: candidate is behind this replica"
        );
        return deny(current);
    }
    let grant = match rt.shared.voted_for() {
        None => true,
        Some(v) => v == req.candidate_id,
    };
    if grant {
        if let Err(e) = rt
            .shared
            .set_term_and_vote(current, Some(&req.candidate_id))
        {
            tracing::warn!("persist vote failed: {e}");
            return deny(current);
        }
    }
    VoteResponse {
        term: current,
        granted: grant,
        applied_seq,
    }
}

/// Follower/witness side of a leader heartbeat: adopt the term, record the
/// leader and refresh the contact timestamp.
pub async fn handle_heartbeat(
    rt: &HaRuntime,
    state: Option<&Arc<AppState>>,
    applied_seq: u64,
    req: HeartbeatRequest,
) -> HeartbeatResponse {
    let deny = |term: u64| HeartbeatResponse {
        term,
        ok: false,
        applied_seq,
        role: rt.role().as_str().into(),
    };
    if req.cluster_id != rt.config.cluster_id {
        return deny(rt.shared.term());
    }
    let term = rt.shared.term();
    if req.term < term {
        return deny(term);
    }
    if req.term > term {
        if let Err(e) = rt.shared.set_term_and_vote(req.term, None) {
            tracing::warn!("persist term failed: {e}");
        }
    }
    let current = rt.shared.term();
    // A heartbeat from a leader in the current or a newer term is authoritative
    // even if this node voted for a different candidate: the winner already
    // holds a voter majority, and rejecting it here (as an earlier version
    // did) makes two candidates campaign in lockstep forever.
    if rt.is_leader() && req.leader_id != rt.config.node_id {
        if let Some(state) = state {
            demote(state, "heartbeat from another leader").await;
        }
    }
    if req.epoch > rt.gate.epoch() {
        rt.gate.set_epoch(req.epoch);
    }
    rt.shared.set_leader(&req.leader_id, &req.leader_url);
    rt.shared.touch_leader();
    HeartbeatResponse {
        term: current,
        ok: true,
        applied_seq,
        role: rt.role().as_str().into(),
    }
}

/// Operator-driven promotion (Phase 2 manual failover). Best-effort catch-up
/// from the known leader, then an immediate epoch bump.
pub async fn manual_promote(state: &Arc<AppState>) -> Result<()> {
    let rt = &state.ha;
    if !rt.is_ha() || rt.config.role != NodeRole::Data {
        anyhow::bail!("manual promotion requires a data node in HA mode");
    }
    if rt.is_leader() {
        return Ok(());
    }
    let term = rt.shared.term() + 1;
    rt.shared
        .set_term_and_vote(term, Some(&rt.config.node_id))?;
    if let Some(leader) = rt.shared.leader() {
        if let Err(e) = catch_up(state, &leader).await {
            tracing::warn!("manual promotion catch-up failed: {e}");
        }
    }
    promote(state, term).await
}

/// Step down from leader. Idempotent; closes WS sessions so nodes reconnect.
pub async fn demote(state: &Arc<AppState>, reason: &str) {
    let rt = &state.ha;
    if rt.gate.role() != HaRole::Leader {
        return;
    }
    rt.gate.demote();
    rt.shared.clear_leader();
    state
        .ws_registry
        .close_all(
            agentgrid_common::ws::WS_CLOSE_DEMOTED,
            "control plane demoted; reconnect to the new leader",
        )
        .await;
    tracing::warn!(reason, epoch = rt.gate.epoch(), "HA leader demoted");
}

async fn election_loop(state: Arc<AppState>) {
    let rt = &state.ha;
    loop {
        if rt.is_leader() {
            tokio::time::sleep(rt.config.lease_ttl / 3).await;
            continue;
        }
        if leader_contact_fresh(rt) {
            tokio::time::sleep(rt.config.election_timeout / 3).await;
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(randomized_timeout(rt)) => {}
            _ = rt.shared.manual_promote.notified() => {
                tokio::time::sleep(rt.config.election_timeout).await;
                continue;
            }
        }
        if leader_contact_fresh(rt) {
            continue;
        }
        if let Err(e) = election_round(&state).await {
            tracing::warn!("election round failed: {e}");
            rt.shared.set_error(Some(format!("election: {e}")));
            tokio::time::sleep(rt.config.election_timeout).await;
        }
    }
}

async fn election_round(state: &Arc<AppState>) -> Result<()> {
    let rt = &state.ha;
    // A candidate must carry the freshest data it can get before asking for
    // votes; a data voter will refuse a lagging candidate anyway.
    if let Some(leader) = rt.shared.leader() {
        if let Err(e) = catch_up(state, &leader).await {
            tracing::debug!("pre-election catch-up failed: {e}");
        }
    }
    let term = rt.shared.term() + 1;
    rt.shared
        .set_term_and_vote(term, Some(&rt.config.node_id))
        .context("persist candidacy")?;
    let req = VoteRequest {
        cluster_id: rt.config.cluster_id.clone(),
        term,
        candidate_id: rt.config.node_id.clone(),
        applied_seq: rt.shared.applied_seq(),
    };
    let voters = voters_of(rt);
    let mut votes = 1usize;
    let mut data_votes = 1usize;
    for peer in &rt.config.peers {
        match transport::vote(rt, &peer.url, &req).await {
            Ok(resp) => {
                if resp.term > rt.shared.term() {
                    rt.shared.set_term_and_vote(resp.term, None)?;
                    return Ok(());
                }
                if resp.granted {
                    votes += 1;
                    data_votes += 1;
                }
            }
            Err(e) => tracing::debug!(peer = %peer.id, "vote request failed: {e}"),
        }
    }
    for w in &rt.config.witnesses {
        match transport::vote(rt, &w.url, &req).await {
            Ok(resp) => {
                if resp.term > rt.shared.term() {
                    rt.shared.set_term_and_vote(resp.term, None)?;
                    return Ok(());
                }
                if resp.granted {
                    votes += 1;
                }
            }
            Err(e) => tracing::debug!(peer = %w.id, "witness vote request failed: {e}"),
        }
    }
    if voters.election_won(votes, data_votes, rt.config.rpo_mode) {
        tracing::info!(term, votes, data_votes, "won HA election");
        promote(state, term).await?;
    } else {
        tracing::info!(term, votes, data_votes, "lost HA election");
    }
    Ok(())
}

async fn promote(state: &Arc<AppState>, term: u64) -> Result<()> {
    let rt = &state.ha;
    let _guard = rt.shared.promote_lock.lock().await;
    if rt.is_leader() || rt.shared.term() != term {
        return Ok(());
    }
    let db_epoch = state.store.ha_epoch().await?;
    let epoch = term.max(db_epoch + 1);
    let seq = state
        .store
        .promote_epoch(epoch, term, &rt.config.node_id)
        .await?;
    rt.gate.promote_for(epoch, rt.config.lease_ttl);
    rt.shared
        .set_leader(&rt.config.node_id, &rt.config.advertise_url);
    rt.shared.touch_leader();
    rt.shared.set_applied_seq(seq);
    rt.shared.set_error(None);
    if let Err(e) = state.store.reconcile_on_startup().await {
        tracing::warn!("post-promotion reconcile failed: {e}");
    }
    tracing::info!(epoch, term, seq, node = %rt.config.node_id, "promoted to HA leader");
    Ok(())
}

async fn leader_loop(state: Arc<AppState>) {
    let rt = &state.ha;
    // Heartbeats must arrive well inside the follower election timeout,
    // otherwise a healthy leader is suspected and terms churn forever. Tie the
    // cadence to the (shorter) election timeout, not the lease TTL; the first
    // tick completes immediately so followers see the new leader at once.
    let tick = (rt.config.election_timeout / 3).max(Duration::from_millis(50));
    let mut heartbeat = tokio::time::interval(tick);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_snapshot = Instant::now();
    loop {
        heartbeat.tick().await;
        if !rt.is_leader() {
            continue;
        }
        if !rt.gate.lease_live() {
            demote(&state, "quorum lease expired").await;
            continue;
        }
        let req = HeartbeatRequest {
            cluster_id: rt.config.cluster_id.clone(),
            term: rt.shared.term(),
            epoch: rt.gate.epoch(),
            leader_id: rt.config.node_id.clone(),
            leader_url: rt.config.advertise_url.clone(),
            commit_seq: rt.shared.applied_seq(),
        };
        let voters = voters_of(rt);
        let mut acks = 1usize;
        let mut data_acks = 1usize;
        for peer in &rt.config.peers {
            match transport::heartbeat(rt, &peer.url, &req).await {
                Ok(resp) => {
                    if resp.term > rt.shared.term() {
                        rt.shared.set_term_and_vote(resp.term, None).ok();
                        demote(&state, "peer reported a newer term").await;
                        break;
                    }
                    if resp.ok {
                        acks += 1;
                        data_acks += 1;
                    }
                }
                Err(e) => tracing::debug!(peer = %peer.id, "heartbeat failed: {e}"),
            }
        }
        if rt.is_leader() {
            for w in &rt.config.witnesses {
                match transport::heartbeat(rt, &w.url, &req).await {
                    Ok(resp) if resp.ok => acks += 1,
                    Ok(resp) if resp.term > rt.shared.term() => {
                        rt.shared.set_term_and_vote(resp.term, None).ok();
                        demote(&state, "witness reported a newer term").await;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(witness = %w.id, "heartbeat failed: {e}"),
                }
            }
        }
        if !rt.is_leader() {
            continue;
        }
        let met =
            acks >= voters.majority() && voters.write_quorum_met(data_acks, rt.config.rpo_mode);
        if met {
            rt.gate.renew_for(rt.config.lease_ttl);
            rt.shared.touch_leader();
        } else {
            tracing::warn!(
                acks,
                data_acks,
                need = voters.majority(),
                "HA quorum acknowledgement missing; lease will expire"
            );
        }
        if next_snapshot.elapsed() >= rt.config.snapshot_interval {
            next_snapshot = Instant::now();
            let epoch = rt.gate.epoch();
            let term = rt.shared.term();
            match snapshot::create(&state.store, &rt.shared, &rt.config.cluster_id, epoch, term)
                .await
            {
                Ok(m) => {
                    tracing::info!(snapshot = %m.snapshot_id, seq = m.seq, "wrote HA snapshot")
                }
                Err(e) => {
                    tracing::warn!("HA snapshot failed: {e}");
                    rt.shared.set_error(Some(format!("snapshot: {e}")));
                }
            }
        }
    }
}

async fn apply_loop(state: Arc<AppState>) {
    let rt = &state.ha;
    let mut tick = tokio::time::interval(APPLY_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if rt.is_leader() {
            continue;
        }
        let Some(leader) = rt.shared.leader() else {
            continue;
        };
        match transport::status(rt, &leader.url).await {
            Ok(status) if status.role == "leader" => {
                rt.shared.set_leader(&status.node_id, &leader.url);
                rt.shared.touch_leader();
                if let Err(e) = snapshot::apply_from(rt, &state.store, &status).await {
                    tracing::warn!("snapshot apply failed: {e}");
                    rt.shared.set_error(Some(format!("apply: {e}")));
                }
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(leader = %leader.id, "leader status probe failed: {e}"),
        }
    }
}

async fn catch_up(state: &Arc<AppState>, leader: &LeaderInfo) -> Result<()> {
    let rt = &state.ha;
    let status = transport::status(rt, &leader.url).await?;
    if status.role != "leader" {
        anyhow::bail!("{} is not a leader (role={})", leader.id, status.role);
    }
    rt.shared.set_leader(&status.node_id, &leader.url);
    rt.shared.touch_leader();
    snapshot::apply_from(rt, &state.store, &status).await?;
    Ok(())
}

/// Build the peer-visible status document.
pub async fn status_of(rt: &HaRuntime, store: Option<&Store>) -> Result<super::protocol::HaStatus> {
    let (schema_version, applied_seq) = match store {
        // The persisted replication sequence is authoritative (a leader bumps
        // it for every snapshot); `shared.applied_seq` is the in-memory view.
        Some(s) => (s.schema_version().await?, s.ha_repl_seq().await?),
        None => (0, 0),
    };
    let leader = rt.shared.leader();
    Ok(super::protocol::HaStatus {
        node_id: rt.config.node_id.clone(),
        cluster_id: rt.config.cluster_id.clone(),
        role: rt.role().as_str().to_string(),
        term: rt.shared.term(),
        epoch: rt.gate.epoch(),
        leader_id: leader.as_ref().map(|l| l.id.clone()),
        leader_url: leader.as_ref().map(|l| l.url.clone()),
        applied_seq,
        schema_version,
        snapshot_id: rt.shared.snapshot_id(),
        snapshot_created_unix: rt.shared.snapshot_created_unix(),
        rpo_mode: rpo_str(rt.config.rpo_mode).to_string(),
        data_replicas: rt.config.data_replicas,
        witnesses: rt.config.witnesses_count,
        error: rt.shared.last_error(),
    })
}

fn rpo_str(mode: RpoMode) -> &'static str {
    match mode {
        RpoMode::Snapshot => "snapshot",
        RpoMode::Async => "async",
        RpoMode::Sync => "sync",
    }
}
