use crate::{ProviderId, Timestamp};
use std::collections::{BTreeMap, BTreeSet};

pub const ACTIVE_SECONDS: i64 = 120;
pub const IDLE_SECONDS: i64 = 900;
pub const MAX_BACKOFF_SECONDS: u64 = 3600;
pub const MAX_CONCURRENT: usize = 2;
pub const MAX_QUEUE: usize = 64;

#[derive(Clone, Debug)]
pub struct Schedule {
    pub provider: ProviderId,
    pub generation: u64,
    pub last_attempt: Option<Timestamp>,
    pub last_monotonic: Option<u64>,
    pub restored_attempt_not_before: Option<u64>,
    pub restored_attempt_at: Option<Timestamp>,
    pub last_success: Option<Timestamp>,
    pub last_success_monotonic: Option<u64>,
    pub retry_monotonic: Option<u64>,
    pub retry_at: Timestamp,
    pub failures: u32,
    pub in_flight: bool,
    pub suspended: bool,
    pub restoring: bool,
    pub demand_until: Timestamp,
    pub forced: bool,
    pub reset_at: Option<Timestamp>,
    pub handled_resets: BTreeSet<Timestamp>,
    pub order: u64,
    pub queued_at: Option<Timestamp>,
}

#[derive(Default)]
pub struct Scheduler {
    pub scopes: BTreeMap<String, Schedule>,
    order: u64,
}

impl Scheduler {
    pub fn track(&mut self, scope: String, provider: ProviderId, generation: u64) {
        if self
            .scopes
            .get(&scope)
            .is_some_and(|state| state.generation == generation)
        {
            return;
        }
        let previous = self.scopes.remove(&scope);
        self.order = self.order.saturating_add(1);
        self.scopes.insert(
            scope,
            Schedule {
                provider,
                generation,
                last_attempt: previous.as_ref().and_then(|state| state.last_attempt),
                last_monotonic: previous.as_ref().and_then(|state| state.last_monotonic),
                restored_attempt_not_before: previous
                    .as_ref()
                    .and_then(|state| state.restored_attempt_not_before),
                restored_attempt_at: previous
                    .as_ref()
                    .and_then(|state| state.restored_attempt_at),
                last_success: None,
                last_success_monotonic: None,
                retry_monotonic: None,
                retry_at: 0,
                failures: 0,
                in_flight: false,
                suspended: false,
                restoring: false,
                demand_until: 0,
                forced: false,
                reset_at: None,
                handled_resets: BTreeSet::new(),
                order: self.order,
                queued_at: None,
            },
        );
    }
    pub fn demand(&mut self, scope: &str, until: Timestamp) {
        if let Some(state) = self.scopes.get_mut(scope) {
            state.demand_until = state.demand_until.max(until);
        }
    }
    pub fn reset_due(&mut self, scope: &str, reset: Timestamp) {
        if let Some(state) = self.scopes.get_mut(scope) {
            state.reset_at = Some(reset);
        }
    }
    pub fn force(&mut self, scope: &str) {
        if let Some(state) = self.scopes.get_mut(scope)
            && !state.in_flight
        {
            state.forced = true;
        }
    }
    pub fn interval(state: &Schedule, now: Timestamp) -> i64 {
        if state.provider == ProviderId::Antigravity || state.demand_until <= now {
            IDLE_SECONDS
        } else {
            ACTIVE_SECONDS
        }
    }
    pub fn floor(provider: ProviderId) -> u64 {
        if provider == ProviderId::Antigravity {
            900
        } else {
            120
        }
    }
    pub fn next_for(state: &Schedule, now: Timestamp, monotonic: u64) -> Timestamp {
        let until = |at: Option<Timestamp>, anchor: Option<u64>, interval: u64| -> u64 {
            match anchor {
                Some(anchor) => anchor.saturating_add(interval).saturating_sub(monotonic),
                None => at.map_or(0, |at| {
                    at.saturating_add(interval as i64)
                        .saturating_sub(now)
                        .clamp(0, interval as i64) as u64
                }),
            }
        };
        let reset_due = state
            .reset_at
            .is_some_and(|reset| reset <= now && !state.handled_resets.contains(&reset));
        let cadence = if state.forced || reset_due {
            0
        } else {
            until(
                state.last_success,
                state.last_success_monotonic,
                Self::interval(state, now) as u64,
            )
        };
        let floor = state.restored_attempt_not_before.map_or_else(
            || {
                until(
                    state.last_attempt,
                    state.last_monotonic,
                    Self::floor(state.provider),
                )
            },
            |deadline| deadline.saturating_sub(monotonic),
        );
        let backoff = state.retry_monotonic.map_or_else(
            || {
                state
                    .retry_at
                    .saturating_sub(now)
                    .clamp(0, MAX_BACKOFF_SECONDS as i64) as u64
            },
            |retry| retry.saturating_sub(monotonic),
        );
        now.saturating_add(cadence.max(floor).max(backoff).min(i64::MAX as u64) as i64)
    }

    pub fn restore_timing(
        &mut self,
        scope: &str,
        attempt: Option<Timestamp>,
        retry_at: Timestamp,
        failures: u32,
        now: Timestamp,
        monotonic: u64,
    ) {
        let Some(state) = self.scopes.get_mut(scope) else {
            return;
        };
        if let Some(attempt) = attempt {
            let remaining = attempt
                .saturating_add(Self::floor(state.provider) as i64)
                .saturating_sub(now)
                .clamp(0, Self::floor(state.provider) as i64) as u64;
            if state.last_monotonic.is_none()
                && state
                    .restored_attempt_at
                    .is_none_or(|previous| attempt > previous)
            {
                // Restarted UTC can be behind the saved clock; preserve the elapsed floor on a monotonic deadline, including an already-expired guard.
                state.last_attempt = Some(attempt.min(now));
                state.restored_attempt_at = Some(attempt);
                state.restored_attempt_not_before = Some(
                    state
                        .restored_attempt_not_before
                        .unwrap_or(0)
                        .max(monotonic.saturating_add(remaining)),
                );
            }
        }
        let remaining = retry_at
            .saturating_sub(now)
            .clamp(0, MAX_BACKOFF_SECONDS as i64) as u64;
        if remaining > 0 && state.retry_monotonic.is_none() {
            state.retry_at = state.retry_at.max(now.saturating_add(remaining as i64));
            state.retry_monotonic = Some(monotonic.saturating_add(remaining));
        }
        state.failures = state.failures.max(failures);
    }

    pub fn next_deadline(&self, now: Timestamp, monotonic: u64) -> Option<Timestamp> {
        self.scopes
            .values()
            .filter(|state| {
                !state.in_flight
                    && !state.suspended
                    && !state.restoring
                    && (matches!(state.provider, ProviderId::Codex | ProviderId::Antigravity)
                        || (state.provider == ProviderId::Claude && state.demand_until > now))
            })
            .map(|state| {
                let due = Self::next_for(state, now, monotonic);
                let reset = state
                    .reset_at
                    .filter(|reset| !state.handled_resets.contains(reset) && *reset > now)
                    .unwrap_or(due);
                due.min(reset)
            })
            .min()
    }
    pub fn start_due(&mut self, now: Timestamp, monotonic: u64) -> Vec<(String, u64)> {
        for state in self.scopes.values_mut() {
            if state
                .reset_at
                .is_some_and(|reset| reset <= now && !state.handled_resets.contains(&reset))
            {
                state.forced = true;
            }
        }
        for state in self.scopes.values_mut() {
            if !state.in_flight
                && !state.suspended
                && !state.restoring
                && (matches!(state.provider, ProviderId::Codex | ProviderId::Antigravity)
                    || (state.provider == ProviderId::Claude && state.demand_until > now))
                && Self::next_for(state, now, monotonic) <= now
            {
                state.queued_at.get_or_insert(now);
            }
        }
        let available = MAX_CONCURRENT
            .saturating_sub(self.scopes.values().filter(|state| state.in_flight).count());
        let mut candidates = self
            .scopes
            .iter()
            .filter(|(_, state)| {
                !state.in_flight
                    && !state.suspended
                    && !state.restoring
                    && (matches!(state.provider, ProviderId::Codex | ProviderId::Antigravity)
                        || (state.provider == ProviderId::Claude && state.demand_until > now))
                    && Self::next_for(state, now, monotonic) <= now
                    && state.last_monotonic.is_none_or(|attempt| {
                        monotonic.saturating_sub(attempt) >= Self::floor(state.provider)
                    })
            })
            .map(|(scope, state)| {
                (
                    scope.clone(),
                    state
                        .queued_at
                        .is_some_and(|queued| now.saturating_sub(queued) >= 900),
                    state.demand_until > now,
                    state.order,
                )
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, aged, active, order)| (!aged, !active, *order));
        let mut started = Vec::new();
        let mut billable = self
            .scopes
            .values()
            .any(|state| state.in_flight && state.provider == ProviderId::Claude);
        for (scope, _, _, _) in candidates.into_iter().take(MAX_QUEUE).take(available) {
            if self
                .scopes
                .get(&scope)
                .is_some_and(|state| state.provider == ProviderId::Claude)
            {
                if billable {
                    continue;
                }
                billable = true;
            }
            if let Some(state) = self.scopes.get_mut(&scope) {
                state.queued_at = None;
                state.in_flight = true;
                state.forced = false;
                state.last_attempt = Some(now);
                state.last_monotonic = Some(monotonic);
                state.restored_attempt_not_before = None;
                state.restored_attempt_at = None;
                if state.reset_at.is_some_and(|reset| reset <= now) {
                    if let Some(reset) = state.reset_at {
                        state.handled_resets.insert(reset);
                    }
                }
                self.order = self.order.saturating_add(1);
                state.order = self.order;
                started.push((scope, state.generation));
            }
        }
        started
    }
    pub fn defer(&mut self, scope: &str, now: Timestamp, monotonic: u64, retry_at: Timestamp) {
        if let Some(state) = self.scopes.get_mut(scope) {
            state.in_flight = false;
            state.retry_at = retry_at.max(now);
            state.retry_monotonic =
                Some(monotonic.saturating_add(state.retry_at.saturating_sub(now) as u64));
        }
    }
    pub fn complete(
        &mut self,
        scope: &str,
        now: Timestamp,
        monotonic: u64,
        success: bool,
        retry_after: Option<Timestamp>,
    ) {
        if let Some(state) = self.scopes.get_mut(scope) {
            state.in_flight = false;
            if success {
                state.last_success = Some(now);
                state.last_success_monotonic = Some(monotonic);
                state.failures = 0;
            } else {
                state.failures = state.failures.saturating_add(1);
            }
            let delay = if success {
                0
            } else {
                (120i64.saturating_mul(1i64 << state.failures.saturating_sub(1).min(5))).min(3600)
            };
            state.retry_at = now.saturating_add(delay).max(retry_after.unwrap_or(now));
            state.retry_monotonic =
                Some(monotonic.saturating_add(state.retry_at.saturating_sub(now).max(0) as u64));
        }
    }
    pub fn passive(&mut self, scope: &str, now: Timestamp, monotonic: u64) {
        if let Some(state) = self.scopes.get_mut(scope) {
            state.last_success = Some(now);
            state.last_success_monotonic = Some(monotonic);
            state.queued_at = None;
            state.forced = false;
            state.failures = 0;
        }
    }
}

pub fn retry_after(value: &str, now: Timestamp) -> Option<Timestamp> {
    if let Ok(seconds) = value.parse::<u64>() {
        return now.checked_add(i64::try_from(seconds).ok()?);
    }
    chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.timestamp().max(now))
}
