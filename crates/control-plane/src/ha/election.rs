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
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;

use super::config::{NodeRole, RpoMode};
use super::incremental;
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
    // In object-storage mode the candidate must consume the data plane before
    // asking for votes; the old leader may be gone, so peer catch-up is not
    // enough to establish freshness.
    catch_up_s3(state).await?;
    let term = rt.shared.term() + 1;
    rt.shared
        .set_term_and_vote(term, Some(&rt.config.node_id))?;
    if let Some(leader) = rt.shared.leader() {
        if let Err(e) = catch_up(state, &leader).await {
            if transport::status(rt, &leader.url).await.is_ok() {
                anyhow::bail!(
                    "leader {} is reachable but catch-up failed; refusing stale promotion: {e}",
                    leader.id
                );
            }
            tracing::warn!("manual promotion catch-up failed: {e}");
        }
    }
    if rt.shared.need_full_snapshot() {
        anyhow::bail!("local replica must restore the leader snapshot before promotion");
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
    // A demoted leader may have committed writes the rest of the cluster never
    // saw. Its local copy is therefore untrusted: restore the new leader's
    // snapshot before campaigning again, and report a zero replication position
    // so it can still vote for a fresh candidate.
    //
    // Only when it actually admitted writes, though. A node that wins an
    // election and loses a same-term race milliseconds later has served
    // nothing; condemning it to a snapshot made it permanently ineligible to
    // campaign, which in a 3-host cluster where the third host is frozen left
    // no electable candidate at all.
    if rt.gate.admitted_writes() > 0 {
        rt.shared.set_need_full_snapshot();
        rt.shared.set_applied_seq(0);
    } else {
        tracing::info!(
            term = rt.shared.term(),
            "demoted without admitting a write; local state is still trustworthy"
        );
    }
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
    // Object storage may hold changesets/snapshots the old leader never got to
    // push peer-to-peer. Consume it before asking for votes so this candidate
    // does not rewind committed data.
    catch_up_s3(state).await?;
    // A candidate must carry the freshest data it can get before asking for
    // votes; a data voter will refuse a lagging candidate anyway. If the old
    // leader still answers but catch-up failed, do NOT campaign: a reachable
    // leader means no failover is due, and promoting with an older snapshot
    // would roll back acknowledged writes for no reason.
    if let Some(leader) = rt.shared.leader() {
        if let Err(e) = catch_up(state, &leader).await {
            // Probe with a tight budget. `catch_up` already bounds the snapshot
            // apply itself, so it is not wrapped here (cancelling it mid-restore
            // would be worse than waiting); but the extra reachability check must
            // not cost another full transport timeout on top.
            //
            // Only a host that still *reports itself leader* blocks the
            // election. A cached leader that answers with `role=standby` is a
            // stale hint, and treating mere reachability as "no failover is
            // due" deadlocked the cluster: every survivor refused to campaign
            // while pointing at a demoted peer.
            let still_leading = tokio::time::timeout(
                rt.config.election_timeout,
                transport::status(rt, &leader.url),
            )
            .await
            .map(|r| matches!(r, Ok(s) if s.role == "leader"))
            .unwrap_or(false);
            if still_leading {
                anyhow::bail!("leader {} reachable but catch-up failed: {e}", leader.id);
            }
            tracing::debug!(
                "pre-election catch-up failed (no leader at {}): {e}",
                leader.id
            );
        }
    }
    if rt.shared.need_full_snapshot() {
        tracing::info!("not campaigning: local database awaits a full snapshot");
        return Ok(());
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
    let mut max_seq = rt.shared.applied_seq();
    // Ask every voter at once and stop as soon as the quorum is met. Collecting
    // sequentially meant one unreachable peer (a frozen leader keeps accepting
    // connections but never answers, so each vote call burns the full transport
    // timeout) serialised every round *and* re-synchronised the candidates:
    // both survivors blocked on the same dead peer for the same 2s, then both
    // bumped the term, so neither would grant the other's vote. That is how a
    // 3-host cluster took 79 terms and ~9 minutes to elect after one SIGSTOP.
    let mut pending = FuturesUnordered::new();
    let targets: Vec<(bool, String, String)> = rt
        .config
        .peers
        .iter()
        .map(|p| (true, p.id.clone(), p.url.clone()))
        .chain(
            rt.config
                .witnesses
                .iter()
                .map(|w| (false, w.id.clone(), w.url.clone())),
        )
        .collect();
    for (is_data, id, url) in targets {
        let req = req.clone();
        pending.push(async move {
            let out = transport::vote(rt, &url, &req).await;
            (is_data, id, out)
        });
    }
    let mut won = voters.election_won(votes, data_votes, rt.config.rpo_mode);
    while !won {
        let Some((is_data, id, result)) = pending.next().await else {
            break;
        };
        match result {
            Ok(resp) => {
                max_seq = max_seq.max(resp.applied_seq);
                if resp.term > rt.shared.term() {
                    rt.shared.set_term_and_vote(resp.term, None)?;
                    return Ok(());
                }
                if resp.granted {
                    votes += 1;
                    if is_data {
                        data_votes += 1;
                    }
                }
                won = voters.election_won(votes, data_votes, rt.config.rpo_mode);
            }
            Err(e) => tracing::debug!(peer = %id, "vote request failed: {e}"),
        }
    }
    if max_seq > rt.shared.applied_seq() {
        tracing::warn!(
            term,
            local_seq = rt.shared.applied_seq(),
            max_seq,
            "not promoting: a voter is ahead of this replica"
        );
        return Ok(());
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
    if rt.shared.need_full_snapshot() {
        anyhow::bail!("refusing to promote a replica that needs a full snapshot");
    }
    let _guard = rt.shared.promote_lock.lock().await;
    if rt.is_leader() || rt.shared.term() != term {
        return Ok(());
    }
    let db_epoch = state.store.ha_epoch().await?;
    let epoch = term.max(db_epoch + 1);
    // Snapshot mode reserves a fresh sequence for the snapshot the new leader
    // ships; incremental modes continue from the applied position so standbys
    // do not see a gap in the changeset stream.
    let bump_seq = rt.config.rpo_mode == RpoMode::Snapshot;
    let seq = state
        .store
        .promote_epoch(epoch, term, &rt.config.node_id, bump_seq)
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
        // Fan out concurrently and stop collecting at the next tick. Sending
        // sequentially meant a frozen peer (accepts the connection, never
        // answers) delayed every *later* heartbeat by a full transport timeout,
        // so a healthy follower saw no heartbeat for ~2s against a 600ms
        // election timeout: it campaigned, terms churned and leadership
        // flapped — while the edge was answering 503 in the gaps. Ordering must
        // never depend on which peer happens to be dead.
        let mut pending = FuturesUnordered::new();
        let targets: Vec<(bool, String, String)> = rt
            .config
            .peers
            .iter()
            .map(|p| (true, p.id.clone(), p.url.clone()))
            .chain(
                rt.config
                    .witnesses
                    .iter()
                    .map(|w| (false, w.id.clone(), w.url.clone())),
            )
            .collect();
        for (is_data, id, url) in targets {
            let req = req.clone();
            pending.push(async move {
                let out = transport::heartbeat(rt, &url, &req).await;
                (is_data, id, out)
            });
        }
        let deadline = tokio::time::sleep(tick);
        tokio::pin!(deadline);
        let mut stepped_down = false;
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                next = pending.next() => {
                    let Some((is_data, id, result)) = next else { break };
                    match result {
                        Ok(resp) => {
                            if resp.term > rt.shared.term() {
                                rt.shared.set_term_and_vote(resp.term, None).ok();
                                let reason = if is_data {
                                    "peer reported a newer term"
                                } else {
                                    "witness reported a newer term"
                                };
                                demote(&state, reason).await;
                                stepped_down = true;
                                break;
                            }
                            if resp.ok {
                                acks += 1;
                                if is_data {
                                    data_acks += 1;
                                }
                            }
                        }
                        Err(e) => tracing::debug!(peer = %id, "heartbeat failed: {e}"),
                    }
                }
            }
        }
        if stepped_down {
            continue;
        }
        if !rt.is_leader() {
            continue;
        }
        // Incremental replication fell outside its RPO budget: the leader can
        // no longer promise the configured durability, so stop accepting
        // writes and let a caught-up replica take over.
        if rt.shared.take_replication_lost() {
            demote(&state, "incremental replication lost").await;
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
        let requested = rt.shared.take_snapshot_request();
        if requested || next_snapshot.elapsed() >= rt.config.snapshot_interval {
            next_snapshot = Instant::now();
            let _create = rt.shared.snapshot_lock.lock().await;
            match snapshot::create(rt, &state.store).await {
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
        // S3 is a data plane independent of the leader's HTTP surface: a
        // standby must keep applying changesets and the latest snapshot even
        // while the old leader is unreachable.
        if rt.s3.is_some() {
            match incremental::apply_s3_pending(rt, &state.store).await {
                Ok(0) => {}
                Ok(n) => tracing::debug!(batches = n, "applied object-storage changesets"),
                Err(e) => {
                    tracing::warn!("object-storage changeset apply failed: {e}");
                    rt.shared.set_error(Some(format!("changeset: {e}")));
                }
            }
            if let Err(e) = snapshot::apply_from_s3(rt, &state.store).await {
                tracing::warn!("object-storage snapshot apply failed: {e}");
                rt.shared.set_error(Some(format!("apply: {e}")));
            }
        }
        let Some(leader) = rt.shared.leader() else {
            continue;
        };
        match transport::status(rt, &leader.url).await {
            Ok(status) if status.role == "leader" => {
                if status.jwt_fingerprint != rt.jwt_fingerprint() {
                    tracing::warn!(
                        leader = %leader.id,
                        "leader JWT fingerprint mismatch; refusing its snapshot"
                    );
                    rt.shared
                        .set_error(Some("jwt fingerprint mismatch with leader".into()));
                    continue;
                }
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

async fn catch_up_s3(state: &Arc<AppState>) -> Result<()> {
    let rt = &state.ha;
    if rt.s3.is_none() {
        return Ok(());
    }
    incremental::apply_s3_pending(rt, &state.store).await?;
    snapshot::apply_from_s3(rt, &state.store).await?;
    if rt.shared.need_full_snapshot() {
        anyhow::bail!("object storage did not provide a snapshot this replica can trust");
    }
    Ok(())
}

async fn catch_up(state: &Arc<AppState>, leader: &LeaderInfo) -> Result<()> {
    let rt = &state.ha;
    let status = transport::status(rt, &leader.url).await?;
    if status.role != "leader" {
        anyhow::bail!("{} is not a leader (role={})", leader.id, status.role);
    }
    if status.jwt_fingerprint != rt.jwt_fingerprint() {
        anyhow::bail!("{} has a different JWT secret fingerprint", leader.id);
    }
    rt.shared.set_leader(&status.node_id, &leader.url);
    rt.shared.touch_leader();
    // A snapshot download/restore that outlives the apply budget would let a
    // promotion respond while far behind; bound it and stay a standby.
    tokio::time::timeout(
        rt.config.apply_timeout,
        snapshot::apply_from(rt, &state.store, &status),
    )
    .await
    .map_err(|_| anyhow::anyhow!("snapshot apply exceeded {:?}", rt.config.apply_timeout))??;
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
        snapshot_seq: rt.shared.snapshot_seq(),
        snapshot_created_unix: rt.shared.snapshot_created_unix(),
        rpo_mode: rpo_str(rt.config.rpo_mode).to_string(),
        data_replicas: rt.config.data_replicas,
        witnesses: rt.config.witnesses_count,
        jwt_fingerprint: rt.jwt_fingerprint(),
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
