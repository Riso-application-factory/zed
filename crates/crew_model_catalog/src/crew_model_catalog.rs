mod extract;
mod upstream;

use std::{fmt, rc::Rc, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use futures::{FutureExt as _, future::Shared};
use gpui::{App, Context, Entity, EventEmitter, Global, SharedString, Task, TaskExt as _};
use serde::{Deserialize, Serialize};
use smol::lock::Semaphore;

pub use extract::{
    ExtractError, OfferedOption, OfferedSelect, OfferedValue, RawBudget, check_raw_budget,
    extract_models,
};
pub use upstream::init;

pub const SOFT_TTL: Duration = Duration::from_secs(6 * 60 * 60);
pub const HARD_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_CONCURRENT_PROBES: usize = 2;
const STARTUP_DELAY: Duration = Duration::from_secs(10);
const SCHEDULER_INTERVAL: Duration = Duration::from_secs(5 * 60);
const FAILURE_BACKOFF_BASE: Duration = Duration::from_secs(5 * 60);
const ENSURE_FRESH_COOLDOWN: Duration = Duration::from_secs(60);
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentKey(pub SharedString);

impl fmt::Display for AgentKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}", self.0.as_ref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub model_id: SharedString,
    pub name: SharedString,
    pub description: Option<SharedString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installation {
    /// Installed outside of Zed, e.g. a custom command; always eligible for probing.
    Unmanaged,
    NotInstalled,
    /// `version` is `None` when only a different version is installed, so the next
    /// connection will update the agent.
    Installed {
        version: Option<SharedString>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDescriptor {
    pub key: AgentKey,
    pub installation: Installation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Timeout,
    ConnectionFailed,
    SessionNewFailed,
    NoModelOption,
    RejectedLimits,
    StoreFailed,
}

impl From<ExtractError> for FailureKind {
    fn from(error: ExtractError) -> Self {
        match error {
            ExtractError::NoModelOption => FailureKind::NoModelOption,
            ExtractError::CountLimit
            | ExtractError::SizeLimit
            | ExtractError::InvalidCharacters => FailureKind::RejectedLimits,
        }
    }
}

pub enum ProbeOutcome {
    Models {
        options: Vec<OfferedOption>,
        agent_version: Option<SharedString>,
        installation: Installation,
    },
    NotInstalled,
    NotLoggedIn,
    Failed(FailureKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale { age: Duration },
    Expired(ExpiredReason),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiredReason {
    TooOld,
    AgentVersionChanged,
    NotLoggedIn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentAvailability {
    Ready,
    Loading,
    NotConfigured,
    NotInstalled,
    NotLoggedIn,
    /// The latest refresh failed; previously published models are still reported with their
    /// own freshness.
    Failing(FailureKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentModels {
    pub agent: AgentKey,
    pub models: Arc<[ModelEntry]>,
    pub freshness: Freshness,
    pub fetched_at: Option<DateTime<Utc>>,
    pub agent_version: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCheck {
    /// Published by the agent in a fresh catalog.
    Available,
    /// Not published by the agent according to a fresh catalog.
    Missing,
    /// The catalog is not fresh enough to decide; call [`ModelCatalog::ensure_fresh`].
    NeedsRefresh(Freshness),
    AgentUnavailable(AgentAvailability),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshError {
    NotConfigured,
    NotInstalled,
    NotLoggedIn,
    Failed(FailureKind),
    /// The previous refresh failed moments ago; retrying immediately would only repeat it.
    CoolingDown,
    Cancelled,
}

impl fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefreshError::NotConfigured => write!(formatter, "agent is not configured"),
            RefreshError::NotInstalled => write!(formatter, "agent is not installed"),
            RefreshError::NotLoggedIn => write!(formatter, "agent is not logged in"),
            RefreshError::Failed(kind) => write!(formatter, "model refresh failed: {kind:?}"),
            RefreshError::CoolingDown => write!(formatter, "model refresh failed recently"),
            RefreshError::Cancelled => write!(formatter, "model refresh was cancelled"),
        }
    }
}

impl std::error::Error for RefreshError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogEvent {
    Changed(AgentKey),
    AvailabilityChanged(AgentKey),
}

pub trait AgentInventory {
    fn agents(&self, cx: &mut App) -> Task<Result<Vec<AgentDescriptor>>>;
}

pub trait ModelProbe {
    fn probe(&self, agent: &AgentKey, cx: &mut App) -> Task<ProbeOutcome>;
}

pub trait CatalogStore {
    fn load(&self, agent: &AgentKey, cx: &App) -> Task<Result<Option<String>>>;
    fn save(&self, agent: &AgentKey, value: String, cx: &App) -> Task<Result<()>>;
}

pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub struct Ports {
    pub inventory: Rc<dyn AgentInventory>,
    pub probe: Rc<dyn ModelProbe>,
    pub store: Rc<dyn CatalogStore>,
    pub clock: Rc<dyn Clock>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StoredRecord {
    schema_version: u32,
    #[serde(default)]
    models: Vec<StoredModel>,
    #[serde(default)]
    fetched_at: Option<DateTime<Utc>>,
    #[serde(default)]
    installed_version: Option<String>,
    #[serde(default)]
    agent_version: Option<String>,
    #[serde(default)]
    source: Option<Source>,
    #[serde(default)]
    last_attempt: Option<StoredAttempt>,
    #[serde(default)]
    consecutive_failures: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredModel {
    model_id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Probe,
    LiveThread,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredAttempt {
    at: DateTime<Utc>,
    outcome: AttemptOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AttemptOutcome {
    Ok,
    NotLoggedIn,
    Failed(FailureKind),
}

impl StoredRecord {
    fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            models: Vec::new(),
            fetched_at: None,
            installed_version: None,
            agent_version: None,
            source: None,
            last_attempt: None,
            consecutive_failures: 0,
        }
    }

    fn record_failure(&mut self, outcome: AttemptOutcome, now: DateTime<Utc>) {
        self.last_attempt = Some(StoredAttempt { at: now, outcome });
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    fn record_models(
        &mut self,
        models: &[ModelEntry],
        source: Source,
        installation: &Installation,
        agent_version: Option<SharedString>,
        now: DateTime<Utc>,
    ) {
        self.models = models.iter().map(StoredModel::from).collect();
        self.fetched_at = Some(now);
        self.installed_version = installed_version(installation);
        self.agent_version = agent_version.map(|version| version.to_string());
        self.source = Some(source);
        self.last_attempt = Some(StoredAttempt {
            at: now,
            outcome: AttemptOutcome::Ok,
        });
        self.consecutive_failures = 0;
    }

    fn backoff_until(&self) -> Option<DateTime<Utc>> {
        if self.consecutive_failures == 0 {
            return None;
        }
        let attempt = self.last_attempt.as_ref()?;
        let exponent = self.consecutive_failures.saturating_sub(1).min(16);
        let delay = FAILURE_BACKOFF_BASE
            .saturating_mul(1 << exponent)
            .min(HARD_TTL);
        attempt
            .at
            .checked_add_signed(chrono::Duration::from_std(delay).ok()?)
    }
}

impl From<&ModelEntry> for StoredModel {
    fn from(model: &ModelEntry) -> Self {
        Self {
            model_id: model.model_id.to_string(),
            name: model.name.to_string(),
            description: model
                .description
                .as_ref()
                .map(|description| description.to_string()),
        }
    }
}

fn installed_version(installation: &Installation) -> Option<String> {
    match installation {
        Installation::Installed {
            version: Some(version),
        } => Some(version.to_string()),
        Installation::Installed { version: None }
        | Installation::Unmanaged
        | Installation::NotInstalled => None,
    }
}

/// Decodes a stored record, validating its models with the same rules as data from agents,
/// since the database can be edited by other processes.
fn parse_record(json: &str, now: DateTime<Utc>) -> Result<(StoredRecord, Arc<[ModelEntry]>)> {
    let record: StoredRecord = serde_json::from_str(json).context("decoding catalog record")?;
    anyhow::ensure!(
        record.schema_version == SCHEMA_VERSION,
        "unsupported catalog schema version"
    );
    anyhow::ensure!(
        valid_record(&record, now),
        "invalid catalog timestamps or empty models"
    );
    if record.models.is_empty() {
        return Ok((record, Arc::from(Vec::new())));
    }
    let offered = OfferedOption {
        is_model_category: true,
        select: Some(OfferedSelect::Ungrouped(
            record
                .models
                .iter()
                .map(|model| OfferedValue {
                    value: model.model_id.clone(),
                    name: model.name.clone(),
                    description: model.description.clone(),
                })
                .collect(),
        )),
    };
    let models = extract_models(&[offered])
        .map_err(|error| anyhow::anyhow!("invalid stored models: {error:?}"))?;
    anyhow::ensure!(
        models.len() == record.models.len(),
        "stored models contain duplicates"
    );
    Ok((record, Arc::from(models)))
}

fn valid_record(record: &StoredRecord, now: DateTime<Utc>) -> bool {
    let future_limit = now
        .checked_add_signed(chrono::Duration::minutes(5))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    !(record.fetched_at.is_some() && record.models.is_empty())
        && record.fetched_at.is_none_or(|at| at <= future_limit)
        && record
            .last_attempt
            .as_ref()
            .is_none_or(|attempt| attempt.at <= future_limit)
}

fn freshness(record: &StoredRecord, installation: &Installation, now: DateTime<Utc>) -> Freshness {
    if !valid_record(record, now) {
        return Freshness::Unknown;
    }
    if let Some(attempt) = &record.last_attempt
        && attempt.outcome == AttemptOutcome::NotLoggedIn
        && record
            .fetched_at
            .is_none_or(|fetched_at| attempt.at >= fetched_at)
    {
        return Freshness::Expired(ExpiredReason::NotLoggedIn);
    }
    let Some(fetched_at) = record.fetched_at else {
        return Freshness::Unknown;
    };
    match installation {
        Installation::Installed { version: None } => {
            return Freshness::Expired(ExpiredReason::AgentVersionChanged);
        }
        Installation::Installed {
            version: Some(version),
        } if record.installed_version.as_deref() != Some(version.as_ref()) => {
            return Freshness::Expired(ExpiredReason::AgentVersionChanged);
        }
        Installation::Installed { .. } | Installation::Unmanaged | Installation::NotInstalled => {}
    }
    // A clock that moved backwards makes the record look newer, never older than it is.
    let age = (now - fetched_at).to_std().unwrap_or_default();
    if age > HARD_TTL {
        Freshness::Expired(ExpiredReason::TooOld)
    } else if age > SOFT_TTL {
        Freshness::Stale { age }
    } else {
        Freshness::Fresh
    }
}

struct LoadedRecord {
    stored: StoredRecord,
    models: Arc<[ModelEntry]>,
}

enum RecordState {
    Loading,
    Loaded(Option<LoadedRecord>),
}

type RefreshTask = Shared<Task<Result<AgentModels, RefreshError>>>;

struct AgentState {
    installation: Installation,
    installation_generation: u64,
    observation_sequence: u64,
    commits: Arc<Semaphore>,
    record: RecordState,
    refresh: Option<RefreshTask>,
}

struct GlobalModelCatalog(Entity<ModelCatalog>);

impl Global for GlobalModelCatalog {}

/// App-wide catalog of the models each local External Agent publishes.
///
/// Reads never run an agent. Answers carry their freshness, and [`ModelCatalog::check_model`]
/// only confirms a model from fresh data, so stale data cannot silently drive a decision.
pub struct ModelCatalog {
    ports: Ports,
    agents: HashMap<AgentKey, AgentState>,
    pending_live: HashMap<AgentKey, (Vec<OfferedOption>, Option<SharedString>)>,
    probe_permits: Arc<Semaphore>,
    inventory_task: Option<Task<()>>,
    _scheduler: Task<()>,
}

impl EventEmitter<CatalogEvent> for ModelCatalog {}

impl ModelCatalog {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalModelCatalog>()
            .map(|global| global.0.clone())
    }

    fn set_global(catalog: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalModelCatalog(catalog));
    }

    pub fn new(ports: Ports, cx: &mut Context<Self>) -> Self {
        let scheduler = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STARTUP_DELAY).await;
            loop {
                if this.update(cx, |this, cx| this.sync_inventory(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(SCHEDULER_INTERVAL).await;
            }
        });
        Self {
            ports,
            agents: HashMap::default(),
            pending_live: HashMap::default(),
            probe_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES)),
            inventory_task: None,
            _scheduler: scheduler,
        }
    }

    pub fn agents(&self) -> Vec<(AgentKey, AgentAvailability)> {
        let mut agents = self
            .agents
            .keys()
            .map(|key| (key.clone(), self.availability(key)))
            .collect::<Vec<_>>();
        agents.sort_by(|(left, _), (right, _)| left.cmp(right));
        agents
    }

    pub fn availability(&self, agent: &AgentKey) -> AgentAvailability {
        let Some(state) = self.agents.get(agent) else {
            return AgentAvailability::NotConfigured;
        };
        if state.installation == Installation::NotInstalled {
            return AgentAvailability::NotInstalled;
        }
        let RecordState::Loaded(record) = &state.record else {
            return AgentAvailability::Loading;
        };
        let Some(record) = record else {
            return AgentAvailability::Ready;
        };
        match record
            .stored
            .last_attempt
            .as_ref()
            .map(|attempt| attempt.outcome)
        {
            Some(AttemptOutcome::NotLoggedIn) if record.stored.consecutive_failures > 0 => {
                AgentAvailability::NotLoggedIn
            }
            Some(AttemptOutcome::Failed(kind)) if record.stored.consecutive_failures > 0 => {
                AgentAvailability::Failing(kind)
            }
            _ => AgentAvailability::Ready,
        }
    }

    pub fn models(&self, agent: &AgentKey) -> Option<AgentModels> {
        let state = self.agents.get(agent)?;
        let RecordState::Loaded(Some(record)) = &state.record else {
            return None;
        };
        Some(AgentModels {
            agent: agent.clone(),
            models: record.models.clone(),
            freshness: freshness(&record.stored, &state.installation, self.ports.clock.now()),
            fetched_at: record.stored.fetched_at,
            agent_version: record.stored.agent_version.clone().map(SharedString::from),
        })
    }

    pub fn check_model(&self, agent: &AgentKey, model_id: &str) -> ModelCheck {
        let availability = self.availability(agent);
        match availability {
            AgentAvailability::NotConfigured
            | AgentAvailability::NotInstalled
            | AgentAvailability::NotLoggedIn
            | AgentAvailability::Loading => return ModelCheck::AgentUnavailable(availability),
            AgentAvailability::Ready | AgentAvailability::Failing(_) => {}
        }
        let Some(models) = self.models(agent) else {
            return ModelCheck::NeedsRefresh(Freshness::Unknown);
        };
        if models.freshness != Freshness::Fresh {
            return ModelCheck::NeedsRefresh(models.freshness);
        }
        if models
            .models
            .iter()
            .any(|model| model.model_id.as_ref() == model_id)
        {
            ModelCheck::Available
        } else {
            ModelCheck::Missing
        }
    }

    /// Returns fresh models, probing the agent first when the catalog is not fresh.
    /// Concurrent callers share one probe.
    pub fn ensure_fresh(
        &mut self,
        agent: &AgentKey,
        cx: &mut Context<Self>,
    ) -> Task<Result<AgentModels, RefreshError>> {
        let Some(state) = self.agents.get(agent) else {
            return Task::ready(Err(RefreshError::NotConfigured));
        };
        if state.installation == Installation::NotInstalled {
            return Task::ready(Err(RefreshError::NotInstalled));
        }
        if let Some(models) = self.models(agent)
            && models.freshness == Freshness::Fresh
        {
            return Task::ready(Ok(models));
        }
        if state.refresh.is_none() {
            if let RecordState::Loaded(Some(record)) = &state.record
                && record.stored.consecutive_failures > 0
                && let Some(attempt) = &record.stored.last_attempt
                && self.ports.clock.now() - attempt.at
                    < chrono::Duration::from_std(ENSURE_FRESH_COOLDOWN)
                        .unwrap_or(chrono::Duration::MAX)
            {
                return Task::ready(Err(RefreshError::CoolingDown));
            }
            self.start_refresh(agent.clone(), cx);
        }
        let Some(refresh) = self
            .agents
            .get(agent)
            .and_then(|state| state.refresh.clone())
        else {
            return Task::ready(Err(RefreshError::NotConfigured));
        };
        cx.spawn(async move |_, _| refresh.await)
    }

    /// Re-reads the agent inventory, loads records of new agents and schedules due refreshes.
    pub fn sync_inventory(&mut self, cx: &mut Context<Self>) {
        let agents = self.ports.inventory.agents(cx);
        self.inventory_task = Some(cx.spawn(async move |this, cx| {
            let descriptors = match agents.await {
                Ok(descriptors) => descriptors,
                Err(_) => {
                    log::warn!("model_catalog: inventory_failed");
                    return;
                }
            };
            this.update(cx, |this, cx| this.apply_inventory(descriptors, cx))
                .ok();
        }));
    }

    /// Accepts models a running thread reported, which costs no probe.
    pub fn observe_live(
        &mut self,
        agent: AgentKey,
        options: Vec<OfferedOption>,
        agent_version: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.agents.get(&agent) else {
            self.pending_live.insert(agent, (options, agent_version));
            return;
        };
        if matches!(state.record, RecordState::Loading) {
            self.pending_live.insert(agent, (options, agent_version));
            return;
        }
        let commits = state.commits.clone();
        cx.spawn(async move |this, cx| {
            let _guard = commits.acquire_arc().await;
            let commit = this
                .update(cx, |this, cx| {
                    this.apply_live(agent, options, agent_version, cx)
                })
                .map_err(|_| RefreshError::Cancelled)?;
            commit.await
        })
        .detach_and_log_err(cx);
    }

    fn apply_live(
        &mut self,
        agent: AgentKey,
        options: Vec<OfferedOption>,
        agent_version: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Task<Result<AgentModels, RefreshError>> {
        let Some(state) = self.agents.get_mut(&agent) else {
            return Task::ready(Err(RefreshError::NotConfigured));
        };
        let models = match extract_models(&options) {
            Ok(models) => models,
            Err(error) => return Task::ready(Err(RefreshError::Failed(error.into()))),
        };
        let now = self.ports.clock.now();
        let half_soft_ttl =
            chrono::Duration::from_std(SOFT_TTL / 2).unwrap_or(chrono::Duration::MAX);
        if let RecordState::Loaded(Some(current)) = &state.record
            && current.models.as_ref() == models.as_slice()
            && current.stored.agent_version.as_deref() == agent_version.as_deref()
            && current.stored.consecutive_failures == 0
            && current
                .stored
                .fetched_at
                .is_some_and(|fetched_at| now - fetched_at < half_soft_ttl)
        {
            // Even an unchanged live list supersedes probes that started before it arrived.
            state.observation_sequence += 1;
            return Task::ready(self.models(&agent).ok_or(RefreshError::NotConfigured));
        }
        let mut record = match &state.record {
            RecordState::Loaded(Some(current)) => current.stored.clone(),
            _ => StoredRecord::empty(),
        };
        record.record_models(
            &models,
            Source::LiveThread,
            &state.installation,
            agent_version,
            now,
        );
        self.commit(agent, record, None, Ok(()), cx)
    }

    fn apply_inventory(&mut self, descriptors: Vec<AgentDescriptor>, cx: &mut Context<Self>) {
        let mut configured = HashSet::default();
        for descriptor in descriptors {
            configured.insert(descriptor.key.clone());
            match self.agents.get_mut(&descriptor.key) {
                Some(state) => {
                    if state.installation != descriptor.installation {
                        state.installation_generation += 1;
                        state.installation = descriptor.installation;
                        cx.emit(CatalogEvent::AvailabilityChanged(descriptor.key));
                    }
                }
                None => {
                    self.agents.insert(
                        descriptor.key.clone(),
                        AgentState {
                            installation: descriptor.installation,
                            installation_generation: 0,
                            observation_sequence: 0,
                            commits: Arc::new(Semaphore::new(1)),
                            record: RecordState::Loading,
                            refresh: None,
                        },
                    );
                    self.load(descriptor.key.clone(), cx);
                    cx.emit(CatalogEvent::AvailabilityChanged(descriptor.key));
                }
            }
        }
        let removed = self
            .agents
            .keys()
            .filter(|key| !configured.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in removed {
            self.agents.remove(&key);
            cx.emit(CatalogEvent::AvailabilityChanged(key));
        }
        self.schedule_refreshes(cx);
    }

    fn load(&mut self, agent: AgentKey, cx: &mut Context<Self>) {
        let load = self.ports.store.load(&agent, cx);
        let clock = self.ports.clock.clone();
        cx.spawn(async move |this, cx| {
            let record = match load.await {
                Ok(Some(json)) => match parse_record(&json, clock.now()) {
                    Ok((stored, models)) => Some(LoadedRecord { stored, models }),
                    Err(_) => {
                        log::warn!("model_catalog: record_invalid agent={agent}");
                        None
                    }
                },
                Ok(None) => None,
                Err(_) => {
                    log::warn!("model_catalog: store_read_failed agent={agent}");
                    None
                }
            };
            this.update(cx, |this, cx| {
                // A refresh may have committed while loading; its record is newer.
                if let Some(state) = this.agents.get_mut(&agent)
                    && matches!(state.record, RecordState::Loading)
                {
                    state.record = RecordState::Loaded(record);
                    cx.emit(CatalogEvent::Changed(agent.clone()));
                    if let Some((options, version)) = this.pending_live.remove(&agent) {
                        this.observe_live(agent, options, version, cx);
                    }
                    this.schedule_refreshes(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn schedule_refreshes(&mut self, cx: &mut Context<Self>) {
        let now = self.ports.clock.now();
        let due = self
            .agents
            .iter()
            .filter(|(_, state)| {
                if state.refresh.is_some() || state.installation == Installation::NotInstalled {
                    return false;
                }
                let RecordState::Loaded(record) = &state.record else {
                    return false;
                };
                let Some(record) = record else {
                    return true;
                };
                freshness(&record.stored, &state.installation, now) != Freshness::Fresh
                    && record
                        .stored
                        .backoff_until()
                        .is_none_or(|backoff_until| now >= backoff_until)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for agent in due {
            self.start_refresh(agent, cx);
        }
    }

    /// Starts a probe unless one is already running; the task is kept in the agent's state so
    /// every caller awaits the same run.
    fn start_refresh(&mut self, agent: AgentKey, cx: &mut Context<Self>) {
        let Some(state) = self.agents.get(&agent) else {
            return;
        };
        if state.refresh.is_some() {
            return;
        }
        let commits = state.commits.clone();
        let permits = self.probe_permits.clone();
        let probe = self.ports.probe.clone();
        let refresh = cx
            .spawn({
                let agent = agent.clone();
                async move |this, cx| {
                    let permit = permits.acquire_arc().await;
                    let (installation_generation, observation_sequence) = this
                        .read_with(cx, |this, _| {
                            this.agents.get(&agent).map(|state| {
                                (state.installation_generation, state.observation_sequence)
                            })
                        })
                        .map_err(|_| RefreshError::Cancelled)?
                        .ok_or(RefreshError::NotConfigured)?;
                    let outcome = cx.update(|cx| probe.probe(&agent, cx)).await;
                    drop(permit);
                    let _guard = commits.acquire_arc().await;
                    let commit = this
                        .update(cx, |this, cx| {
                            this.apply_probe_outcome(
                                &agent,
                                outcome,
                                installation_generation,
                                observation_sequence,
                                cx,
                            )
                        })
                        .map_err(|_| RefreshError::Cancelled)?;
                    let result = commit.await;
                    this.update(cx, |this, _| {
                        if let Some(state) = this.agents.get_mut(&agent) {
                            state.refresh = None;
                        }
                    })
                    .ok();
                    result
                }
            })
            .shared();
        if let Some(state) = self.agents.get_mut(&agent) {
            state.refresh = Some(refresh);
        }
    }

    fn apply_probe_outcome(
        &mut self,
        agent: &AgentKey,
        outcome: ProbeOutcome,
        installation_generation: u64,
        observation_sequence: u64,
        cx: &mut Context<Self>,
    ) -> Task<Result<AgentModels, RefreshError>> {
        let Some(state) = self.agents.get(agent) else {
            return Task::ready(Err(RefreshError::NotConfigured));
        };
        if state.installation_generation != installation_generation {
            return Task::ready(Err(RefreshError::Cancelled));
        }
        if state.observation_sequence != observation_sequence {
            return Task::ready(self.models(agent).ok_or(RefreshError::Cancelled));
        }
        let now = self.ports.clock.now();
        let mut record = match &state.record {
            RecordState::Loaded(Some(record)) => record.stored.clone(),
            RecordState::Loaded(None) | RecordState::Loading => StoredRecord::empty(),
        };
        match outcome {
            ProbeOutcome::Models {
                options,
                agent_version,
                installation,
            } => match extract_models(&options) {
                Ok(models) => {
                    record.record_models(&models, Source::Probe, &installation, agent_version, now);
                    self.commit(agent.clone(), record, Some(installation), Ok(()), cx)
                }
                Err(error) => {
                    let kind = FailureKind::from(error);
                    record.record_failure(AttemptOutcome::Failed(kind), now);
                    self.commit(
                        agent.clone(),
                        record,
                        Some(installation),
                        Err(RefreshError::Failed(kind)),
                        cx,
                    )
                }
            },
            ProbeOutcome::NotInstalled => {
                if let Some(state) = self.agents.get_mut(agent) {
                    state.installation = Installation::NotInstalled;
                    cx.emit(CatalogEvent::AvailabilityChanged(agent.clone()));
                }
                Task::ready(Err(RefreshError::NotInstalled))
            }
            ProbeOutcome::NotLoggedIn => {
                record.record_failure(AttemptOutcome::NotLoggedIn, now);
                self.commit(
                    agent.clone(),
                    record,
                    None,
                    Err(RefreshError::NotLoggedIn),
                    cx,
                )
            }
            ProbeOutcome::Failed(kind) => {
                record.record_failure(AttemptOutcome::Failed(kind), now);
                self.commit(
                    agent.clone(),
                    record,
                    None,
                    Err(RefreshError::Failed(kind)),
                    cx,
                )
            }
        }
    }

    /// Persists the record and only then publishes it, so readers never see data the
    /// database does not hold.
    fn commit(
        &mut self,
        agent: AgentKey,
        record: StoredRecord,
        installation: Option<Installation>,
        result: Result<(), RefreshError>,
        cx: &mut Context<Self>,
    ) -> Task<Result<AgentModels, RefreshError>> {
        let models = record
            .models
            .iter()
            .map(|model| ModelEntry {
                model_id: SharedString::from(model.model_id.clone()),
                name: SharedString::from(model.name.clone()),
                description: model.description.clone().map(SharedString::from),
            })
            .collect::<Arc<[ModelEntry]>>();
        let json = match serde_json::to_string(&record) {
            Ok(json) => json,
            Err(_) => {
                log::error!("model_catalog: store_encode_failed agent={agent}");
                return Task::ready(Err(RefreshError::Failed(FailureKind::StoreFailed)));
            }
        };
        let installation_generation = self
            .agents
            .get(&agent)
            .map(|state| state.installation_generation);
        let save = self.ports.store.save(&agent, json, cx);
        cx.spawn(async move |this, cx| {
            if save.await.is_err() {
                log::warn!("model_catalog: store_write_failed agent={agent}");
                return Err(RefreshError::Failed(FailureKind::StoreFailed));
            }
            this.update(cx, |this, cx| {
                let previous_availability = this.availability(&agent);
                let Some(state) = this.agents.get_mut(&agent) else {
                    return Err(RefreshError::NotConfigured);
                };
                let previous_ids = match &state.record {
                    RecordState::Loaded(Some(previous)) => previous
                        .models
                        .iter()
                        .map(|model| model.model_id.clone())
                        .collect::<HashSet<_>>(),
                    RecordState::Loaded(None) | RecordState::Loading => HashSet::default(),
                };
                if Some(state.installation_generation) != installation_generation {
                    return Err(RefreshError::Cancelled);
                }
                if record.source == Some(Source::LiveThread) && result.is_ok() {
                    state.observation_sequence += 1;
                }
                if let Some(installation) = installation {
                    state.installation = installation;
                }
                if result.is_ok() {
                    let current_ids = models
                        .iter()
                        .map(|model| model.model_id.clone())
                        .collect::<HashSet<_>>();
                    log::info!(
                        "model_catalog: refreshed agent={agent} source={:?} count={} added={} removed={}",
                        record.source,
                        models.len(),
                        current_ids.difference(&previous_ids).count(),
                        previous_ids.difference(&current_ids).count(),
                    );
                } else if let Some(attempt) = &record.last_attempt {
                    log::info!(
                        "model_catalog: refresh_failed agent={agent} outcome={:?}",
                        attempt.outcome
                    );
                }
                state.record = RecordState::Loaded(Some(LoadedRecord {
                    stored: record,
                    models,
                }));
                cx.emit(CatalogEvent::Changed(agent.clone()));
                if this.availability(&agent) != previous_availability {
                    cx.emit(CatalogEvent::AvailabilityChanged(agent.clone()));
                }
                result?;
                this.models(&agent).ok_or(RefreshError::NotConfigured)
            })
            .map_err(|_| RefreshError::Cancelled)?
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
    };

    use futures::channel::oneshot;
    use gpui::{AppContext as _, TestAppContext};

    use super::*;

    pub(crate) struct FakeInventory(pub(crate) RefCell<Vec<AgentDescriptor>>);

    impl AgentInventory for FakeInventory {
        fn agents(&self, _cx: &mut App) -> Task<Result<Vec<AgentDescriptor>>> {
            Task::ready(Ok(self.0.borrow().clone()))
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeProbe {
        pub(crate) outcomes: RefCell<HashMap<AgentKey, VecDeque<ProbeOutcome>>>,
        pub(crate) calls: RefCell<Vec<AgentKey>>,
        pub(crate) gated: Cell<bool>,
        pub(crate) gates: RefCell<Vec<oneshot::Sender<ProbeOutcome>>>,
    }

    impl ModelProbe for FakeProbe {
        fn probe(&self, agent: &AgentKey, cx: &mut App) -> Task<ProbeOutcome> {
            self.calls.borrow_mut().push(agent.clone());
            if self.gated.get() {
                let (sender, receiver) = oneshot::channel();
                self.gates.borrow_mut().push(sender);
                return cx.background_spawn(async move {
                    receiver
                        .await
                        .unwrap_or(ProbeOutcome::Failed(FailureKind::Timeout))
                });
            }
            let outcome = self
                .outcomes
                .borrow_mut()
                .get_mut(agent)
                .and_then(VecDeque::pop_front)
                .unwrap_or(ProbeOutcome::Failed(FailureKind::ConnectionFailed));
            Task::ready(outcome)
        }
    }

    #[derive(Default)]
    pub(crate) struct MemoryStore {
        pub(crate) records: Rc<RefCell<HashMap<AgentKey, String>>>,
        pub(crate) save_gate: RefCell<Option<oneshot::Receiver<()>>>,
        pub(crate) fail_writes: Cell<bool>,
    }

    impl CatalogStore for MemoryStore {
        fn load(&self, agent: &AgentKey, _cx: &App) -> Task<Result<Option<String>>> {
            Task::ready(Ok(self.records.borrow().get(agent).cloned()))
        }

        fn save(&self, agent: &AgentKey, value: String, cx: &App) -> Task<Result<()>> {
            if self.fail_writes.get() {
                return Task::ready(Err(anyhow::anyhow!("write failed")));
            }
            if let Some(gate) = self.save_gate.borrow_mut().take() {
                let records = self.records.clone();
                let agent = agent.clone();
                return cx.spawn(async move |_| {
                    gate.await?;
                    records.borrow_mut().insert(agent, value);
                    Ok(())
                });
            }
            self.records.borrow_mut().insert(agent.clone(), value);
            Task::ready(Ok(()))
        }
    }

    pub(crate) struct FakeClock(pub(crate) Cell<DateTime<Utc>>);

    impl FakeClock {
        pub(crate) fn advance(&self, duration: Duration) {
            self.0
                .set(self.0.get() + chrono::Duration::from_std(duration).unwrap());
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            self.0.get()
        }
    }

    pub(crate) struct Harness {
        pub(crate) catalog: Entity<ModelCatalog>,
        pub(crate) inventory: Rc<FakeInventory>,
        pub(crate) probe: Rc<FakeProbe>,
        pub(crate) store: Rc<MemoryStore>,
        pub(crate) clock: Rc<FakeClock>,
    }

    impl Harness {
        pub(crate) fn new(descriptors: Vec<AgentDescriptor>, cx: &mut TestAppContext) -> Self {
            Self::with_store(descriptors, MemoryStore::default(), cx)
        }

        pub(crate) fn with_store(
            descriptors: Vec<AgentDescriptor>,
            store: MemoryStore,
            cx: &mut TestAppContext,
        ) -> Self {
            let inventory = Rc::new(FakeInventory(RefCell::new(descriptors)));
            let probe = Rc::new(FakeProbe::default());
            let store = Rc::new(store);
            let clock = Rc::new(FakeClock(Cell::new(
                DateTime::parse_from_rfc3339("2026-11-04T09:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )));
            let ports = Ports {
                inventory: inventory.clone(),
                probe: probe.clone(),
                store: store.clone(),
                clock: clock.clone(),
            };
            let catalog = cx.new(|cx| ModelCatalog::new(ports, cx));
            Self {
                catalog,
                inventory,
                probe,
                store,
                clock,
            }
        }

        pub(crate) fn queue(&self, agent: &str, outcome: ProbeOutcome) {
            self.probe
                .outcomes
                .borrow_mut()
                .entry(key(agent))
                .or_default()
                .push_back(outcome);
        }

        pub(crate) fn sync(&self, cx: &mut TestAppContext) {
            self.catalog
                .update(cx, |catalog, cx| catalog.sync_inventory(cx));
            cx.run_until_parked();
        }

        pub(crate) fn calls(&self) -> usize {
            self.probe.calls.borrow().len()
        }

        pub(crate) fn check(
            &self,
            agent: &str,
            model: &str,
            cx: &mut TestAppContext,
        ) -> ModelCheck {
            self.catalog
                .read_with(cx, |catalog, _| catalog.check_model(&key(agent), model))
        }

        pub(crate) fn freshness(&self, agent: &str, cx: &mut TestAppContext) -> Option<Freshness> {
            self.catalog.read_with(cx, |catalog, _| {
                catalog.models(&key(agent)).map(|models| models.freshness)
            })
        }

        pub(crate) fn availability(
            &self,
            agent: &str,
            cx: &mut TestAppContext,
        ) -> AgentAvailability {
            self.catalog
                .read_with(cx, |catalog, _| catalog.availability(&key(agent)))
        }

        pub(crate) fn ensure_fresh(
            &self,
            agent: &str,
            cx: &mut TestAppContext,
        ) -> Task<Result<AgentModels, RefreshError>> {
            self.catalog
                .update(cx, |catalog, cx| catalog.ensure_fresh(&key(agent), cx))
        }
    }

    pub(crate) fn key(agent: &str) -> AgentKey {
        AgentKey(SharedString::from(agent.to_string()))
    }

    pub(crate) fn installed(agent: &str, version: &str) -> AgentDescriptor {
        AgentDescriptor {
            key: key(agent),
            installation: Installation::Installed {
                version: Some(SharedString::from(version.to_string())),
            },
        }
    }

    pub(crate) fn offered(ids: &[&str]) -> Vec<OfferedOption> {
        vec![OfferedOption {
            is_model_category: true,
            select: Some(OfferedSelect::Ungrouped(
                ids.iter()
                    .map(|id| OfferedValue {
                        value: id.to_string(),
                        name: id.to_string(),
                        description: None,
                    })
                    .collect(),
            )),
        }]
    }

    pub(crate) fn models_outcome(ids: &[&str], version: &str) -> ProbeOutcome {
        ProbeOutcome::Models {
            options: offered(ids),
            agent_version: Some(SharedString::from(format!("acp-{version}"))),
            installation: Installation::Installed {
                version: Some(SharedString::from(version.to_string())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::test_support::*;
    use super::*;

    #[gpui::test]
    async fn installation_changes_discard_inflight_probes(cx: &mut TestAppContext) {
        for installation in [
            Installation::Installed {
                version: Some("2".into()),
            },
            Installation::NotInstalled,
        ] {
            let harness = Harness::new(vec![installed("agent", "1")], cx);
            harness.probe.gated.set(true);
            harness.sync(cx);
            *harness.inventory.0.borrow_mut() = vec![AgentDescriptor {
                key: key("agent"),
                installation: installation.clone(),
            }];
            harness.sync(cx);
            harness
                .probe
                .gates
                .borrow_mut()
                .remove(0)
                .send(models_outcome(&["old"], "1"))
                .ok();
            cx.run_until_parked();
            assert_eq!(
                harness.catalog.read_with(cx, |catalog, _| catalog
                    .agents
                    .get(&key("agent"))
                    .unwrap()
                    .installation
                    .clone()),
                installation
            );
            assert!(!harness.store.records.borrow().contains_key(&key("agent")));
            if installation != Installation::NotInstalled {
                harness.sync(cx);
                assert_eq!(harness.calls(), 2);
            }
        }
    }

    #[gpui::test]
    async fn newer_live_observations_win_over_probe_models_and_failures(cx: &mut TestAppContext) {
        for outcome in [
            models_outcome(&["old"], "1"),
            ProbeOutcome::NotLoggedIn,
            ProbeOutcome::Failed(FailureKind::Timeout),
        ] {
            let harness = Harness::new(vec![installed("agent", "1")], cx);
            harness.probe.gated.set(true);
            harness.sync(cx);
            harness.catalog.update(cx, |catalog, cx| {
                catalog.observe_live(key("agent"), offered(&["live"]), None, cx)
            });
            cx.run_until_parked();
            harness
                .probe
                .gates
                .borrow_mut()
                .remove(0)
                .send(outcome)
                .ok();
            cx.run_until_parked();
            assert_eq!(harness.check("agent", "live", cx), ModelCheck::Available);
            assert_eq!(harness.check("agent", "old", cx), ModelCheck::Missing);
            assert_eq!(harness.availability("agent", cx), AgentAvailability::Ready);
            let (stored, _) = parse_record(
                harness.store.records.borrow().get(&key("agent")).unwrap(),
                harness.clock.now(),
            )
            .unwrap();
            assert_eq!(stored.source, Some(Source::LiveThread));
        }
    }

    #[gpui::test]
    async fn commits_wait_for_the_latest_saved_live_observation(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("agent", "1")], cx);
        harness.probe.gated.set(true);
        harness.sync(cx);
        let (sender, receiver) = futures::channel::oneshot::channel();
        *harness.store.save_gate.borrow_mut() = Some(receiver);
        harness.catalog.update(cx, |catalog, cx| {
            catalog.observe_live(key("agent"), offered(&["live"]), None, cx)
        });
        cx.run_until_parked();
        harness
            .probe
            .gates
            .borrow_mut()
            .remove(0)
            .send(ProbeOutcome::NotLoggedIn)
            .ok();
        cx.run_until_parked();
        assert!(harness.store.records.borrow().is_empty());
        sender.send(()).unwrap();
        cx.run_until_parked();
        assert_eq!(harness.check("agent", "live", cx), ModelCheck::Available);
        assert_eq!(harness.availability("agent", cx), AgentAvailability::Ready);
    }

    #[gpui::test]
    async fn buffers_latest_live_observation_until_inventory_and_load(cx: &mut TestAppContext) {
        let harness = Harness::new(Vec::new(), cx);
        harness.catalog.update(cx, |catalog, cx| {
            catalog.observe_live(key("agent"), offered(&["early"]), None, cx);
            catalog.observe_live(key("agent"), offered(&["latest"]), None, cx);
        });
        *harness.inventory.0.borrow_mut() = vec![installed("agent", "1")];
        harness.probe.gated.set(true);
        harness.sync(cx);
        assert_eq!(harness.check("agent", "latest", cx), ModelCheck::Available);
        assert_eq!(harness.check("agent", "early", cx), ModelCheck::Missing);

        harness.catalog.update(cx, |catalog, cx| {
            catalog.apply_inventory(vec![installed("loading", "1")], cx);
            catalog.observe_live(key("loading"), offered(&["during-load"]), None, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            harness.check("loading", "during-load", cx),
            ModelCheck::Available
        );
    }

    #[test]
    fn backoff_handles_maximum_timestamp() {
        let mut record = StoredRecord::empty();
        record.record_failure(
            AttemptOutcome::Failed(FailureKind::Timeout),
            DateTime::<Utc>::MAX_UTC - chrono::Duration::seconds(1),
        );
        assert_eq!(record.backoff_until(), None);
    }

    #[gpui::test]
    async fn rejects_empty_success_and_future_stored_timestamps(cx: &mut TestAppContext) {
        for invalid in [
            serde_json::json!({"schema_version": 1, "fetched_at": "2026-11-04T08:00:00Z", "models": []}),
            serde_json::json!({"schema_version": 1, "fetched_at": "2026-11-04T09:05:01Z", "models": [{"model_id":"old","name":"old"}]}),
            serde_json::json!({"schema_version": 1, "last_attempt": {"at":"2026-11-04T09:05:01Z","outcome":"ok"}}),
        ] {
            let store = MemoryStore::default();
            store
                .records
                .borrow_mut()
                .insert(key("agent"), invalid.to_string());
            let harness = Harness::with_store(vec![installed("agent", "1")], store, cx);
            assert!(parse_record(&invalid.to_string(), harness.clock.now()).is_err());
            harness.queue("agent", models_outcome(&["new"], "1"));
            harness.sync(cx);
            assert_eq!(harness.calls(), 1);
            assert_eq!(harness.check("agent", "new", cx), ModelCheck::Available);
        }
    }

    #[gpui::test]
    async fn probes_installed_agents_and_serves_fresh_models(cx: &mut TestAppContext) {
        let harness = Harness::new(
            vec![
                installed("claude", "1.0.0"),
                AgentDescriptor {
                    key: key("codex"),
                    installation: Installation::NotInstalled,
                },
            ],
            cx,
        );
        harness.queue("claude", models_outcome(&["default", "opus"], "1.0.0"));

        harness.sync(cx);

        assert_eq!(*harness.probe.calls.borrow(), vec![key("claude")]);
        assert_eq!(harness.check("claude", "opus", cx), ModelCheck::Available);
        assert_eq!(harness.check("claude", "Opus", cx), ModelCheck::Missing);
        assert_eq!(
            harness.check("codex", "gpt-5", cx),
            ModelCheck::AgentUnavailable(AgentAvailability::NotInstalled)
        );
        assert_eq!(
            harness.check("gemini", "pro", cx),
            ModelCheck::AgentUnavailable(AgentAvailability::NotConfigured)
        );
        assert!(harness.store.records.borrow().contains_key(&key("claude")));
    }

    #[gpui::test]
    async fn stale_data_needs_refresh_and_is_refreshed_in_background(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("claude", "1.0.0")], cx);
        harness.queue("claude", models_outcome(&["opus"], "1.0.0"));
        harness.sync(cx);

        harness.clock.advance(SOFT_TTL + Duration::from_secs(1));
        assert!(matches!(
            harness.check("claude", "opus", cx),
            ModelCheck::NeedsRefresh(Freshness::Stale { .. })
        ));

        harness.clock.advance(HARD_TTL);
        assert_eq!(
            harness.check("claude", "opus", cx),
            ModelCheck::NeedsRefresh(Freshness::Expired(ExpiredReason::TooOld))
        );

        harness.queue("claude", models_outcome(&["opus", "sonnet"], "1.0.0"));
        harness.sync(cx);
        assert_eq!(harness.calls(), 2);
        assert_eq!(harness.check("claude", "sonnet", cx), ModelCheck::Available);
    }

    #[gpui::test]
    async fn agent_version_change_expires_data_immediately(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("claude", "1.0.0")], cx);
        harness.queue("claude", models_outcome(&["opus"], "1.0.0"));
        harness.sync(cx);

        *harness.inventory.0.borrow_mut() = vec![installed("claude", "1.1.0")];
        harness.probe.gated.set(true);
        harness.sync(cx);

        assert_eq!(
            harness.freshness("claude", cx),
            Some(Freshness::Expired(ExpiredReason::AgentVersionChanged))
        );
        assert_eq!(
            harness.check("claude", "opus", cx),
            ModelCheck::NeedsRefresh(Freshness::Expired(ExpiredReason::AgentVersionChanged))
        );
        assert_eq!(harness.calls(), 2);
    }

    #[gpui::test]
    async fn ensure_fresh_shares_one_probe(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("claude", "1.0.0")], cx);
        harness.probe.gated.set(true);
        harness.sync(cx);
        assert_eq!(harness.calls(), 1);

        let first = harness.ensure_fresh("claude", cx);
        let second = harness.ensure_fresh("claude", cx);
        cx.run_until_parked();
        assert_eq!(harness.calls(), 1);

        let gate = harness.probe.gates.borrow_mut().remove(0);
        gate.send(models_outcome(&["opus"], "1.0.0")).ok();
        cx.run_until_parked();

        for result in [first.await, second.await] {
            let models = result.unwrap();
            assert_eq!(models.freshness, Freshness::Fresh);
            assert_eq!(models.models[0].model_id.as_ref(), "opus");
        }
    }

    #[gpui::test]
    async fn failures_keep_models_and_back_off(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("claude", "1.0.0")], cx);
        harness.queue("claude", models_outcome(&["opus"], "1.0.0"));
        harness.sync(cx);

        harness.clock.advance(SOFT_TTL + Duration::from_secs(1));
        harness.queue("claude", ProbeOutcome::Failed(FailureKind::Timeout));
        let result = harness.ensure_fresh("claude", cx);
        cx.run_until_parked();
        assert_eq!(
            result.await,
            Err(RefreshError::Failed(FailureKind::Timeout))
        );

        assert_eq!(
            harness.availability("claude", cx),
            AgentAvailability::Failing(FailureKind::Timeout)
        );
        assert!(matches!(
            harness.freshness("claude", cx),
            Some(Freshness::Stale { .. })
        ));
        assert_eq!(
            harness.ensure_fresh("claude", cx).await,
            Err(RefreshError::CoolingDown)
        );
        harness.sync(cx);
        assert_eq!(harness.calls(), 2);

        harness.clock.advance(FAILURE_BACKOFF_BASE);
        harness.queue("claude", models_outcome(&["opus"], "1.0.0"));
        harness.sync(cx);
        assert_eq!(harness.calls(), 3);
        assert_eq!(harness.availability("claude", cx), AgentAvailability::Ready);
    }

    #[gpui::test]
    async fn not_logged_in_blocks_decisions_until_a_live_thread_succeeds(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("claude", "1.0.0")], cx);
        harness.queue("claude", models_outcome(&["opus"], "1.0.0"));
        harness.sync(cx);
        harness.clock.advance(SOFT_TTL + Duration::from_secs(1));
        harness.queue("claude", ProbeOutcome::NotLoggedIn);
        harness.sync(cx);

        assert_eq!(
            harness.check("claude", "opus", cx),
            ModelCheck::AgentUnavailable(AgentAvailability::NotLoggedIn)
        );
        assert_eq!(
            harness.freshness("claude", cx),
            Some(Freshness::Expired(ExpiredReason::NotLoggedIn))
        );

        harness.catalog.update(cx, |catalog, cx| {
            catalog.observe_live(
                key("claude"),
                offered(&["opus", "haiku"]),
                Some("acp-1.0.0".into()),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(harness.check("claude", "haiku", cx), ModelCheck::Available);
    }

    #[gpui::test]
    async fn limits_concurrent_probes(cx: &mut TestAppContext) {
        let harness = Harness::new(
            vec![
                installed("antigravity", "1.0.0"),
                installed("claude", "1.0.0"),
                installed("codex", "1.0.0"),
            ],
            cx,
        );
        harness.probe.gated.set(true);
        harness.sync(cx);
        assert_eq!(harness.calls(), MAX_CONCURRENT_PROBES);

        let gate = harness.probe.gates.borrow_mut().remove(0);
        gate.send(models_outcome(&["model"], "1.0.0")).ok();
        cx.run_until_parked();
        assert_eq!(harness.calls(), 3);
    }

    #[gpui::test]
    async fn restores_records_and_isolates_invalid_ones(cx: &mut TestAppContext) {
        let fresh_record = serde_json::json!({
            "schema_version": 1,
            "models": [{ "model_id": "opus", "name": "Opus" }],
            "fetched_at": "2026-11-04T08:00:00Z",
            "installed_version": "1.0.0",
            "future_field": true,
        });
        let store = MemoryStore::default();
        store
            .records
            .borrow_mut()
            .insert(key("claude"), fresh_record.to_string());
        store
            .records
            .borrow_mut()
            .insert(key("codex"), "{ not json".to_string());
        store.records.borrow_mut().insert(
            key("gemini"),
            serde_json::json!({ "schema_version": 2 }).to_string(),
        );
        let harness = Harness::with_store(
            vec![
                installed("claude", "1.0.0"),
                installed("codex", "1.0.0"),
                installed("gemini", "1.0.0"),
            ],
            store,
            cx,
        );
        harness.queue("codex", models_outcome(&["gpt-5"], "1.0.0"));
        harness.queue("gemini", models_outcome(&["pro"], "1.0.0"));

        harness.sync(cx);

        assert_eq!(harness.check("claude", "opus", cx), ModelCheck::Available);
        assert_eq!(harness.check("codex", "gpt-5", cx), ModelCheck::Available);
        assert_eq!(harness.check("gemini", "pro", cx), ModelCheck::Available);
        let mut probed = harness.probe.calls.borrow().clone();
        probed.sort();
        assert_eq!(probed, vec![key("codex"), key("gemini")]);
    }

    #[gpui::test]
    async fn failed_writes_do_not_publish(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("claude", "1.0.0")], cx);
        harness.store.fail_writes.set(true);
        harness.queue("claude", models_outcome(&["opus"], "1.0.0"));

        harness.sync(cx);

        assert_eq!(harness.freshness("claude", cx), None);
        assert_eq!(
            harness.check("claude", "opus", cx),
            ModelCheck::NeedsRefresh(Freshness::Unknown)
        );
    }

    #[gpui::test]
    async fn does_not_publish_live_observations_until_agent_is_configured(cx: &mut TestAppContext) {
        let harness = Harness::new(vec![installed("claude", "1.0.0")], cx);
        harness.queue("claude", models_outcome(&["opus"], "1.0.0"));
        harness.sync(cx);

        harness.catalog.update(cx, |catalog, cx| {
            catalog.observe_live(key("native"), offered(&["anything"]), None, cx)
        });
        cx.run_until_parked();

        assert!(!harness.store.records.borrow().contains_key(&key("native")));
        assert_eq!(
            harness.check("native", "anything", cx),
            ModelCheck::AgentUnavailable(AgentAvailability::NotConfigured)
        );
    }
}
