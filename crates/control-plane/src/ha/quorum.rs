//! Pure quorum arithmetic for election and write acknowledgement.

use super::config::RpoMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoterSet {
    pub data: usize,
    pub witnesses: usize,
}

impl VoterSet {
    pub fn new(data: usize, witnesses: usize) -> Self {
        Self { data, witnesses }
    }

    pub fn total(&self) -> usize {
        self.data + self.witnesses
    }

    pub fn majority(&self) -> usize {
        self.total() / 2 + 1
    }

    pub fn data_majority(&self) -> usize {
        self.data / 2 + 1
    }

    /// A candidate wins when it has a voter majority and, in `sync` mode, a
    /// majority of *data* voters. A witness vote must never stand in for a
    /// fresh data replica: with 2 data + 1 witness a stale data node could
    /// otherwise win with only the witness, breaking RPO=0.
    pub fn election_won(&self, votes: usize, data_votes: usize, rpo: RpoMode) -> bool {
        votes >= self.majority() && (rpo != RpoMode::Sync || data_votes >= self.data_majority())
    }

    /// Sync writes must be durable on a data majority before the client sees
    /// success. Async/snapshot modes never block on this.
    pub fn write_quorum_met(&self, data_acks: usize, rpo: RpoMode) -> bool {
        rpo != RpoMode::Sync || data_acks >= self.data_majority()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_data() {
        let v = VoterSet::new(3, 0);
        assert_eq!(v.majority(), 2);
        assert_eq!(v.data_majority(), 2);
        assert!(v.election_won(2, 2, RpoMode::Sync));
        assert!(!v.election_won(2, 1, RpoMode::Sync));
        assert!(v.election_won(2, 1, RpoMode::Async));
    }

    #[test]
    fn two_data_plus_witness() {
        let v = VoterSet::new(2, 1);
        assert_eq!(v.majority(), 2);
        assert_eq!(v.data_majority(), 2);
        assert!(!v.election_won(2, 1, RpoMode::Sync));
        assert!(v.election_won(2, 2, RpoMode::Sync));
        assert!(v.election_won(2, 1, RpoMode::Snapshot));
    }

    #[test]
    fn single_node() {
        let v = VoterSet::new(1, 0);
        assert_eq!(v.majority(), 1);
        assert!(v.election_won(1, 1, RpoMode::Sync));
    }

    #[test]
    fn write_quorum_is_sync_only() {
        let v = VoterSet::new(3, 0);
        assert!(v.write_quorum_met(0, RpoMode::Async));
        assert!(!v.write_quorum_met(1, RpoMode::Sync));
        assert!(v.write_quorum_met(2, RpoMode::Sync));
    }
}
