use crate::{ProviderId, Timestamp};
use std::collections::{BTreeMap, BTreeSet};

pub const ACTIVE_SECONDS: i64 = 120;
pub const IDLE_SECONDS: i64 = 900;
pub const MAX_CONCURRENT: usize = 2;
pub const MAX_QUEUE: usize = 64;

#[derive(Clone, Debug)]
pub struct Schedule {
    pub provider: ProviderId,
    pub generation: u64,
    pub last_attempt: Option<Timestamp>,
    pub last_monotonic: Option<u64>,
    pub last_success: Option<Timestamp>,
    pub retry_at: Timestamp,
    pub failures: u32,
    pub in_flight: bool,
    pub suspended: bool,
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
                last_success: None,
                retry_at: previous.as_ref().map_or(0, |state| state.retry_at),
                failures: previous.as_ref().map_or(0, |state| state.failures),
                in_flight: false,
                suspended: false,
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
    pub fn due_at(state: &Schedule, now: Timestamp) -> Timestamp {
        let reset_due = state
            .reset_at
            .is_some_and(|reset| reset <= now && !state.handled_resets.contains(&reset));
        let cadence = if state.forced || reset_due {
            now
        } else {
            state.last_success.map_or(now, |success| {
                success.saturating_add(Self::interval(state, now))
            })
        };
        cadence
            .max(state.last_attempt.map_or(i64::MIN, |attempt| {
                attempt.saturating_add(Self::floor(state.provider) as i64)
            }))
            .max(state.retry_at)
    }
    pub fn next_deadline(&self, now: Timestamp, monotonic: u64) -> Option<Timestamp> {
        self.scopes
            .values()
            .filter(|state| {
                !state.in_flight && !state.suspended && state.provider == ProviderId::Codex
            })
            .map(|state| {
                let floor_wait = state.last_monotonic.map_or(0, |attempt| {
                    Self::floor(state.provider).saturating_sub(monotonic.saturating_sub(attempt))
                });
                {
                    let effective_now = scheduling_now(state, now, monotonic);
                    let due = now.saturating_add(
                        Self::due_at(state, effective_now)
                            .saturating_sub(effective_now)
                            .max(0),
                    );
                    let reset = state
                        .reset_at
                        .filter(|reset| !state.handled_resets.contains(reset) && *reset > now)
                        .unwrap_or(due);
                    due.min(reset).max(now.saturating_add(floor_wait as i64))
                }
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
                && state.provider == ProviderId::Codex
                && Self::due_at(state, scheduling_now(state, now, monotonic))
                    <= scheduling_now(state, now, monotonic)
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
                    && state.provider == ProviderId::Codex
                    && Self::due_at(state, scheduling_now(state, now, monotonic))
                        <= scheduling_now(state, now, monotonic)
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
        for (scope, _, _, _) in candidates.into_iter().take(MAX_QUEUE).take(available) {
            if let Some(state) = self.scopes.get_mut(&scope) {
                state.queued_at = None;
                state.in_flight = true;
                state.forced = false;
                state.last_attempt = Some(now);
                state.last_monotonic = Some(monotonic);
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
    pub fn complete(
        &mut self,
        scope: &str,
        now: Timestamp,
        _monotonic: u64,
        success: bool,
        retry_after: Option<Timestamp>,
    ) {
        if let Some(state) = self.scopes.get_mut(scope) {
            state.in_flight = false;
            if success {
                state.last_success = Some(now);
                state.failures = 0;
            } else {
                state.failures = state.failures.saturating_add(1);
            }
            let delay = if success {
                0
            } else {
                (120i64.saturating_mul(1i64 << state.failures.saturating_sub(1).min(5))).min(3600)
            };
            if !success {
                state.retry_at = state.retry_at.max(now.saturating_add(delay));
            }
            state.retry_at = state.retry_at.max(retry_after.unwrap_or(0));
        }
    }
    pub fn passive(&mut self, scope: &str, now: Timestamp) {
        if let Some(state) = self.scopes.get_mut(scope) {
            state.last_success = Some(now);
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

fn scheduling_now(state: &Schedule, now: Timestamp, monotonic: u64) -> Timestamp {
    match (state.last_attempt, state.last_monotonic) {
        (Some(at), Some(elapsed)) if now < at => {
            at.saturating_add(monotonic.saturating_sub(elapsed).min(i64::MAX as u64) as i64)
        }
        _ => now,
    }
}
