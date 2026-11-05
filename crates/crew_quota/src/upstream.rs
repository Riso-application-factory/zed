mod antigravity;
mod claude;
mod grok;
pub use grok::install_http_observer;
mod panel;
mod providers;

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use db::kvp::KeyValueStore;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    future::Shared,
};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task};
use project::{
    AgentId, AgentServerStore, AgentServersUpdated, LocalProjectFlags, Project,
    agent_registry_store::{AgentRegistryStore, RegistryAgent},
    agent_server_store::{AgentInstallStatus, AllAgentServersSettings, CustomAgentServerSettings},
};
use settings::SettingsStore;
use workspace::AppState;

use crate::*;

pub trait Clock {
    fn now(&self) -> Timestamp;
    fn monotonic(&self) -> u64;
}
struct SystemClock(Instant);
impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        chrono::Utc::now().timestamp()
    }
    fn monotonic(&self) -> u64 {
        self.0.elapsed().as_secs()
    }
}

pub trait QuotaStore {
    fn load(&self, key: String, cx: &App) -> Task<Result<Option<String>, ErrorCode>>;
    fn save(&self, key: String, value: String, cx: &App) -> Task<Result<(), ErrorCode>>;
}
struct KvpStore;
impl QuotaStore for KvpStore {
    fn load(&self, key: String, cx: &App) -> Task<Result<Option<String>, ErrorCode>> {
        let store = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            store
                .scoped("crew_quota")
                .read(&key)
                .map_err(|_| ErrorCode::Persistence)
        })
    }
    fn save(&self, key: String, value: String, cx: &App) -> Task<Result<(), ErrorCode>> {
        let store = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            store
                .scoped("crew_quota")
                .write(key, value)
                .await
                .map_err(|_| ErrorCode::Persistence)
        })
    }
}

pub trait QuotaSource {
    fn refresh(
        &self,
        scope: String,
        generation: u64,
        start_sequence: u64,
        cx: &mut App,
    ) -> Task<Result<Observation, RefreshError>>;
}

pub struct Ports {
    pub source: Rc<dyn QuotaSource>,
    pub store: Rc<dyn QuotaStore>,
    pub clock: Rc<dyn Clock>,
}
type RefreshTask = Shared<Task<Result<(), RefreshError>>>;

struct GlobalQuota(Entity<QuotaService>);
impl Global for GlobalQuota {}

pub struct DemandLease {
    until: Rc<Cell<Timestamp>>,
    active: Rc<Cell<bool>>,
    wake: mpsc::UnboundedSender<()>,
}
impl DemandLease {
    pub fn renew(&self, now: Timestamp, ttl: Duration) {
        self.until
            .set(now.saturating_add(ttl.as_secs().min(300) as i64));
        if self.wake.unbounded_send(()).is_err() {
            log::debug!("crew_quota: demand_service_closed");
        }
    }
}
impl Drop for DemandLease {
    fn drop(&mut self) {
        self.active.set(false);
        if self.wake.unbounded_send(()).is_err() {
            log::debug!("crew_quota: demand_service_closed");
        }
    }
}
struct Demand {
    allow_inference: bool,
    selector: Selector,
    until: Rc<Cell<Timestamp>>,
    active: Rc<Cell<bool>>,
}

pub struct QuotaService {
    core: QuotaCore,
    scheduler: Scheduler,
    ports: Ports,
    tasks: BTreeMap<String, RefreshTask>,
    cancellations: BTreeMap<String, futures::future::AbortHandle>,
    demands: Vec<Demand>,
    wake: mpsc::UnboundedSender<()>,
    writes: mpsc::UnboundedSender<(String, String)>,
    persistence_blocked: BTreeSet<String>,
    hydrated: bool,
    ready: Shared<Task<Result<(), ErrorCode>>>,
    ready_sender: Option<oneshot::Sender<Result<(), ErrorCode>>>,
    _scheduler: Task<()>,
    _writer: Task<()>,
    pending_persistence: BTreeMap<String, Task<()>>,
    _inventory: Option<Task<()>>,
    inventory_refresh: Option<Shared<Task<()>>>,
    codex_installation: Option<&'static str>,
    grok_task: Option<Task<()>>,
    grok_identity: Option<(String, bool)>,
    claude: claude::ClaudeState,
    antigravity_inventory: Option<Task<()>>,
    antigravity_identity: Option<(Vec<String>, Availability)>,
    _auth_watcher: Option<Task<()>>,
    auth_path: Option<std::path::PathBuf>,
    _project: Option<Entity<Project>>,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<QuotaEvent> for QuotaService {}

impl QuotaService {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalQuota>()
            .map(|global| global.0.clone())
    }

    pub fn new(ports: Ports, cx: &mut Context<Self>) -> Self {
        let (ready_sender, ready_receiver) = oneshot::channel();
        let ready = cx
            .spawn(async move |_, _| ready_receiver.await.unwrap_or(Err(ErrorCode::Unknown)))
            .shared();
        let (wake, mut receiver) = mpsc::unbounded();
        let (writes, mut records) = mpsc::unbounded::<(String, String)>();
        let store = ports.store.clone();
        let writer = cx.spawn(async move |this, cx| {
            while let Some((scope, record)) = records.next().await {
                let key = format!("snapshot/{scope}");
                let save = cx.update(|cx| store.save(key, record, cx));
                if save.await.is_err() {
                    if this
                        .update(cx, |this, cx| this.persistence_failure(scope, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let scheduler = cx.spawn(async move |this, cx| {
            loop {
                let delay = match this.update(cx, |this, cx| {
                    this.evaluate(cx);
                    let now = this.ports.clock.now();
                    let next = this
                        .scheduler
                        .next_deadline(now, this.ports.clock.monotonic())
                        .unwrap_or(now.saturating_add(900));
                    let expiry = this
                        .demands
                        .iter()
                        .filter(|demand| demand.active.get() && demand.until.get() > now)
                        .map(|demand| demand.until.get())
                        .min()
                        .unwrap_or(next);
                    Duration::from_secs(next.min(expiry).saturating_sub(now).clamp(1, 900) as u64)
                }) {
                    Ok(delay) => delay,
                    Err(_) => break,
                };
                futures::select_biased! {
                    value = receiver.next().fuse() => if value.is_none() { break; },
                    _ = cx.background_executor().timer(delay).fuse() => {
                        if let Ok(task) = this.update(cx, |this, cx| this.recheck_inventory("tick", cx)) { task.await; }
                    },
                }
            }
        });
        Self {
            core: QuotaCore::default(),
            scheduler: Scheduler::default(),
            ports,
            tasks: BTreeMap::new(),
            cancellations: BTreeMap::new(),
            demands: Vec::new(),
            wake,
            writes,
            persistence_blocked: BTreeSet::new(),
            hydrated: false,
            ready,
            ready_sender: Some(ready_sender),
            _scheduler: scheduler,
            _writer: writer,
            pending_persistence: BTreeMap::new(),
            _inventory: None,
            inventory_refresh: None,
            codex_installation: None,
            grok_task: None,
            grok_identity: None,
            claude: Default::default(),
            antigravity_inventory: None,
            antigravity_identity: None,
            _auth_watcher: None,
            auth_path: None,
            _project: None,
            _subscriptions: Vec::new(),
        }
    }

    pub fn ready(&self) -> Shared<Task<Result<(), ErrorCode>>> {
        self.ready.clone()
    }
    fn finish_init(&mut self, result: Result<(), ErrorCode>) {
        self.hydrated = result.is_ok();
        if let Some(sender) = self.ready_sender.take()
            && sender.send(result).is_err()
        {
            log::debug!("crew_quota: readiness_receiver_closed");
        }
    }

    pub fn snapshot(&self, selector: &Selector, now: Timestamp) -> Arc<QuotaSnapshot> {
        Arc::new(self.core.snapshot(selector, now))
    }

    pub fn acquire_demand(&mut self, request: FreshRequest, cx: &mut Context<Self>) -> DemandLease {
        let active = Rc::new(Cell::new(true));
        let until = request
            .deadline
            .min(self.ports.clock.now().saturating_add(300));
        let until = Rc::new(Cell::new(until));
        self.demands.push(Demand {
            allow_inference: request.allow_inference,
            selector: request.selector,
            until: until.clone(),
            active: active.clone(),
        });
        self.evaluate(cx);
        self.wake();
        DemandLease {
            until,
            active,
            wake: self.wake.clone(),
        }
    }

    pub fn ensure_fresh(
        &mut self,
        request: FreshRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<FreshResult, RefreshError>> {
        match self.core.fresh(&request, self.ports.clock.now()) {
            Ok(result) => return Task::ready(Ok(result)),
            Err(
                error @ (RefreshError::NotInstalled
                | RefreshError::NotLoggedIn
                | RefreshError::Unsupported),
            ) => return Task::ready(Err(error)),
            Err(_) => {}
        }
        if self.core.snapshot.accounts.values().any(|account| {
            request.selector.matches(&account.scope) && account.scope.provider == ProviderId::Grok
        }) {
            return Task::ready(Err(RefreshError::NotRefreshable));
        }
        if request.deadline <= self.ports.clock.now() {
            return Task::ready(Err(RefreshError::Deadline));
        }
        for account in self
            .core
            .snapshot
            .accounts
            .values()
            .filter(|account| request.selector.matches(&account.scope))
        {
            self.scheduler.force(&account.scope.id);
        }
        let lease = self.acquire_demand(request.clone(), cx);
        let targets = self
            .core
            .snapshot
            .accounts
            .values()
            .filter(|account| request.selector.matches(&account.scope))
            .map(|account| account.scope.id.clone())
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Task::ready(Err(RefreshError::Unknown));
        }
        let mut tasks = Vec::new();
        for scope in targets {
            let Some(state) = self.scheduler.scopes.get(&scope) else {
                return Task::ready(Err(RefreshError::Unknown));
            };
            if !matches!(state.provider, ProviderId::Codex | ProviderId::Antigravity)
                && (state.provider != ProviderId::Claude || !request.allow_inference)
            {
                return Task::ready(Err(RefreshError::NotRefreshable));
            }
            if let Some(task) = self.tasks.get(&scope) {
                tasks.push(task.clone());
            } else {
                let retry_at = Scheduler::next_for(
                    state,
                    self.ports.clock.now(),
                    self.ports.clock.monotonic(),
                );
                return Task::ready(Err(RefreshError::Deferred { retry_at }));
            }
        }
        let seconds = request
            .deadline
            .saturating_sub(self.ports.clock.now())
            .max(0) as u64;
        cx.spawn(async move |this, cx| {
            let _lease = lease;
            let wait = async { for task in tasks { task.await?; } Ok::<_, RefreshError>(()) };
            futures::select_biased! { result = wait.fuse() => result?, _ = cx.background_executor().timer(Duration::from_secs(seconds)).fuse() => return Err(RefreshError::Deadline) }
            this.read_with(cx, |this, _| this.core.fresh(&request, this.ports.clock.now())).map_err(|_| RefreshError::Cancelled)?
        })
    }

    pub fn refresh(
        &mut self,
        selector: Selector,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), RefreshError>> {
        let inventory = self.recheck_inventory("manual", cx);
        cx.spawn(async move |this, cx| {
            inventory.await;
            let pending = this
                .update(cx, |service, cx| {
                    for account in service
                        .core
                        .snapshot
                        .accounts
                        .values()
                        .filter(|account| selector.matches(&account.scope))
                    {
                        service.scheduler.force(&account.scope.id);
                    }
                    service.evaluate(cx);
                    service
                        .tasks
                        .iter()
                        .filter(|(scope, _)| {
                            service
                                .core
                                .snapshot
                                .accounts
                                .get(*scope)
                                .is_some_and(|account| selector.matches(&account.scope))
                        })
                        .map(|(_, task)| task.clone())
                        .collect::<Vec<_>>()
                })
                .map_err(|_| RefreshError::Unknown)?;
            for task in pending {
                if task.await.is_err() {
                    // The completion path records failures on the affected account.
                    log::debug!("crew_quota: refresh_provider_failed");
                }
            }
            Ok(())
        })
    }

    fn recheck_inventory(
        &mut self,
        trigger: &'static str,
        cx: &mut Context<Self>,
    ) -> Shared<Task<()>> {
        if let Some(task) = &self.inventory_refresh {
            return task.clone();
        }
        log::info!("crew_quota: refresh_trigger={trigger}");
        let Some(project) = self._project.clone() else {
            return Task::ready(()).shared();
        };
        let task = cx
            .spawn(async move |this, cx| {
                let Some(service) = this.upgrade() else {
                    return;
                };
                cx.update(|cx| {
                    grok::sync(&service, cx);
                    claude::sync(&service, &project, cx);
                    antigravity::sync(&service, &project, cx);
                    service.update(cx, |service, cx| {
                        sync_inventory(
                            service,
                            &project.read(cx).agent_server_store().clone(),
                            false,
                            cx,
                        )
                    });
                });
                let tasks = service.update(cx, |service, _| {
                    [
                        service._inventory.take(),
                        service.claude.inventory.take(),
                        service.antigravity_inventory.take(),
                    ]
                });
                for task in tasks.into_iter().flatten() {
                    task.await;
                }
                service.update(cx, |service, _| service.inventory_refresh = None);
            })
            .shared();
        self.inventory_refresh = Some(task.clone());
        task
    }

    pub fn observe(
        &mut self,
        observation: Observation,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorCode> {
        log::info!(
            "crew_quota: agent={:?} source={:?} outcome=Observed",
            self.core
                .snapshot
                .accounts
                .get(&observation.scope)
                .map(|account| &account.scope.provider),
            observation.source
        );
        let live = observation.source != SourceKind::Persisted;
        let scope = observation.scope.clone();
        let previous_availability = self
            .core
            .snapshot
            .accounts
            .get(&scope)
            .map(|account| account.availability.clone());
        let previous_revision = self.core.snapshot.revision;
        let changed = self.core.apply(observation, self.ports.clock.now())?;
        if self.core.snapshot.revision != previous_revision {
            if previous_availability
                != self
                    .core
                    .snapshot
                    .accounts
                    .get(&scope)
                    .map(|account| account.availability.clone())
            {
                cx.emit(QuotaEvent::AvailabilityChanged(scope.clone()));
            }
            if live && !changed.is_empty() {
                self.scheduler.passive(
                    &scope,
                    self.ports.clock.now(),
                    self.ports.clock.monotonic(),
                );
            }
            self.persist(&scope, cx);
            cx.emit(QuotaEvent::Changed {
                scopes: vec![scope],
                windows: changed,
                revision: self.core.snapshot.revision,
            });
            cx.notify();
        }
        self.wake();
        Ok(())
    }

    fn watch_auth(
        &mut self,
        home: std::path::PathBuf,
        fs: Arc<dyn fs::Fs>,
        cx: &mut Context<Self>,
    ) {
        if self.auth_path.as_ref() == Some(&home) {
            return;
        }
        self.auth_path = Some(home.clone());
        self._auth_watcher = Some(cx.spawn(async move |this, cx| {
            let (mut events, watcher) = fs.watch(&home, Duration::from_millis(100)).await;
            let _watcher = watcher;
            while let Some(events) = events.next().await {
                if events.iter().any(|event| {
                    event.path == home.join("auth.json") || event.path == home.join("config.toml")
                }) {
                    if this
                        .update(cx, |this, cx| this.invalidate_auth(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }));
    }

    fn invalidate_auth(&mut self, cx: &mut Context<Self>) {
        let scopes: Vec<_> = self
            .core
            .snapshot
            .accounts
            .values()
            .filter(|account| account.scope.provider == ProviderId::Codex)
            .map(|account| account.scope.id.clone())
            .collect();
        for scope in &scopes {
            if let Some(handle) = self.cancellations.remove(scope) {
                handle.abort();
            }
            self.tasks.remove(scope);
        }
        let windows = self
            .core
            .snapshot
            .accounts
            .values()
            .filter(|account| account.scope.provider == ProviderId::Codex)
            .flat_map(|account| account.windows.keys().cloned())
            .collect();
        for account in self
            .core
            .snapshot
            .accounts
            .values_mut()
            .filter(|account| account.scope.provider == ProviderId::Codex)
        {
            account.generation = account.generation.saturating_add(1);
            account.windows.clear();
            account.metadata = AccountMetadata::default();
            account.complete = false;
            account.availability = Availability::Ready;
            account.refresh = RefreshState::Idle;
            self.scheduler.track(
                account.scope.id.clone(),
                account.scope.provider,
                account.generation,
            );
            cx.emit(QuotaEvent::AvailabilityChanged(account.scope.id.clone()));
        }
        self.core.snapshot.revision = self.core.snapshot.revision.saturating_add(1);
        cx.emit(QuotaEvent::Changed {
            scopes,
            windows,
            revision: self.core.snapshot.revision,
        });
        self.wake();
        cx.notify();
    }

    fn wake(&self) {
        if self.wake.unbounded_send(()).is_err() {
            log::debug!("crew_quota: scheduler_closed");
        }
    }
    fn persistence_failure(&mut self, scope: String, cx: &mut Context<Self>) {
        log::warn!("crew_quota: persistence_failed");
        cx.emit(QuotaEvent::PersistenceFailed(scope));
    }
    fn persist(&mut self, scope: &str, cx: &mut Context<Self>) {
        if self.pending_persistence.contains_key(scope) {
            return;
        }
        let scope = scope.to_string();
        let task = cx.spawn({
            let scope = scope.clone();
            async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                if this
                    .update(cx, |this, cx| {
                        this.pending_persistence.remove(&scope);
                        this.write_snapshot(&scope, cx);
                    })
                    .is_err()
                {
                    log::debug!("crew_quota: persistence_service_closed");
                }
            }
        });
        self.pending_persistence.insert(scope, task);
    }
    fn write_snapshot(&mut self, scope: &str, cx: &mut Context<Self>) {
        if self.persistence_blocked.contains(scope) {
            return;
        }
        let Some(account) = self.core.snapshot.accounts.get(scope) else {
            return;
        };
        let schedule = self.scheduler.scopes.get(scope);
        let record = StoredRecord {
            schema_version: 1,
            revision: self.core.snapshot.revision,
            account: account.clone(),
            failures: schedule.map_or(0, |schedule| schedule.failures),
            retry_at: schedule.map_or(0, |schedule| schedule.retry_at),
        };
        match encode_record(&record) {
            Ok(record) => {
                if self.writes.unbounded_send((scope.into(), record)).is_err() {
                    self.persistence_failure(scope.into(), cx);
                }
            }
            Err(_) => self.persistence_failure(scope.into(), cx),
        }
    }

    fn evaluate(&mut self, cx: &mut Context<Self>) {
        if !self.hydrated {
            return;
        }
        let now = self.ports.clock.now();
        let monotonic = self.ports.clock.monotonic();
        self.core.expire(now);
        self.demands
            .retain(|demand| demand.active.get() && demand.until.get() > now);
        for state in self.scheduler.scopes.values_mut() {
            state.demand_until = 0;
        }
        for demand in &self.demands {
            for account in self.core.snapshot.accounts.values().filter(|account| {
                demand.selector.matches(&account.scope)
                    && (account.scope.provider != ProviderId::Claude || demand.allow_inference)
            }) {
                self.scheduler.demand(&account.scope.id, demand.until.get());
            }
        }
        for account in self.core.snapshot.accounts.values() {
            if account.scope.provider == ProviderId::Claude {
                let fresh = ["five_hour", "seven_day"].iter().all(|bucket| {
                    account.windows.values().any(|window| {
                        window.key.bucket.bucket == *bucket
                            && window.freshness(now, Duration::from_secs(120)) == Freshness::Fresh
                    })
                });
                if fresh {
                    if let Some(schedule) = self.scheduler.scopes.get_mut(&account.scope.id) {
                        schedule.demand_until = 0;
                    }
                } else {
                    self.scheduler.force(&account.scope.id);
                }
            }
            let reset = account
                .windows
                .values()
                .filter_map(|window| window.reset_at.as_ref().and_then(|sample| sample.value))
                .filter(|reset| {
                    self.scheduler
                        .scopes
                        .get(&account.scope.id)
                        .is_some_and(|schedule| !schedule.handled_resets.contains(reset))
                })
                .min();
            if let Some(reset) = reset {
                self.scheduler.reset_due(&account.scope.id, reset);
            }
            cx.emit(QuotaEvent::FreshnessChanged(account.scope.id.clone()));
        }
        for (scope, schedule) in &mut self.scheduler.scopes {
            if self
                .core
                .snapshot
                .accounts
                .get(scope)
                .is_some_and(|account| {
                    matches!(
                        account.availability,
                        Availability::NotInstalled
                            | Availability::NotLoggedIn
                            | Availability::Unsupported(_)
                    )
                })
            {
                schedule.suspended = true;
            }
        }
        for (scope, generation) in self.scheduler.start_due(now, monotonic) {
            let Some(account) = self.core.snapshot.accounts.get_mut(&scope) else {
                continue;
            };
            if account.availability != Availability::Ready
                && !matches!(
                    account.availability,
                    Availability::Error(_) | Availability::Unknown | Availability::Unavailable(_)
                )
            {
                if let Some(schedule) = self.scheduler.scopes.get_mut(&scope) {
                    schedule.in_flight = false;
                    schedule.suspended = true;
                }
                continue;
            }
            log::info!(
                "crew_quota: agent={:?} source=Active outcome=Attempt",
                account.scope.provider
            );
            account.last_attempt_at = Some(now);
            account.refresh = RefreshState::InFlight;
            let sequence = account.sequence;
            let source = self.ports.source.clone();
            let reservation = serde_json::to_string(&AttemptRecord {
                schema_version: 1,
                scope: scope.clone(),
                provider: account.scope.provider,
                at: now,
            })
            .map_err(|_| ErrorCode::InvalidData);
            let reservation = match reservation {
                Ok(reservation) if !self.persistence_blocked.contains(&scope) => self
                    .ports
                    .store
                    .save(format!("attempt/{scope}"), reservation, cx),
                _ => Task::ready(Err(ErrorCode::Persistence)),
            };
            cx.emit(QuotaEvent::RefreshStateChanged(scope.clone()));
            let (handle, registration) = futures::future::AbortHandle::new_pair();
            self.cancellations.insert(scope.clone(), handle);
            let task = cx
                .spawn({
                    let scope = scope.clone();
                    async move |this, cx| {
                        let operation = async {
                            reservation
                                .await
                                .map_err(|_| RefreshError::Failed(ErrorCode::Persistence))?;
                            let current = this
                                .read_with(cx, |this, _| {
                                    this.core
                                        .snapshot
                                        .accounts
                                        .get(&scope)
                                        .is_some_and(|account| account.generation == generation)
                                })
                                .map_err(|_| RefreshError::Cancelled)?;
                            if !current {
                                return Err(RefreshError::Cancelled);
                            }
                            let read = cx.update(|cx| {
                                source.refresh(scope.clone(), generation, sequence, cx)
                            });
                            read.await
                        };
                        let result = futures::future::Abortable::new(operation, registration)
                            .await
                            .unwrap_or(Err(RefreshError::Cancelled));
                        this.update(cx, |this, cx| this.complete(&scope, generation, result, cx))
                            .map_err(|_| RefreshError::Cancelled)?
                    }
                })
                .shared();
            self.tasks.insert(scope, task);
        }
    }

    fn complete(
        &mut self,
        scope: &str,
        generation: u64,
        result: Result<Observation, RefreshError>,
        cx: &mut Context<Self>,
    ) -> Result<(), RefreshError> {
        if self
            .core
            .snapshot
            .accounts
            .get(scope)
            .is_none_or(|account| account.generation != generation)
        {
            return Err(RefreshError::Cancelled);
        }
        self.cancellations.remove(scope);
        self.tasks.remove(scope);
        let now = self.ports.clock.now();
        let result = result.and_then(|observation| {
            self.core
                .apply(observation, now)
                .map_err(RefreshError::Failed)
        });
        log::info!(
            "crew_quota: agent={:?} source=Active outcome={:?}",
            self.core
                .snapshot
                .accounts
                .get(scope)
                .map(|account| account.scope.provider),
            result.as_ref().map(|_| ()).map_err(|error| match error {
                RefreshError::Deferred { .. } => RefreshError::Deferred { retry_at: 0 },
                other => other.clone(),
            })
        );
        let retry_at = match &result {
            Err(RefreshError::Deferred { retry_at }) => Some(*retry_at),
            _ => None,
        };
        self.scheduler.complete(
            scope,
            now,
            self.ports.clock.monotonic(),
            result.is_ok(),
            retry_at,
        );
        if let Some(account) = self.core.snapshot.accounts.get_mut(scope) {
            account.refresh = match &result {
                Ok(_) => RefreshState::Idle,
                Err(RefreshError::NotLoggedIn) => {
                    account.availability = Availability::NotLoggedIn;
                    if let Some(schedule) = self.scheduler.scopes.get_mut(scope) {
                        schedule.suspended = true;
                    }
                    RefreshState::Suspended(ErrorCode::Unknown)
                }
                Err(RefreshError::Failed(
                    code
                    @ (ErrorCode::CleanupFailed | ErrorCode::Protocol | ErrorCode::InvalidData),
                )) if account.scope.provider == ProviderId::Antigravity
                    || *code == ErrorCode::CleanupFailed =>
                {
                    account.availability = Availability::Error(*code);
                    if let Some(schedule) = self.scheduler.scopes.get_mut(scope) {
                        schedule.suspended = true;
                    }
                    RefreshState::Suspended(*code)
                }
                Err(RefreshError::Failed(
                    code @ (ErrorCode::InferenceAccess | ErrorCode::StoredAuthUnsupported),
                )) => {
                    account.availability = if *code == ErrorCode::StoredAuthUnsupported {
                        Availability::Unsupported(*code)
                    } else {
                        Availability::Unavailable(*code)
                    };
                    if let Some(schedule) = self.scheduler.scopes.get_mut(scope) {
                        schedule.suspended = true;
                    }
                    RefreshState::Suspended(*code)
                }
                Err(RefreshError::UnsupportedPlatform) => {
                    account.availability =
                        Availability::Unsupported(ErrorCode::UnsupportedPlatform);
                    if let Some(schedule) = self.scheduler.scopes.get_mut(scope) {
                        schedule.suspended = true;
                    }
                    RefreshState::Suspended(ErrorCode::UnsupportedPlatform)
                }
                Err(RefreshError::Unsupported) => {
                    account.availability = Availability::Unsupported(ErrorCode::UnsupportedVersion);
                    if let Some(schedule) = self.scheduler.scopes.get_mut(scope) {
                        schedule.suspended = true;
                    }
                    RefreshState::Suspended(ErrorCode::UnsupportedVersion)
                }
                Err(error) => {
                    account.availability = Availability::Error(match error {
                        RefreshError::Failed(code) => *code,
                        _ => ErrorCode::Unknown,
                    });
                    RefreshState::Backoff {
                        retry_at: self
                            .scheduler
                            .scopes
                            .get(scope)
                            .map_or(now.saturating_add(120), |state| state.retry_at),
                    }
                }
            };
        }
        if matches!(result, Err(RefreshError::Failed(ErrorCode::Persistence))) {
            self.persistence_failure(scope.into(), cx);
        }
        self.persist(scope, cx);
        cx.emit(QuotaEvent::AvailabilityChanged(scope.into()));
        cx.emit(QuotaEvent::RefreshStateChanged(scope.into()));
        if let Ok(windows) = &result {
            cx.emit(QuotaEvent::Changed {
                scopes: vec![scope.into()],
                windows: windows.clone(),
                revision: self.core.snapshot.revision,
            });
        }
        cx.notify();
        self.wake();
        result.map(|_| ())
    }
}

struct CodexSource {
    project: Entity<Project>,
    node_runtime: node_runtime::NodeRuntime,
    fs: Arc<dyn fs::Fs>,
}
impl QuotaSource for CodexSource {
    fn refresh(
        &self,
        scope: String,
        generation: u64,
        start_sequence: u64,
        cx: &mut App,
    ) -> Task<Result<Observation, RefreshError>> {
        if scope == "antigravity-registry" {
            return antigravity::refresh(
                self.project.clone(),
                scope,
                generation,
                start_sequence,
                cx,
            );
        }
        if scope.starts_with("claude-") {
            return claude::refresh(self.project.clone(), scope, generation, start_sequence, cx);
        }
        let project = self.project.clone();
        let node_runtime = self.node_runtime.clone();
        let fs = self.fs.clone();
        cx.spawn(async move |cx| {
            let executor = cx.background_executor().clone();
            let launch = {
                let timeout = executor.timer(Duration::from_secs(15)).fuse();
                let prepare = resolve(&project, node_runtime, cx).fuse();
                futures::pin_mut!(prepare, timeout);
                futures::select_biased! { result = prepare => result?, _ = timeout => return Err(RefreshError::Failed(ErrorCode::Timeout)) }
            };
            let home = launch.environment.get("CODEX_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| launch.environment.get("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| paths::home_dir().to_path_buf()).join(".codex"));
            cx.update(|cx| {
                if let Some(service) = QuotaService::global(cx) { service.update(cx, |service, cx| service.watch_auth(home, fs, cx)); }
            });
            let version = launch.version.clone();
            let executor = cx.background_executor().clone();
            let response = cx
                .background_spawn(async move { providers::read(launch, executor).await })
                .await?;
            providers::decode(
                &response,
                &scope,
                generation,
                &version,
                chrono::Utc::now().timestamp(),
                start_sequence.saturating_add(1),
                start_sequence,
            )
        })
    }
}

async fn resolve(
    project: &Entity<Project>,
    node_runtime: node_runtime::NodeRuntime,
    cx: &mut gpui::AsyncApp,
) -> Result<providers::Launch, RefreshError> {
    let status = project
        .read_with(cx, |project, _| project.agent_server_store().clone())
        .update(cx, |store, cx| {
            store
                .get_external_agent(&AgentId::new("codex-acp"))
                .map(|agent| agent.install_status(cx))
        })
        .ok_or(RefreshError::NotInstalled)?;
    match status.await {
        Ok(AgentInstallStatus::Installed { .. }) => {}
        Ok(AgentInstallStatus::NotInstalled) => return Err(RefreshError::NotInstalled),
        _ => return Err(RefreshError::Unsupported),
    }
    let (registry, settings_env, environment) = cx.update(|cx| {
        let settings = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None);
        let Some(CustomAgentServerSettings::Registry { env, .. }) = settings.get("codex-acp")
        else {
            return Err(RefreshError::Unsupported);
        };
        let registry = AgentRegistryStore::try_global(cx)
            .and_then(|store| {
                store
                    .read(cx)
                    .agents()
                    .iter()
                    .find(|agent| agent.id().0.as_ref() == "codex-acp")
                    .cloned()
            })
            .ok_or(RefreshError::Unsupported)?;
        let settings_env = env.clone();
        let environment = project
            .read(cx)
            .environment()
            .clone()
            .update(cx, |environment, cx| environment.default_environment(cx));
        Ok((registry, settings_env, environment))
    })?;
    let RegistryAgent::Npx(registry) = registry else {
        return Err(RefreshError::Unsupported);
    };
    if !registry.args.is_empty() {
        return Err(RefreshError::Unsupported);
    }
    let mut environment = environment
        .await
        .ok_or(RefreshError::Unsupported)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let root = paths::external_agents_dir().join("registry/npx/codex-acp/node_modules");
    let (entrypoint, version) = providers::installed_entrypoint(&root, &registry.package)
        .await
        .map_err(|error| {
            if error == ErrorCode::Transport {
                RefreshError::NotInstalled
            } else {
                RefreshError::Unsupported
            }
        })?;
    let node = node_runtime
        .binary_path()
        .await
        .map_err(|_| RefreshError::Unsupported)?;
    environment.extend(node_runtime::npm_command_env(&node));
    environment.extend(registry.env);
    environment.extend(settings_env);
    let directory = paths::data_dir().join("crew/quota/read");
    smol::fs::create_dir_all(&directory)
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
    Ok(providers::Launch {
        node,
        entrypoint,
        version,
        environment,
        directory,
    })
}

pub fn init(app_state: Arc<AppState>, cx: &mut App) {
    let project = Project::local(
        app_state.client.clone(),
        app_state.node_runtime.clone(),
        app_state.user_store.clone(),
        app_state.languages.clone(),
        app_state.fs.clone(),
        None,
        LocalProjectFlags {
            init_worktree_trust: false,
            watch_global_configs: false,
        },
        cx,
    );
    let store = project.read(cx).agent_server_store().clone();
    let service = cx.new(|cx| {
        QuotaService::new(
            Ports {
                source: Rc::new(CodexSource {
                    project: project.clone(),
                    node_runtime: app_state.node_runtime.clone(),
                    fs: app_state.fs.clone(),
                }),
                store: Rc::new(KvpStore),
                clock: Rc::new(SystemClock(Instant::now())),
            },
            cx,
        )
    });
    cx.set_global(GlobalQuota(service.clone()));
    cx.observe_new::<acp_thread::AcpThread>({
        let service = service.downgrade();
        move |thread, _, cx| {
            if !thread.project().read(cx).is_local() {
                return;
            }
            let service = service.clone();
            cx.defer(move |cx| {
                if let Ok(task) =
                    service.update(cx, |service, cx| service.recheck_inventory("thread", cx))
                {
                    cx.spawn(async move |_| task.await).detach();
                }
            });
        }
    })
    .detach();
    grok::init(&service, cx);
    panel::init(cx);
    claude::init(&service, &project, cx);
    antigravity::init(&service, &project, cx);
    service.update(cx, |service, _| service._project = Some(project.clone()));
    let subscription = cx.subscribe(&store, {
        let service = service.downgrade();
        move |store, _: &AgentServersUpdated, cx| {
            log::info!("crew_quota: refresh_trigger=agent_servers_updated");
            if service
                .update(cx, |service, cx| sync_inventory(service, &store, true, cx))
                .is_err()
            {
                log::debug!("crew_quota: inventory_service_closed");
            }
        }
    });
    service.update(cx, |service, cx| {
        service._subscriptions.push(subscription);
        sync_inventory(
            service,
            &project.read(cx).agent_server_store().clone(),
            false,
            cx,
        );
    });
}

fn sync_inventory(
    service: &mut QuotaService,
    store: &Entity<AgentServerStore>,
    changed: bool,
    cx: &mut Context<QuotaService>,
) {
    let status = store.update(cx, |store, cx| {
        store
            .get_external_agent(&AgentId::new("codex-acp"))
            .map(|agent| agent.install_status(cx))
    });
    let Some(status) = status else {
        log::info!("crew_quota: inventory agent=codex-acp installation=not_configured");
        let scopes: Vec<_> = service
            .core
            .snapshot
            .accounts
            .values()
            .filter(|account| account.scope.provider == ProviderId::Codex)
            .map(|account| account.scope.id.clone())
            .collect();
        for scope in &scopes {
            if let Some(handle) = service.cancellations.remove(scope) {
                handle.abort();
            }
            service.tasks.remove(scope);
            service.core.snapshot.accounts.remove(scope);
            service.scheduler.scopes.remove(scope);
        }
        service.core.snapshot.revision = service.core.snapshot.revision.saturating_add(1);
        service.finish_init(Ok(()));
        cx.emit(QuotaEvent::Changed {
            scopes,
            windows: Vec::new(),
            revision: service.core.snapshot.revision,
        });
        service.wake();
        cx.notify();
        return;
    };
    let store = service.ports.store.clone();
    service._inventory = Some(cx.spawn(async move |this, cx| {
        let status = status.await;
        let mapping = cx
            .update(|cx| store.load("inventory/codex-acp".into(), cx))
            .await;
        let scope_id = match mapping {
            Ok(Some(value)) if value.len() < 128 && uuid::Uuid::parse_str(&value).is_ok() => value,
            Ok(None) => {
                let value = uuid::Uuid::new_v4().to_string();
                if cx
                    .update(|cx| store.save("inventory/codex-acp".into(), value.clone(), cx))
                    .await
                    .is_err()
                {
                    if this
                        .update(cx, |this, cx| {
                            this.persistence_failure("inventory".into(), cx);
                            this.finish_init(Err(ErrorCode::Persistence));
                        })
                        .is_err()
                    {
                        log::debug!("crew_quota: inventory_service_closed");
                    }
                    return;
                }
                value
            }
            _ => {
                if this
                    .update(cx, |this, cx| {
                        this.persistence_failure("inventory".into(), cx);
                        this.finish_init(Err(ErrorCode::Persistence));
                    })
                    .is_err()
                {
                    log::debug!("crew_quota: inventory_service_closed");
                }
                return;
            }
        };
        let scope = AccountScope {
            id: scope_id.clone(),
            provider: ProviderId::Codex,
            runtime: "local".into(),
            identity_known: false,
        };
        let record = cx
            .update(|cx| store.load(format!("snapshot/{scope_id}"), cx))
            .await;
        let attempt = cx
            .update(|cx| store.load(format!("attempt/{scope_id}"), cx))
            .await;
        if this
            .update(cx, |this, cx| {
                let generation = this
                    .core
                    .snapshot
                    .accounts
                    .get(&scope_id)
                    .map_or(1, |account| account.generation.saturating_add(1));
                let availability = match status {
                    Ok(AgentInstallStatus::Installed { .. }) => Availability::Ready,
                    Ok(AgentInstallStatus::NotInstalled) => Availability::NotInstalled,
                    Ok(AgentInstallStatus::Unmanaged) => {
                        Availability::Unsupported(ErrorCode::UnsupportedVersion)
                    }
                    Err(_) => Availability::Error(ErrorCode::Transport),
                };
                log::info!(
                    "crew_quota: inventory agent=codex-acp installation={}",
                    installation_code(&availability)
                );
                let installation = installation_code(&availability);
                if !changed && this.codex_installation == Some(installation) {
                    this.finish_init(Ok(()));
                    return;
                }
                this.codex_installation = Some(installation);
                if let Some(handle) = this.cancellations.remove(&scope_id) {
                    handle.abort();
                }
                this.tasks.remove(&scope_id);
                if this
                    .core
                    .inventory(scope.clone(), generation, availability)
                    .is_err()
                {
                    this.finish_init(Err(ErrorCode::InvalidData));
                    return;
                }
                this.scheduler
                    .track(scope_id.clone(), ProviderId::Codex, generation);
                match record {
                    Ok(Some(record)) => {
                        match decode_record(&record, &scope, this.ports.clock.now()) {
                            Ok(record) => {
                                // Installation scope cannot prove account identity across restarts; retain attempt guards, never hydrate numbers into an unverified login.
                                this.scheduler.restore_timing(
                                    &scope_id,
                                    record.account.last_attempt_at,
                                    record.retry_at,
                                    record.failures,
                                    this.ports.clock.now(),
                                    this.ports.clock.monotonic(),
                                );
                            }
                            Err(error) => {
                                if error == ErrorCode::UnsupportedVersion {
                                    this.persistence_blocked.insert(scope_id.clone());
                                }
                                this.persistence_failure(scope_id.clone(), cx);
                            }
                        }
                    }
                    Err(_) => this.persistence_failure(scope_id.clone(), cx),
                    Ok(None) => {}
                }
                match attempt {
                    Ok(Some(record)) => {
                        match decode_attempt(&record, &scope, this.ports.clock.now()) {
                            Ok(attempt) => {
                                this.scheduler.restore_timing(
                                    &scope_id,
                                    Some(attempt.at),
                                    0,
                                    0,
                                    this.ports.clock.now(),
                                    this.ports.clock.monotonic(),
                                );
                            }
                            Err(error) => {
                                if error == ErrorCode::UnsupportedVersion {
                                    this.persistence_blocked.insert(scope_id.clone());
                                }
                                this.persistence_failure(scope_id.clone(), cx);
                            }
                        }
                    }
                    Err(_) => this.persistence_failure(scope_id.clone(), cx),
                    Ok(None) => {}
                }
                this.finish_init(Ok(()));
                cx.emit(QuotaEvent::AvailabilityChanged(scope_id));
                this.evaluate(cx);
                this.wake();
            })
            .is_err()
        {
            log::debug!("crew_quota: inventory_service_closed");
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use std::cell::RefCell;

    struct FakeClock {
        wall: Cell<i64>,
        elapsed: Cell<u64>,
    }
    impl Clock for FakeClock {
        fn now(&self) -> i64 {
            self.wall.get()
        }
        fn monotonic(&self) -> u64 {
            self.elapsed.get()
        }
    }
    #[derive(Default)]
    struct MemoryStore {
        records: RefCell<BTreeMap<String, String>>,
        fail: Cell<bool>,
    }
    impl QuotaStore for MemoryStore {
        fn load(&self, key: String, _: &App) -> Task<Result<Option<String>, ErrorCode>> {
            Task::ready(Ok(self.records.borrow().get(&key).cloned()))
        }
        fn save(&self, key: String, value: String, _: &App) -> Task<Result<(), ErrorCode>> {
            if self.fail.get() {
                return Task::ready(Err(ErrorCode::Persistence));
            }
            self.records.borrow_mut().insert(key, value);
            Task::ready(Ok(()))
        }
    }
    struct FakeSource {
        calls: Cell<usize>,
        clock: Rc<FakeClock>,
        fail: Cell<bool>,
    }
    impl QuotaSource for FakeSource {
        fn refresh(
            &self,
            scope: String,
            generation: u64,
            start_sequence: u64,
            cx: &mut App,
        ) -> Task<Result<Observation, RefreshError>> {
            self.calls.set(self.calls.get() + 1);
            let at = self.clock.now();
            let fail = self.fail.get();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if fail {
                    return Err(RefreshError::Failed(ErrorCode::Transport));
                }
                Ok(Observation {
                    scope: scope.clone(),
                    generation,
                    source: SourceKind::CodexAppServerRead,
                    source_version: "2.5.0".into(),
                    observed_at: at,
                    provider_sample_at: None,
                    sequence: start_sequence.saturating_add(1),
                    start_sequence: Some(start_sequence),
                    complete: true,
                    metadata: AccountMetadata::default(),
                    windows: vec![WindowPatch {
                        key: WindowKey {
                            bucket: BucketKey {
                                scope,
                                bucket: "codex".into(),
                                applicability: Applicability::AllModels,
                            },
                            role: "primary".into(),
                            duration_seconds: Some(18000),
                        },
                        used_fraction: Patch::Set(Some(0.6)),
                        reset_at: Patch::Set(None),
                        remaining: Patch::Missing,
                        limit: Patch::Missing,
                        admission: Patch::Set(Admission::Allowed),
                        disabled: Patch::Missing,
                        precision: Precision::Fraction,
                    }],
                })
            })
        }
    }
    #[gpui::test]
    fn passive_grok_snapshot_persists_without_active_schedule(cx: &mut TestAppContext) {
        let (service, _, _, store) = harness(cx);
        service.update(cx, |service, cx| {
            service.scheduler.scopes.remove("scope");
            service
                .core
                .snapshot
                .accounts
                .get_mut("scope")
                .expect("scope")
                .scope
                .provider = ProviderId::Grok;
            service.write_snapshot("scope", cx);
        });
        cx.run_until_parked();
        assert!(store.records.borrow().contains_key("snapshot/scope"));
    }

    #[gpui::test]
    fn claude_fake_v1_v2_readers_survive_inventory_without_duplicate_subscriptions(
        cx: &mut TestAppContext,
    ) {
        for identity in [1, 2] {
            let (service, _, _, _) = harness(cx);
            let (sender, receiver) = async_channel::unbounded();
            service.update(cx, |service, cx| {
                let account = service
                    .core
                    .snapshot
                    .accounts
                    .get_mut("scope")
                    .expect("account");
                account.scope.provider = ProviderId::Claude;
                claude::attach_reader(
                    service,
                    identity,
                    "scope".into(),
                    "fixture".into(),
                    Vec::new(),
                    receiver.clone(),
                    cx,
                );
                claude::attach_reader(
                    service,
                    identity,
                    "scope".into(),
                    "second-session".into(),
                    Vec::new(),
                    receiver,
                    cx,
                );
                assert_eq!(service.claude.readers.len(), 1);
                let scope = service.core.snapshot.accounts["scope"].scope.clone();
                service
                    .core
                    .inventory(scope, 2, Availability::Unknown)
                    .expect("generation");
            });
            sender
                .try_send(agent_servers::AcpDebugMessage {
                    direction: agent_servers::AcpDebugMessageDirection::Incoming,
                    message: agent_servers::AcpDebugMessageContent::Notification {
                        method: "session/update".into(),
                        params: Some(
                            serde_json::from_str(include_str!(
                                "../tests/fixtures/claude-passive.sanitized.json"
                            ))
                            .expect("fixture"),
                        ),
                    },
                })
                .expect("send");
            cx.run_until_parked();
            service.read_with(cx, |service, _| {
                let account = &service.core.snapshot.accounts["scope"];
                assert_eq!(account.generation, 2);
                assert!(!account.windows.is_empty());
                assert_eq!(service.claude.readers.len(), 1);
            });
        }
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (
        Entity<QuotaService>,
        Rc<FakeClock>,
        Rc<FakeSource>,
        Rc<MemoryStore>,
    ) {
        let clock = Rc::new(FakeClock {
            wall: Cell::new(1000),
            elapsed: Cell::new(0),
        });
        let source = Rc::new(FakeSource {
            calls: Cell::new(0),
            clock: clock.clone(),
            fail: Cell::new(false),
        });
        let store = Rc::new(MemoryStore::default());
        let service = cx.new(|cx| {
            QuotaService::new(
                Ports {
                    clock: clock.clone(),
                    source: source.clone(),
                    store: store.clone(),
                },
                cx,
            )
        });
        service.update(cx, |service, _| {
            assert!(
                service
                    .core
                    .inventory(
                        AccountScope {
                            id: "scope".into(),
                            provider: ProviderId::Codex,
                            runtime: "local".into(),
                            identity_known: false,
                        },
                        1,
                        Availability::Ready,
                    )
                    .is_ok()
            );
            service
                .scheduler
                .track("scope".into(), ProviderId::Codex, 1);
            service.hydrated = true;
        });
        (service, clock, source, store)
    }
    fn request() -> FreshRequest {
        FreshRequest {
            selector: Selector::default(),
            max_age: Duration::from_secs(120),
            required_measurements: Vec::new(),
            deadline: 1300,
            allow_inference: false,
        }
    }

    #[gpui::test]
    async fn tick_rechecks_installation_without_agent_servers_updated(cx: &mut TestAppContext) {
        use project::agent_registry_store::{RegistryAgentMetadata, RegistryNpxAgent};
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            AgentRegistryStore::init_test_global(
                cx,
                vec![RegistryAgent::Npx(RegistryNpxAgent {
                    metadata: RegistryAgentMetadata {
                        id: AgentId::new("codex-acp"),
                        name: "Codex".into(),
                        description: "".into(),
                        version: "1.0.0".into(),
                        repository: None,
                        website: None,
                        license_url: None,
                        icon_path: None,
                    },
                    package: "codex-acp".into(),
                    args: Vec::new(),
                    env: Default::default(),
                })],
            );
        });
        let filesystem = fs::FakeFs::new(cx.executor());
        let project = Project::test(filesystem.clone(), [], cx).await;
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings
                    .set_user_settings(r#"{"agent_servers":{"codex-acp":{"type":"registry"}}}"#, cx)
                    .expect("registry settings");
            });
        });
        cx.run_until_parked();
        let (service, _, _, _) = harness(cx);
        service.update(cx, |service, _| service._project = Some(project));
        let task = service.update(cx, |service, cx| service.recheck_inventory("tick", cx));
        task.await;
        service.read_with(cx, |service, _| {
            assert!(
                service
                    .core
                    .snapshot
                    .accounts
                    .values()
                    .any(|account| account.scope.provider == ProviderId::Codex
                        && account.availability == Availability::NotInstalled)
            )
        });
        filesystem.insert_tree(paths::external_agents_dir().join("registry/npx/codex-acp/node_modules"), serde_json::json!({ "codex-acp": { "package.json": r#"{"version":"1.0.0","bin":"cli.js"}"#, "cli.js": "" } })).await;
        let task = service.update(cx, |service, cx| service.recheck_inventory("tick", cx));
        task.await;
        service.read_with(cx, |service, _| {
            assert!(
                service
                    .core
                    .snapshot
                    .accounts
                    .values()
                    .any(|account| account.scope.provider == ProviderId::Codex
                        && account.availability == Availability::Ready)
            )
        });
    }

    #[gpui::test]
    async fn manual_refresh_skips_missing_agents_and_isolates_provider_failure(
        cx: &mut TestAppContext,
    ) {
        let (service, _, source, _) = harness(cx);
        source.fail.set(true);
        service.update(cx, |service, _| {
            for (id, availability) in [
                ("missing", Availability::NotInstalled),
                ("logged-out", Availability::NotLoggedIn),
            ] {
                service
                    .core
                    .inventory(
                        AccountScope {
                            id: id.into(),
                            provider: ProviderId::Claude,
                            runtime: "local".into(),
                            identity_known: false,
                        },
                        1,
                        availability,
                    )
                    .expect("inventory");
                service.scheduler.track(id.into(), ProviderId::Claude, 1);
            }
        });
        let result = service.update(cx, |service, cx| service.refresh(Selector::default(), cx));
        assert_eq!(result.await, Ok(()));
        assert_eq!(source.calls.get(), 1);
        service.read_with(cx, |service, _| {
            assert_eq!(service.core.snapshot.accounts.len(), 3);
            assert_eq!(
                service
                    .core
                    .snapshot
                    .accounts
                    .get("missing")
                    .expect("missing")
                    .availability,
                Availability::NotInstalled
            );
        });
    }

    #[gpui::test]
    async fn concurrent_waiters_share_read_and_failure_retains_last_good(cx: &mut TestAppContext) {
        let (service, clock, source, _) = harness(cx);
        let first = service.update(cx, |service, cx| service.ensure_fresh(request(), cx));
        let second = service.update(cx, |service, cx| service.ensure_fresh(request(), cx));
        cx.run_until_parked();
        assert_eq!(source.calls.get(), 1);
        cx.executor().advance_clock(Duration::from_secs(1));
        assert!(first.await.is_ok());
        assert!(second.await.is_ok());
        source.fail.set(true);
        clock.wall.set(1120);
        clock.elapsed.set(120);
        let next = service.update(cx, |service, cx| service.ensure_fresh(request(), cx));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        assert!(matches!(
            next.await,
            Err(RefreshError::Failed(ErrorCode::Transport))
        ));
        service.read_with(cx, |service, _| {
            let snapshot = service.snapshot(&Selector::default(), 1121);
            let sample = snapshot
                .accounts
                .get("scope")
                .and_then(|account| account.windows.values().next())
                .and_then(|window| window.used_fraction.as_ref());
            assert_eq!(
                sample.map(|sample| (sample.value, sample.observed_at)),
                Some((Some(0.6), 1000))
            );
        });
    }

    #[gpui::test]
    fn demand_drop_and_expiry_restore_idle_cadence(cx: &mut TestAppContext) {
        let (service, clock, _, _) = harness(cx);
        let lease = service.update(cx, |service, cx| service.acquire_demand(request(), cx));
        service.read_with(cx, |service, _| {
            assert_eq!(
                service
                    .scheduler
                    .scopes
                    .get("scope")
                    .map(|state| Scheduler::interval(state, 1000)),
                Some(120)
            );
        });
        drop(lease);
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert_eq!(
                service
                    .scheduler
                    .scopes
                    .get("scope")
                    .map(|state| Scheduler::interval(state, 1000)),
                Some(900)
            );
        });
        let _lease = service.update(cx, |service, cx| service.acquire_demand(request(), cx));
        clock.wall.set(1400);
        clock.elapsed.set(400);
        service.update(cx, |service, cx| service.evaluate(cx));
        service.read_with(cx, |service, _| {
            assert!(service.demands.is_empty());
        });
    }
    #[gpui::test]
    async fn failed_attempt_reservation_prevents_source_launch(cx: &mut TestAppContext) {
        let (service, _, source, store) = harness(cx);
        store.fail.set(true);
        let read = service.update(cx, |service, cx| service.ensure_fresh(request(), cx));
        assert!(matches!(
            read.await,
            Err(RefreshError::Failed(ErrorCode::Persistence))
        ));
        assert_eq!(source.calls.get(), 0);
    }

    #[gpui::test]
    async fn auth_change_cancels_waiters_and_drops_old_numbers(cx: &mut TestAppContext) {
        let (service, _, source, _) = harness(cx);
        let read = service.update(cx, |service, cx| service.ensure_fresh(request(), cx));
        cx.run_until_parked();
        assert_eq!(source.calls.get(), 1);
        service.update(cx, |service, cx| service.invalidate_auth(cx));
        assert!(matches!(read.await, Err(RefreshError::Cancelled)));
        service.read_with(cx, |service, _| {
            assert!(
                service
                    .core
                    .snapshot
                    .accounts
                    .values()
                    .all(|account| account.generation == 2 && account.windows.is_empty())
            );
        });
    }

    #[gpui::test]
    async fn not_installed_never_reaches_source_or_node_resolution(cx: &mut TestAppContext) {
        let (service, _, source, _) = harness(cx);
        service.update(cx, |service, _| {
            if let Some(account) = service.core.snapshot.accounts.get_mut("scope") {
                account.availability = Availability::NotInstalled;
            }
        });
        let read = service.update(cx, |service, cx| service.ensure_fresh(request(), cx));
        assert!(matches!(read.await, Err(RefreshError::NotInstalled)));
        cx.run_until_parked();
        assert_eq!(source.calls.get(), 0);
    }
    #[gpui::test]
    fn passive_recovery_emits_availability_transition(cx: &mut TestAppContext) {
        let (service, _, _, _) = harness(cx);
        service.update(cx, |service, _| {
            if let Some(account) = service.core.snapshot.accounts.get_mut("scope") {
                account.availability = Availability::Error(ErrorCode::Transport);
            }
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let events = events.clone();
            |cx| {
                cx.subscribe(&service, move |_, event: &QuotaEvent, _| {
                    events.borrow_mut().push(event.clone())
                })
            }
        });
        service.update(cx, |service, cx| {
            assert!(
                service
                    .observe(
                        Observation {
                            scope: "scope".into(),
                            generation: 1,
                            source: SourceKind::CodexAppServerRead,
                            source_version: "2.5.0".into(),
                            observed_at: 1000,
                            provider_sample_at: None,
                            sequence: 1,
                            start_sequence: None,
                            windows: Vec::new(),
                            complete: false,
                            metadata: AccountMetadata {
                                ordinary_usage_allowed: Some(true),
                                ..Default::default()
                            }
                        },
                        cx
                    )
                    .is_ok()
            );
        });
        cx.run_until_parked();
        assert!(
            events
                .borrow()
                .contains(&QuotaEvent::AvailabilityChanged("scope".into()))
        );
    }

    #[gpui::test]
    async fn auth_invalidation_emits_changed_with_removed_windows(cx: &mut TestAppContext) {
        let (service, _, _, _) = harness(cx);
        let read = service.update(cx, |service, cx| service.ensure_fresh(request(), cx));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        assert!(read.await.is_ok());
        let removed = service.read_with(cx, |service, _| {
            service
                .core
                .snapshot
                .accounts
                .get("scope")
                .map(|account| account.windows.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        });
        assert_eq!(removed.len(), 1);
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let events = events.clone();
            |cx| {
                cx.subscribe(&service, move |_, event: &QuotaEvent, _| {
                    events.borrow_mut().push(event.clone())
                })
            }
        });
        service.update(cx, |service, cx| service.invalidate_auth(cx));
        cx.run_until_parked();
        assert!(events.borrow().iter().any(|event| matches!(event, QuotaEvent::Changed { scopes, windows, .. } if scopes == &["scope".to_string()] && windows == &removed)));
    }
    #[gpui::test]
    fn unsupported_platform_is_explicit_and_suspends_reads(cx: &mut TestAppContext) {
        let (service, _, source, _) = harness(cx);
        service.update(cx, |service, cx| {
            assert!(matches!(
                service.complete("scope", 1, Err(RefreshError::UnsupportedPlatform), cx),
                Err(RefreshError::UnsupportedPlatform)
            ));
            service.evaluate(cx);
        });
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert!(
                service
                    .core
                    .snapshot
                    .accounts
                    .get("scope")
                    .is_some_and(|account| account.availability
                        == Availability::Unsupported(ErrorCode::UnsupportedPlatform)
                        && account.refresh
                            == RefreshState::Suspended(ErrorCode::UnsupportedPlatform))
            );
        });
        assert_eq!(source.calls.get(), 0);
    }
    #[gpui::test]
    async fn claude_inference_failure_latches_after_fake_http_response(cx: &mut TestAppContext) {
        let (service, _, source, _) = harness(cx);
        let client = http_client::FakeHttpClient::create(|_| async {
            Ok(http_client::Response::builder()
                .status(403)
                .body(http_client::AsyncBody::empty())?)
        });
        let failure = claude::probe(client, "fake-token".into(), "scope", 1, 0)
            .await
            .map(|(observation, _)| observation);
        service.update(cx, |service, cx| {
            service
                .core
                .snapshot
                .accounts
                .get_mut("scope")
                .expect("scope")
                .scope
                .provider = ProviderId::Claude;
            service
                .scheduler
                .track("scope".into(), ProviderId::Claude, 1);
            assert_eq!(
                service.complete("scope", 1, failure, cx),
                Err(RefreshError::Failed(ErrorCode::InferenceAccess))
            );
            service.scheduler.force("scope");
            service.evaluate(cx);
            assert!(service.scheduler.scopes["scope"].suspended);
        });
        cx.run_until_parked();
        assert_eq!(source.calls.get(), 0);
    }
    #[gpui::test]
    fn codex_auth_change_preserves_claude_generation_and_latch(cx: &mut TestAppContext) {
        let (service, _, _, _) = harness(cx);
        service.update(cx, |service, cx| {
            service
                .core
                .inventory(
                    AccountScope {
                        id: "claude-test".into(),
                        provider: ProviderId::Claude,
                        runtime: "local".into(),
                        identity_known: false,
                    },
                    7,
                    Availability::Unavailable(ErrorCode::InferenceAccess),
                )
                .expect("inventory");
            service
                .scheduler
                .track("claude-test".into(), ProviderId::Claude, 7);
            service
                .scheduler
                .scopes
                .get_mut("claude-test")
                .expect("schedule")
                .suspended = true;
            service.invalidate_auth(cx);
            assert_eq!(service.core.snapshot.accounts["claude-test"].generation, 7);
            assert_eq!(
                service.core.snapshot.accounts["claude-test"].availability,
                Availability::Unavailable(ErrorCode::InferenceAccess)
            );
            assert!(service.scheduler.scopes["claude-test"].suspended);
        });
    }
}

fn installation_code(availability: &Availability) -> &'static str {
    match availability {
        Availability::NotInstalled => "not_installed",
        Availability::Unsupported(_) => "unsupported",
        Availability::Error(_) => "error",
        _ => "installed",
    }
}
