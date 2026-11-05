use crate::*;
use std::{collections::BTreeMap, time::Duration};

#[derive(Default)]
pub struct QuotaCore {
    pub snapshot: QuotaSnapshot,
}

impl QuotaCore {
    pub fn inventory(
        &mut self,
        scope: AccountScope,
        generation: u64,
        availability: Availability,
    ) -> Result<(), ErrorCode> {
        if scope
            .native_provider_id
            .as_ref()
            .is_some_and(|id| !valid_text(id, 128))
            || (scope.category == AccountCategory::LlmProvider
                && scope.native_provider_id.is_none())
            || !valid_text(&scope.id, 512)
            || !valid_text(&scope.runtime, 512)
            || (self.snapshot.accounts.len() >= MAX_SCOPES
                && !self.snapshot.accounts.contains_key(&scope.id))
        {
            return Err(ErrorCode::InvalidData);
        }
        let existing = self.snapshot.accounts.get_mut(&scope.id);
        if let Some(account) = existing
            && account.generation == generation
        {
            if account.availability != availability {
                account.availability = availability;
                self.snapshot.revision = self.snapshot.revision.saturating_add(1);
            }
            return Ok(());
        }
        self.snapshot.accounts.insert(
            scope.id.clone(),
            AccountState {
                scope,
                generation,
                availability,
                refresh: RefreshState::Idle,
                windows: BTreeMap::new(),
                metadata: AccountMetadata::default(),
                last_attempt_at: None,
                last_success_at: None,
                source_version: None,
                complete: false,
                sequence: 0,
            },
        );
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        Ok(())
    }

    pub fn apply(
        &mut self,
        observation: Observation,
        now: Timestamp,
    ) -> Result<Vec<WindowKey>, ErrorCode> {
        validate_observation(&observation, now)?;
        let Some(account) = self.snapshot.accounts.get_mut(&observation.scope) else {
            return Ok(Vec::new());
        };
        if account.generation != observation.generation {
            return Ok(Vec::new());
        }
        let added = observation
            .windows
            .iter()
            .filter(|patch| !account.windows.contains_key(&patch.key))
            .count();
        if account.windows.len().saturating_add(added) > MAX_WINDOWS {
            return Err(ErrorCode::InvalidData);
        }
        let mut changed = Vec::new();
        for patch in observation.windows {
            let window = account
                .windows
                .entry(patch.key.clone())
                .or_insert_with(|| QuotaWindow {
                    key: patch.key.clone(),
                    used_fraction: None,
                    remaining: None,
                    limit: None,
                    reset_at: None,
                    admission: None,
                    disabled: None,
                    precision: patch.precision,
                });
            fn merge<T: PartialEq>(
                destination: &mut Option<Measurement<T>>,
                patch: Patch<T>,
                observation: &ObservationStamp,
            ) -> bool {
                let Patch::Set(value) = patch else {
                    return false;
                };
                if let Some(previous) = destination {
                    if matches!(
                        observation.source,
                        SourceKind::ClaudeAcpEvent | SourceKind::ClaudeHeaderProbe
                    ) && observation.at < previous.observed_at
                    {
                        return false;
                    }
                    if observation.source == SourceKind::Persisted
                        && previous.source != SourceKind::Persisted
                    {
                        return false;
                    }
                    if let (Some(old), Some(new)) =
                        (previous.provider_sample_at, observation.sample_at)
                    {
                        if new < old || (new == old && observation.sequence < previous.sequence) {
                            return false;
                        }
                    } else if previous.sequence > observation.sequence
                        || observation
                            .start_sequence
                            .is_some_and(|start| previous.sequence > start)
                    {
                        return false;
                    }
                }
                *destination = Some(Measurement {
                    value,
                    observed_at: observation.at,
                    provider_sample_at: observation.sample_at,
                    sequence: observation.sequence,
                    source: observation.source,
                });
                true
            }
            let stamp = ObservationStamp {
                at: observation.observed_at,
                sample_at: observation.provider_sample_at,
                sequence: observation.sequence,
                start_sequence: observation.start_sequence,
                source: observation.source,
            };
            let mut updated = merge(&mut window.used_fraction, patch.used_fraction, &stamp);
            updated |= merge(&mut window.remaining, patch.remaining, &stamp);
            updated |= merge(&mut window.limit, patch.limit, &stamp);
            updated |= merge(&mut window.reset_at, patch.reset_at, &stamp);
            updated |= merge(&mut window.admission, patch.admission, &stamp);
            updated |= merge(&mut window.disabled, patch.disabled, &stamp);
            if updated {
                window.precision = patch.precision;
                changed.push(patch.key);
            }
        }
        let previous_metadata = account.metadata.clone();
        if observation.sequence >= account.sequence {
            macro_rules! metadata { ($($field:ident),*) => { $(if observation.metadata.$field.is_some() { account.metadata.$field = observation.metadata.$field; })* }; }
            metadata!(
                native_provider,
                grok_response,
                plan,
                ordinary_usage_allowed,
                spend_control_reached,
                has_credits,
                unlimited_credits,
                credit_balance,
                available_reset_credits
            );
        }
        if !changed.is_empty() || previous_metadata != account.metadata {
            account.availability = Availability::Ready;
            account.last_success_at = Some(observation.observed_at);
            account.sequence = account.sequence.max(observation.sequence);
            account.source_version = Some(observation.source_version);
            account.complete |= observation.complete;
            self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        }
        Ok(changed)
    }

    pub fn fresh(
        &self,
        request: &FreshRequest,
        now: Timestamp,
    ) -> Result<FreshResult, RefreshError> {
        let mut matched = false;
        let mut coverage = Vec::new();
        for account in self
            .snapshot
            .accounts
            .values()
            .filter(|account| request.selector.matches(&account.scope))
        {
            matched = true;
            match account.availability {
                Availability::NotInstalled | Availability::NotSetUp => {
                    return Err(RefreshError::NotInstalled);
                }
                Availability::NotLoggedIn => return Err(RefreshError::NotLoggedIn),
                Availability::Unsupported(_) => return Err(RefreshError::Unsupported),
                _ => {}
            }
            let windows = account
                .windows
                .values()
                .filter(|window| request.selector.matches_window(&window.key))
                .collect::<Vec<_>>();
            if windows.is_empty() {
                return Err(RefreshError::Unknown);
            }
            if request.selector.windows.is_empty()
                && request.selector.model.is_none()
                && !account.complete
            {
                return Err(RefreshError::Partial);
            }
            for key in &request.selector.windows {
                if key.bucket.scope == account.scope.id && !account.windows.contains_key(key) {
                    return Err(RefreshError::Partial);
                }
            }
            for window in windows {
                if window.freshness_for(now, request.max_age, &request.required_measurements)
                    != Freshness::Fresh
                {
                    return Err(RefreshError::Partial);
                }
                coverage.push(window.key.clone());
            }
        }
        if !matched
            || request
                .selector
                .scopes
                .iter()
                .any(|scope| !self.snapshot.accounts.contains_key(scope))
        {
            return Err(RefreshError::Unknown);
        }
        Ok(FreshResult {
            revision: self.snapshot.revision,
            as_of: now,
            coverage,
        })
    }

    pub fn expire(&mut self, now: Timestamp) {
        let mut changed = false;
        for account in self.snapshot.accounts.values_mut() {
            let previous_windows = account.windows.clone();
            account.windows.retain(|_, window| window.expire(now));
            changed |= account.windows != previous_windows;
        }
        if changed {
            self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        }
    }

    pub fn snapshot(&self, selector: &Selector, now: Timestamp) -> QuotaSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot
            .accounts
            .retain(|_, account| selector.matches(&account.scope));
        for account in snapshot.accounts.values_mut() {
            account
                .windows
                .retain(|key, window| selector.matches_window(key) && window.expire(now));
        }
        snapshot
    }
}

struct ObservationStamp {
    at: Timestamp,
    sample_at: Option<Timestamp>,
    sequence: u64,
    start_sequence: Option<u64>,
    source: SourceKind,
}

pub fn default_max_age(provider: ProviderId, active: bool) -> Duration {
    Duration::from_secs(match provider {
        ProviderId::Grok => 60,
        ProviderId::Antigravity => 900,
        _ if active => 120,
        _ => 900,
    })
}
