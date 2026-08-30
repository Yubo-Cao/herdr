use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::schema::{
    CollaborationPaneClaim, CollaborationParticipant, CollaborationSnapshot,
    CollaborationUpdateParams,
};

pub const COLLABORATION_LEASE_TTL_MS: u64 = 45_000;
pub const COLLABORATION_TYPING_TTL_MS: u64 = 3_000;
pub const COLLABORATION_MAX_CONTROL_PROTECTION_MS: u64 = 60_000;

#[derive(Debug, Default)]
pub struct CollaborationState {
    participants: HashMap<String, CollaborationParticipant>,
    pane_claims: HashMap<String, CollaborationPaneClaim>,
}

impl CollaborationState {
    pub fn from_snapshot(snapshot: CollaborationSnapshot, now: u64) -> Self {
        let mut state = Self {
            participants: snapshot
                .participants
                .into_iter()
                .map(|participant| (participant.participant_id.clone(), participant))
                .collect(),
            pane_claims: snapshot
                .pane_claims
                .into_iter()
                .map(|claim| (claim.pane_id.clone(), claim))
                .collect(),
        };
        state.prune(now);
        state
    }

    pub fn update_participant(
        &mut self,
        params: CollaborationUpdateParams,
        now: u64,
    ) -> CollaborationParticipant {
        self.prune(now);
        let participant = CollaborationParticipant {
            participant_id: params.participant_id,
            display_name: params.display_name,
            color: params.color,
            role: params.role,
            activity: params.activity,
            surface: params.surface,
            workspace_id: params.workspace_id,
            tab_id: params.tab_id,
            pane_id: params.pane_id,
            typing: params.typing,
            typing_expires_at_unix_ms: params
                .typing
                .then_some(now.saturating_add(COLLABORATION_TYPING_TTL_MS)),
            updated_at_unix_ms: now,
            expires_at_unix_ms: now.saturating_add(COLLABORATION_LEASE_TTL_MS),
        };
        if participant.role == crate::api::schema::CollaborationRole::Viewer {
            self.pane_claims
                .retain(|_, claim| claim.participant_id != participant.participant_id);
        } else {
            for claim in self.pane_claims.values_mut() {
                if claim.participant_id == participant.participant_id {
                    claim.updated_at_unix_ms = now;
                    claim.expires_at_unix_ms = participant.expires_at_unix_ms;
                }
            }
        }
        self.participants
            .insert(participant.participant_id.clone(), participant.clone());
        participant
    }

    pub fn leave(&mut self, participant_id: &str) -> bool {
        let removed = self.participants.remove(participant_id).is_some();
        self.pane_claims
            .retain(|_, claim| claim.participant_id != participant_id);
        removed
    }

    pub fn claim_pane(
        &mut self,
        participant_id: &str,
        pane_id: &str,
        takeover: bool,
        protect_ms: Option<u64>,
        now: u64,
    ) -> Result<(bool, CollaborationPaneClaim), &'static str> {
        self.prune(now);
        let participant = self
            .participants
            .get(participant_id)
            .ok_or("participant_not_registered")?;
        if participant.role == crate::api::schema::CollaborationRole::Viewer {
            return Err("participant_is_viewer");
        }
        if let Some(existing) = self.pane_claims.get(pane_id) {
            if existing.participant_id != participant_id {
                let protected = existing
                    .protected_until_unix_ms
                    .is_some_and(|until| until > now);
                if protected || !takeover {
                    return Ok((false, existing.clone()));
                }
            }
        }
        let existing = self
            .pane_claims
            .get(pane_id)
            .filter(|claim| claim.participant_id == participant_id);
        let acquired_at = existing.map_or(now, |claim| claim.acquired_at_unix_ms);
        let requested_protection = protect_ms
            .unwrap_or_default()
            .min(COLLABORATION_MAX_CONTROL_PROTECTION_MS);
        let protected_until_unix_ms = if requested_protection > 0 {
            Some(
                existing
                    .and_then(|claim| claim.protected_until_unix_ms)
                    .unwrap_or_default()
                    .max(now.saturating_add(requested_protection)),
            )
        } else {
            existing
                .and_then(|claim| claim.protected_until_unix_ms)
                .filter(|until| *until > now)
        };
        let claim = CollaborationPaneClaim {
            pane_id: pane_id.to_string(),
            participant_id: participant_id.to_string(),
            acquired_at_unix_ms: acquired_at,
            updated_at_unix_ms: now,
            expires_at_unix_ms: now.saturating_add(COLLABORATION_LEASE_TTL_MS),
            protected_until_unix_ms,
        };
        self.pane_claims.insert(pane_id.to_string(), claim.clone());
        Ok((true, claim))
    }

    pub fn release_pane(&mut self, participant_id: &str, pane_id: &str) -> bool {
        if self
            .pane_claims
            .get(pane_id)
            .is_some_and(|claim| claim.participant_id == participant_id)
        {
            self.pane_claims.remove(pane_id);
            true
        } else {
            false
        }
    }

    pub fn participant_needs_refresh(&self, participant_id: &str, now: u64) -> bool {
        self.participants
            .get(participant_id)
            .is_none_or(|participant| {
                participant.expires_at_unix_ms <= now.saturating_add(COLLABORATION_LEASE_TTL_MS / 2)
            })
    }

    pub fn pane_participant_count(&self, pane_id: &str) -> usize {
        self.participants
            .values()
            .filter(|participant| participant.pane_id.as_deref() == Some(pane_id))
            .count()
    }

    pub fn pane_typing_participants(&self, pane_id: &str) -> Vec<&CollaborationParticipant> {
        self.participants
            .values()
            .filter(|participant| {
                participant.pane_id.as_deref() == Some(pane_id) && participant.typing
            })
            .collect()
    }

    pub fn prune_expired(&mut self, now: u64) -> bool {
        let before = (self.participants.len(), self.pane_claims.len());
        self.prune(now);
        before != (self.participants.len(), self.pane_claims.len())
    }

    pub fn snapshot(&mut self, now: u64) -> CollaborationSnapshot {
        self.prune(now);
        let mut participants = self.participants.values().cloned().collect::<Vec<_>>();
        participants.sort_by(|a, b| a.participant_id.cmp(&b.participant_id));
        let mut pane_claims = self.pane_claims.values().cloned().collect::<Vec<_>>();
        pane_claims.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        CollaborationSnapshot {
            participants,
            pane_claims,
            lease_ttl_ms: COLLABORATION_LEASE_TTL_MS,
        }
    }

    fn prune(&mut self, now: u64) {
        for participant in self.participants.values_mut() {
            if participant
                .typing_expires_at_unix_ms
                .is_some_and(|expires_at| expires_at <= now)
            {
                participant.typing = false;
                participant.typing_expires_at_unix_ms = None;
            }
        }
        self.participants
            .retain(|_, participant| participant.expires_at_unix_ms > now);
        self.pane_claims.retain(|_, claim| {
            claim.expires_at_unix_ms > now && self.participants.contains_key(&claim.participant_id)
        });
    }
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{CollaborationActivity, CollaborationRole};

    fn participant(id: &str, role: CollaborationRole) -> CollaborationUpdateParams {
        CollaborationUpdateParams {
            participant_id: id.into(),
            display_name: id.into(),
            color: "#0969da".into(),
            role,
            activity: CollaborationActivity::Active,
            surface: "web".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            typing: false,
        }
    }

    #[test]
    fn pane_claims_are_exclusive_but_can_be_taken_over() {
        let mut state = CollaborationState::default();
        state.update_participant(participant("a", CollaborationRole::Editor), 10);
        state.update_participant(participant("b", CollaborationRole::Editor), 10);
        assert!(state.claim_pane("a", "pane", false, None, 20).unwrap().0);
        let denied = state.claim_pane("b", "pane", false, None, 30).unwrap();
        assert!(!denied.0);
        assert_eq!(denied.1.participant_id, "a");
        assert!(state.claim_pane("b", "pane", true, None, 40).unwrap().0);
    }

    #[test]
    fn protected_claims_reject_takeover_until_the_protection_expires() {
        let mut state = CollaborationState::default();
        state.update_participant(participant("a", CollaborationRole::Editor), 10);
        state.update_participant(participant("b", CollaborationRole::Editor), 10);
        let claimed = state
            .claim_pane("a", "pane", true, Some(15_000), 20)
            .unwrap();
        assert_eq!(claimed.1.protected_until_unix_ms, Some(15_020));

        let denied = state
            .claim_pane("b", "pane", true, Some(15_000), 15_019)
            .unwrap();
        assert!(!denied.0);
        assert_eq!(denied.1.participant_id, "a");

        let granted = state
            .claim_pane("b", "pane", true, Some(15_000), 15_020)
            .unwrap();
        assert!(granted.0);
        assert_eq!(granted.1.participant_id, "b");
    }

    #[test]
    fn expired_participants_release_their_claims() {
        let mut state = CollaborationState::default();
        state.update_participant(participant("a", CollaborationRole::Editor), 10);
        state.claim_pane("a", "pane", false, None, 20).unwrap();
        let snapshot = state.snapshot(10 + COLLABORATION_LEASE_TTL_MS + 1);
        assert!(snapshot.participants.is_empty());
        assert!(snapshot.pane_claims.is_empty());
    }

    #[test]
    fn viewer_downgrade_releases_existing_claims() {
        let mut state = CollaborationState::default();
        state.update_participant(participant("a", CollaborationRole::Editor), 10);
        state.update_participant(participant("b", CollaborationRole::Editor), 10);
        state.claim_pane("a", "pane", false, None, 20).unwrap();

        state.update_participant(participant("a", CollaborationRole::Viewer), 30);

        assert!(state.snapshot(30).pane_claims.is_empty());
        assert!(state.claim_pane("b", "pane", false, None, 40).unwrap().0);
    }

    #[test]
    fn restoring_a_snapshot_keeps_only_live_participants_and_claims() {
        let mut source = CollaborationState::default();
        source.update_participant(participant("live", CollaborationRole::Editor), 10);
        source.claim_pane("live", "pane", false, None, 20).unwrap();

        let mut restored = CollaborationState::from_snapshot(source.snapshot(30), 40);
        let snapshot = restored.snapshot(40);
        assert_eq!(snapshot.participants[0].participant_id, "live");
        assert_eq!(snapshot.pane_claims[0].participant_id, "live");

        let expired =
            CollaborationState::from_snapshot(snapshot, 10 + COLLABORATION_LEASE_TTL_MS + 1);
        assert!(expired.participants.is_empty());
        assert!(expired.pane_claims.is_empty());
    }

    #[test]
    fn typing_presence_expires_before_the_participant_lease() {
        let mut state = CollaborationState::default();
        let mut params = participant("alice", CollaborationRole::Editor);
        params.pane_id = Some("pane".into());
        params.typing = true;
        state.update_participant(params, 10);

        let active = state.snapshot(10 + COLLABORATION_TYPING_TTL_MS - 1);
        assert!(active.participants[0].typing);

        let idle = state.snapshot(10 + COLLABORATION_TYPING_TTL_MS);
        assert!(!idle.participants[0].typing);
        assert_eq!(idle.participants[0].typing_expires_at_unix_ms, None);
        assert_eq!(idle.participants.len(), 1);
    }
}
