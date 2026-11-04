//! The only module that touches upstream Zed types. Everything else in this crate works on the
//! catalog's own types, so upstream changes are absorbed here.

use std::{path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use acp_thread::{AcpThread, AcpThreadEvent, AgentConnection, AuthRequired};
use agent_client_protocol::v2::schema as acp;
use agent_servers::{AgentServer as _, AgentServerDelegate, CustomAgentServer};
use anyhow::Result;
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use gpui::{App, AppContext as _, AsyncApp, Entity, EntityId, Task, WeakEntity};
use project::{
    AgentId, AgentServerStore, AgentServersUpdated, LocalProjectFlags, Project,
    agent_server_store::AgentInstallStatus,
};
use util::path_list::PathList;
use workspace::AppState;

use crate::{
    AgentDescriptor, AgentInventory, AgentKey, CatalogStore, FailureKind, Installation,
    ModelCatalog, ModelProbe, OfferedOption, OfferedSelect, OfferedValue, Ports, ProbeOutcome,
    RawBudget, SystemClock, check_raw_budget,
};

const STORE_NAMESPACE: &str = "crew_model_catalog";
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const OPTIONS_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn init(app_state: Arc<AppState>, cx: &mut App) {
    let probe_project = Project::local(
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
    let agent_server_store = probe_project.read(cx).agent_server_store().clone();
    let ports = Ports {
        inventory: Rc::new(UpstreamInventory {
            store: agent_server_store.clone(),
        }),
        probe: Rc::new(AcpModelProbe {
            project: probe_project.clone(),
            fs: app_state.fs.clone(),
            directory: paths::data_dir()
                .join("crew")
                .join("model_catalog")
                .join("probe"),
        }),
        store: Rc::new(KvpCatalogStore),
        clock: Rc::new(SystemClock),
    };
    let catalog = cx.new(|cx| ModelCatalog::new(ports, cx));
    ModelCatalog::set_global(catalog.clone(), cx);

    cx.subscribe(&agent_server_store, {
        let catalog = catalog.downgrade();
        move |_, _: &AgentServersUpdated, cx| {
            catalog
                .update(cx, |catalog, cx| catalog.sync_inventory(cx))
                .ok();
        }
    })
    .detach();

    observe_live_threads(catalog.downgrade(), probe_project.entity_id(), cx);
}

struct UpstreamInventory {
    store: Entity<AgentServerStore>,
}

impl AgentInventory for UpstreamInventory {
    fn agents(&self, cx: &mut App) -> Task<Result<Vec<AgentDescriptor>>> {
        let statuses = self.store.update(cx, |store, cx| {
            let agent_ids = store.external_agents().cloned().collect::<Vec<_>>();
            agent_ids
                .into_iter()
                .filter_map(|agent_id| {
                    let status = store.get_external_agent(&agent_id)?.install_status(cx);
                    Some((agent_key(&agent_id), status))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |_| {
            let mut descriptors = Vec::with_capacity(statuses.len());
            for (key, status) in statuses {
                let installation = match status.await {
                    Ok(status) => installation(status),
                    Err(_) => {
                        log::warn!("model_catalog: install_status_failed agent={key}");
                        Installation::NotInstalled
                    }
                };
                descriptors.push(AgentDescriptor { key, installation });
            }
            Ok(descriptors)
        })
    }
}

fn agent_key(agent_id: &AgentId) -> AgentKey {
    AgentKey(agent_id.0.clone())
}

fn installation(status: AgentInstallStatus) -> Installation {
    match status {
        AgentInstallStatus::Unmanaged => Installation::Unmanaged,
        AgentInstallStatus::NotInstalled => Installation::NotInstalled,
        AgentInstallStatus::Installed { version } => Installation::Installed { version },
    }
}

/// Reads models by opening a throwaway session on a connection owned by the catalog, so the
/// Human's panels, connections and threads are never involved.
struct AcpModelProbe {
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    directory: PathBuf,
}

impl ModelProbe for AcpModelProbe {
    fn probe(&self, agent: &AgentKey, cx: &mut App) -> Task<ProbeOutcome> {
        let agent_id = AgentId(agent.0.clone());
        let project = self.project.clone();
        let fs = self.fs.clone();
        let directory = self.directory.clone();
        cx.spawn(async move |cx| run_probe(agent_id, project, fs, directory, cx).await)
    }
}

async fn run_probe(
    agent_id: AgentId,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    directory: PathBuf,
    cx: &mut AsyncApp,
) -> ProbeOutcome {
    let timeout = cx.background_executor().timer(PROBE_TIMEOUT).fuse();
    futures::pin_mut!(timeout);
    let prepare = async {
        let store = project.read_with(cx, |project, _| project.agent_server_store().clone());
        let status = cx.update(|cx| {
            store.update(cx, |store, cx| {
                store
                    .get_external_agent(&agent_id)
                    .map(|agent| agent.install_status(cx))
            })
        });
        let Some(status) = status else {
            return Err(ProbeOutcome::NotInstalled);
        };
        let before_connect = match status.await {
            Ok(AgentInstallStatus::NotInstalled) | Err(_) => {
                return Err(ProbeOutcome::NotInstalled);
            }
            Ok(status) => installation(status),
        };
        if fs.create_dir(&directory).await.is_err() {
            return Err(ProbeOutcome::Failed(FailureKind::ConnectionFailed));
        }
        let server = Rc::new(CustomAgentServer::new(agent_id.clone()));
        let connect = cx.update(|cx| {
            server.connect(
                AgentServerDelegate::new(store.clone(), None, None),
                project.clone(),
                cx,
            )
        });
        connect
            .await
            .map(|connection| (connection, store, before_connect))
            .map_err(|_| ProbeOutcome::Failed(FailureKind::ConnectionFailed))
    };
    let (connection, store, before_connect) = futures::select_biased! {
        result = prepare.fuse() => match result { Ok(result) => result, Err(outcome) => return outcome },
        _ = timeout => return ProbeOutcome::Failed(FailureKind::Timeout),
    };
    let status = cx.update(|cx| {
        store.update(cx, |store, cx| {
            store
                .get_external_agent(&agent_id)
                .map(|agent| agent.install_status(cx))
        })
    });
    let installation = futures::select_biased! {
        installation = installed_after_connect(status, before_connect).fuse() => installation,
        _ = timeout => {
            connection.shutdown();
            return ProbeOutcome::Failed(FailureKind::Timeout);
        }
    };
    probe_connection(
        connection,
        project,
        directory,
        installation,
        &mut timeout,
        cx,
    )
    .await
}

async fn installed_after_connect(
    status: Option<Task<Result<AgentInstallStatus>>>,
    before_connect: Installation,
) -> Installation {
    match status {
        Some(status) => match status.await {
            Ok(status) => installation(status),
            Err(_) => before_connect,
        },
        None => before_connect,
    }
}

/// Opens a throwaway session on a connection the probe owns, reads its models, then closes
/// the session and shuts the connection down whatever the outcome.
async fn probe_connection<
    F: futures::Future<Output = ()> + Unpin + futures::future::FusedFuture,
>(
    connection: Rc<dyn AgentConnection>,
    project: Entity<Project>,
    directory: PathBuf,
    installation: Installation,
    mut timeout: &mut F,
    cx: &mut AsyncApp,
) -> ProbeOutcome {
    let agent_version = connection.agent_version();

    let new_session = cx.update(|cx| {
        connection
            .clone()
            .new_session(project.clone(), PathList::new(&[directory]), cx)
    });
    let session = futures::select_biased! {
        result = new_session.fuse() => result,
        _ = timeout => {
            connection.shutdown();
            return ProbeOutcome::Failed(FailureKind::Timeout);
        }
    };
    let thread = match session {
        Ok(thread) => thread,
        Err(error) => {
            connection.shutdown();
            return if AuthRequired::from_error(&error).is_some() {
                ProbeOutcome::NotLoggedIn
            } else {
                ProbeOutcome::Failed(FailureKind::SessionNewFailed)
            };
        }
    };

    let options = futures::select_biased! {
        options = read_offered_options(&connection, &thread, cx).fuse() => options,
        _ = timeout => Err(FailureKind::Timeout),
    };
    close_probe_session(&connection, &thread, cx).await;
    connection.shutdown();

    match options {
        Ok(Some(options)) => ProbeOutcome::Models {
            options,
            agent_version,
            installation,
        },
        Ok(None) => ProbeOutcome::Failed(FailureKind::NoModelOption),
        Err(kind) => ProbeOutcome::Failed(kind),
    }
}

/// Waits for the first option list that carries a model selection: agents may report it with
/// the new session or in a later update.
async fn read_offered_options(
    connection: &Rc<dyn AgentConnection>,
    thread: &Entity<AcpThread>,
    cx: &mut AsyncApp,
) -> Result<Option<Vec<OfferedOption>>, FailureKind> {
    let (updates_tx, mut updates_rx) = mpsc::unbounded();
    let _subscription = cx.update(|cx| {
        cx.subscribe(thread, move |_, event: &AcpThreadEvent, _| {
            if let AcpThreadEvent::ConfigOptionsUpdated(options) = event {
                updates_tx.unbounded_send(options.clone()).ok();
            }
        })
    });

    let current = cx.update(|cx| {
        let session_id = thread.read(cx).session_id().clone();
        connection
            .session_config_options(&session_id, cx)
            .map(|options| options.config_options())
    });
    if let Some(options) = current
        && let Some(offered) = translate_options(&options)?
    {
        return Ok(Some(offered));
    }

    let deadline = cx.background_executor().timer(OPTIONS_TIMEOUT).fuse();
    futures::pin_mut!(deadline);
    loop {
        futures::select_biased! {
            options = updates_rx.next() => {
                let Some(options) = options else {
                    return Ok(None);
                };
                if let Some(offered) = translate_options(&options)? {
                    return Ok(Some(offered));
                }
            }
            _ = deadline => return Ok(None),
        }
    }
}

async fn close_probe_session(
    connection: &Rc<dyn AgentConnection>,
    thread: &Entity<AcpThread>,
    cx: &mut AsyncApp,
) {
    if !connection.supports_close_session() {
        return;
    }
    let close = cx.update(|cx| {
        let session_id = thread.read(cx).session_id().clone();
        connection.clone().close_session(&session_id, cx)
    });
    let timeout = cx.background_executor().timer(CLOSE_TIMEOUT);
    futures::select_biased! {
        result = close.fuse() => {
            if result.is_err() {
                log::info!("model_catalog: probe_close_failed");
            }
        }
        _ = timeout.fuse() => log::info!("model_catalog: probe_close_timeout"),
    }
}

/// Translates the first usable model selection, checking its size before copying it.
/// Returns `Ok(None)` when the agent offered no model selection yet.
fn translate_options(
    options: &[acp::SessionConfigOption],
) -> Result<Option<Vec<OfferedOption>>, FailureKind> {
    let select = options.iter().find_map(|option| {
        let is_model = matches!(
            option.category,
            Some(acp::SessionConfigOptionCategory::Model)
        );
        match &option.kind {
            acp::SessionConfigKind::Select(select) if is_model && !is_empty(select) => Some(select),
            _ => None,
        }
    });
    let Some(select) = select else {
        check_raw_budget(RawBudget {
            options: options.len(),
            ..RawBudget::default()
        })
        .map_err(FailureKind::from)?;
        return Ok(None);
    };

    let (groups, values) = match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(values) => (0, values.iter().collect()),
        acp::SessionConfigSelectOptions::Grouped(groups) => (
            groups.len(),
            groups
                .iter()
                .flat_map(|group| group.options.iter())
                .collect::<Vec<_>>(),
        ),
        _ => return Ok(None),
    };
    check_raw_budget(RawBudget {
        options: options.len(),
        groups,
        values: values.len(),
        metadata_bytes: values
            .iter()
            .map(|value| {
                value.value.0.len()
                    + value.name.len()
                    + value.description.as_ref().map_or(0, String::len)
            })
            .sum(),
    })
    .map_err(FailureKind::from)?;

    let translate = |value: &acp::SessionConfigSelectOption| OfferedValue {
        value: value.value.0.to_string(),
        name: value.name.clone(),
        description: value.description.clone(),
    };
    let offered = match &select.options {
        acp::SessionConfigSelectOptions::Grouped(groups) => OfferedSelect::Grouped(
            groups
                .iter()
                .map(|group| group.options.iter().map(translate).collect())
                .collect(),
        ),
        _ => OfferedSelect::Ungrouped(values.into_iter().map(translate).collect()),
    };
    Ok(Some(vec![OfferedOption {
        is_model_category: true,
        select: Some(offered),
    }]))
}

fn is_empty(select: &acp::SessionConfigSelect) -> bool {
    match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(values) => values.is_empty(),
        acp::SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().all(|group| group.options.is_empty())
        }
        _ => true,
    }
}

/// Feeds model lists from the Human's own local threads into the catalog at no extra cost.
fn observe_live_threads(
    catalog: WeakEntity<ModelCatalog>,
    probe_project_id: EntityId,
    cx: &mut App,
) {
    cx.observe_new::<AcpThread>(move |thread, _window, cx| {
        let project = thread.project();
        if thread.parent_session_id().is_some()
            || project.entity_id() == probe_project_id
            || !project.read(cx).is_local()
        {
            return;
        }
        let connection = thread.connection().clone();
        let agent = agent_key(&connection.agent_id());
        let session_id = thread.session_id().clone();

        let publish = {
            let catalog = catalog.clone();
            let connection = connection.clone();
            move |options: &[acp::SessionConfigOption], cx: &mut App| {
                let Ok(Some(offered)) = translate_options(options) else {
                    return;
                };
                let agent_version = connection.agent_version();
                catalog
                    .update(cx, |catalog, cx| {
                        catalog.observe_live(agent.clone(), offered, agent_version, cx)
                    })
                    .ok();
            }
        };

        cx.subscribe_self({
            let publish = publish.clone();
            move |_, event: &AcpThreadEvent, cx| {
                if let AcpThreadEvent::ConfigOptionsUpdated(options) = event {
                    publish(options, cx);
                }
            }
        })
        .detach();

        // The thread is still being updated here; reading its options now would re-enter it.
        cx.defer(move |cx| {
            if let Some(options) = connection.session_config_options(&session_id, cx) {
                publish(&options.config_options(), cx);
            }
        });
    })
    .detach();
}

struct KvpCatalogStore;

impl CatalogStore for KvpCatalogStore {
    fn load(&self, agent: &AgentKey, cx: &App) -> Task<Result<Option<String>>> {
        let kvp = KeyValueStore::global(cx);
        let key = agent.0.to_string();
        cx.background_spawn(async move { kvp.scoped(STORE_NAMESPACE).read(&key) })
    }

    fn save(&self, agent: &AgentKey, value: String, cx: &App) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        let key = agent.0.to_string();
        cx.background_spawn(async move { kvp.scoped(STORE_NAMESPACE).write(key, value).await })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        any::Any,
        cell::{Cell, RefCell},
        path::Path,
    };

    use acp_thread::{AgentSessionConfigOptions, StubAgentConnection};
    use gpui::{SharedString, TestAppContext};

    use super::*;
    use crate::{Freshness, ModelCheck, extract_models, test_support::*};

    struct FixedOptions(Vec<acp::SessionConfigOption>);

    impl AgentSessionConfigOptions for FixedOptions {
        fn config_options(&self) -> Vec<acp::SessionConfigOption> {
            self.0.clone()
        }

        fn set_config_option(
            &self,
            _config_id: acp::SessionConfigId,
            _value: acp::SessionConfigOptionValue,
            _cx: &mut App,
        ) -> Task<Result<Vec<acp::SessionConfigOption>>> {
            Task::ready(Err(anyhow::anyhow!("read only")))
        }
    }

    /// Wraps the stub connection to report config options and record the probe's cleanup.
    struct OptionsConnection {
        inner: Rc<StubAgentConnection>,
        options: RefCell<Vec<acp::SessionConfigOption>>,
        new_session_error: RefCell<Option<anyhow::Error>>,
        thread: RefCell<Option<Entity<AcpThread>>>,
        close_calls: Cell<usize>,
        shutdown_calls: Cell<usize>,
        hang_new_session: Cell<bool>,
    }

    impl OptionsConnection {
        fn new(agent: &str) -> Rc<Self> {
            Rc::new(Self {
                inner: Rc::new(StubAgentConnection::new().with_agent_id(AgentId::new(agent))),
                options: RefCell::default(),
                new_session_error: RefCell::default(),
                thread: RefCell::default(),
                close_calls: Cell::default(),
                shutdown_calls: Cell::default(),
                hang_new_session: Cell::default(),
            })
        }
    }

    impl AgentConnection for OptionsConnection {
        fn agent_id(&self) -> AgentId {
            self.inner.agent_id()
        }

        fn telemetry_id(&self) -> SharedString {
            self.inner.telemetry_id()
        }

        fn agent_version(&self) -> Option<SharedString> {
            Some("acp-1.0.0".into())
        }

        fn shutdown(&self) {
            self.shutdown_calls.set(self.shutdown_calls.get() + 1);
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut App,
        ) -> Task<Result<Entity<AcpThread>>> {
            if self.hang_new_session.get() {
                return cx.spawn(async |_| futures::future::pending().await);
            }
            if let Some(error) = self.new_session_error.borrow_mut().take() {
                return Task::ready(Err(error));
            }
            let new_session = self.inner.clone().new_session(project, work_dirs, cx);
            cx.spawn(async move |_| {
                let thread = new_session.await?;
                *self.thread.borrow_mut() = Some(thread.clone());
                Ok(thread)
            })
        }

        fn supports_close_session(&self) -> bool {
            true
        }

        fn close_session(
            self: Rc<Self>,
            _session_id: &acp::SessionId,
            _cx: &mut App,
        ) -> Task<Result<()>> {
            self.close_calls.set(self.close_calls.get() + 1);
            Task::ready(Ok(()))
        }

        fn session_config_options(
            &self,
            _session_id: &acp::SessionId,
            _cx: &App,
        ) -> Option<Rc<dyn AgentSessionConfigOptions>> {
            let options = self.options.borrow().clone();
            if options.is_empty() {
                None
            } else {
                Some(Rc::new(FixedOptions(options)))
            }
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(&self, _method: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) -> Result<()> {
            Ok(())
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        Project::test(fs::FakeFs::new(cx.executor()), [], cx).await
    }

    fn model_options(ids: &[&str]) -> Vec<acp::SessionConfigOption> {
        vec![select_option(
            Some(acp::SessionConfigOptionCategory::Model),
            acp::SessionConfigSelectOptions::Ungrouped(ids.iter().map(|id| value(id)).collect()),
        )]
    }

    fn start_probe(
        connection: &Rc<OptionsConnection>,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Task<ProbeOutcome> {
        let connection: Rc<dyn AgentConnection> = connection.clone();
        let project = project.clone();
        cx.update(|cx| {
            cx.spawn(async move |cx| {
                let timeout = cx.background_executor().timer(PROBE_TIMEOUT).fuse();
                futures::pin_mut!(timeout);
                probe_connection(
                    connection,
                    project,
                    PathBuf::from("/probe"),
                    Installation::Unmanaged,
                    &mut timeout,
                    cx,
                )
                .await
            })
        })
    }

    fn probed_model_ids(outcome: ProbeOutcome) -> Vec<String> {
        let ProbeOutcome::Models { options, .. } = outcome else {
            panic!("expected the probe to report models");
        };
        extract_models(&options)
            .unwrap()
            .iter()
            .map(|model| model.model_id.to_string())
            .collect()
    }

    #[gpui::test]
    async fn probe_timeout_shuts_down_during_new_session(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let connection = OptionsConnection::new("agent");
        connection.hang_new_session.set(true);
        let probe = start_probe(&connection, &project, cx);
        cx.run_until_parked();
        cx.executor()
            .advance_clock(PROBE_TIMEOUT + Duration::from_secs(1));
        assert!(matches!(
            probe.await,
            ProbeOutcome::Failed(FailureKind::Timeout)
        ));
        assert_eq!(connection.shutdown_calls.get(), 1);
        assert_eq!(connection.close_calls.get(), 0);
    }

    #[gpui::test]
    async fn installation_is_read_again_after_connect(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let connection = OptionsConnection::new("agent");
        *connection.options.borrow_mut() = model_options(&["default"]);
        let before = Installation::Installed {
            version: Some("old".into()),
        };
        let after = installed_after_connect(
            Some(Task::ready(Ok(AgentInstallStatus::Installed {
                version: Some("new".into()),
            }))),
            before.clone(),
        )
        .await;
        let outcome = cx
            .update(|cx| {
                cx.spawn(async move |cx| {
                    let timeout = cx.background_executor().timer(PROBE_TIMEOUT).fuse();
                    futures::pin_mut!(timeout);
                    probe_connection(
                        connection,
                        project,
                        PathBuf::from("/probe"),
                        after,
                        &mut timeout,
                        cx,
                    )
                    .await
                })
            })
            .await;
        assert!(matches!(outcome, ProbeOutcome::Models {
            installation: Installation::Installed { version: Some(version) }, ..
        } if version.as_ref() == "new"));
        assert_eq!(
            installed_after_connect(
                Some(Task::ready(Err(anyhow::anyhow!("read failed")))),
                before.clone()
            )
            .await,
            before
        );
    }

    #[gpui::test]
    async fn probe_reads_initial_options_and_cleans_up(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let connection = OptionsConnection::new("agent");
        *connection.options.borrow_mut() = model_options(&["default", "opus"]);

        let outcome = start_probe(&connection, &project, cx).await;

        assert_eq!(probed_model_ids(outcome), ["default", "opus"]);
        assert_eq!(connection.close_calls.get(), 1);
        assert_eq!(connection.shutdown_calls.get(), 1);
    }

    #[gpui::test]
    async fn probe_waits_for_options_reported_after_the_session(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let connection = OptionsConnection::new("agent");

        let probe = start_probe(&connection, &project, cx);
        cx.run_until_parked();
        let thread = connection.thread.borrow().clone().unwrap();
        thread.update(cx, |_, cx| {
            cx.emit(AcpThreadEvent::ConfigOptionsUpdated(model_options(&[
                "gpt-5",
            ])))
        });

        assert_eq!(probed_model_ids(probe.await), ["gpt-5"]);
        assert_eq!(connection.shutdown_calls.get(), 1);
    }

    #[gpui::test]
    async fn probe_gives_up_when_no_models_arrive(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let connection = OptionsConnection::new("agent");

        let probe = start_probe(&connection, &project, cx);
        cx.run_until_parked();
        cx.executor().advance_clock(OPTIONS_TIMEOUT);

        assert!(matches!(
            probe.await,
            ProbeOutcome::Failed(FailureKind::NoModelOption)
        ));
        assert_eq!(connection.close_calls.get(), 1);
        assert_eq!(connection.shutdown_calls.get(), 1);
    }

    #[gpui::test]
    async fn probe_reports_agents_that_need_login(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let connection = OptionsConnection::new("agent");
        *connection.new_session_error.borrow_mut() =
            Some(anyhow::Error::new(AuthRequired { description: None }));

        let outcome = start_probe(&connection, &project, cx).await;

        assert!(matches!(outcome, ProbeOutcome::NotLoggedIn));
        assert_eq!(connection.close_calls.get(), 0);
        assert_eq!(connection.shutdown_calls.get(), 1);
    }

    #[gpui::test]
    async fn live_threads_feed_the_catalog_except_probe_threads(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let probe_project = test_project(cx).await;
        let harness = Harness::new(
            vec![AgentDescriptor {
                key: key("live-agent"),
                installation: Installation::Unmanaged,
            }],
            cx,
        );
        harness.probe.gated.set(true);
        harness.sync(cx);
        cx.update(|cx| {
            observe_live_threads(harness.catalog.downgrade(), probe_project.entity_id(), cx)
        });
        let stub = Rc::new(StubAgentConnection::new().with_agent_id(AgentId::new("live-agent")));
        let new_thread = |project: &Entity<Project>, cx: &mut TestAppContext| {
            let project = project.clone();
            let stub = stub.clone();
            cx.update(|cx| stub.new_session(project, PathList::new(&[Path::new("/")]), cx))
        };

        let probe_thread = new_thread(&probe_project, cx).await.unwrap();
        probe_thread.update(cx, |_, cx| {
            cx.emit(AcpThreadEvent::ConfigOptionsUpdated(model_options(&[
                "from-probe",
            ])))
        });
        cx.run_until_parked();
        assert_eq!(
            harness.check("live-agent", "from-probe", cx),
            ModelCheck::NeedsRefresh(Freshness::Unknown)
        );

        let thread = new_thread(&project, cx).await.unwrap();
        thread.update(cx, |_, cx| {
            cx.emit(AcpThreadEvent::ConfigOptionsUpdated(model_options(&[
                "opus",
            ])))
        });
        cx.run_until_parked();
        assert_eq!(
            harness.check("live-agent", "opus", cx),
            ModelCheck::Available
        );
    }

    fn select_option(
        category: Option<acp::SessionConfigOptionCategory>,
        options: acp::SessionConfigSelectOptions,
    ) -> acp::SessionConfigOption {
        acp::SessionConfigOption::new(
            acp::SessionConfigId::new("model"),
            "Model",
            acp::SessionConfigKind::Select(acp::SessionConfigSelect::new(
                acp::SessionConfigValueId::new("default"),
                options,
            )),
        )
        .category(category)
    }

    fn value(id: &str) -> acp::SessionConfigSelectOption {
        acp::SessionConfigSelectOption::new(acp::SessionConfigValueId::new(id), id)
    }

    #[test]
    fn translates_first_model_selection_only() {
        let options = vec![
            select_option(
                Some(acp::SessionConfigOptionCategory::ModelConfig),
                acp::SessionConfigSelectOptions::Ungrouped(vec![value("high")]),
            ),
            select_option(
                Some(acp::SessionConfigOptionCategory::Model),
                acp::SessionConfigSelectOptions::Grouped(vec![acp::SessionConfigSelectGroup::new(
                    acp::SessionConfigGroupId::new("recommended"),
                    "Recommended",
                    vec![value("default"), value("opus")],
                )]),
            ),
            select_option(
                Some(acp::SessionConfigOptionCategory::Model),
                acp::SessionConfigSelectOptions::Ungrouped(vec![value("ignored")]),
            ),
        ];

        let offered = translate_options(&options).unwrap().unwrap();
        let models = crate::extract_models(&offered).unwrap();

        let ids = models
            .iter()
            .map(|model| model.model_id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["default", "opus"]);
    }

    #[test]
    fn reports_missing_model_selection_as_none() {
        let options = vec![select_option(
            Some(acp::SessionConfigOptionCategory::Mode),
            acp::SessionConfigSelectOptions::Ungrouped(vec![value("plan")]),
        )];
        assert_eq!(translate_options(&options), Ok(None));
        assert_eq!(translate_options(&[]), Ok(None));
    }

    #[test]
    fn rejects_oversized_selection_before_copying() {
        let values = (0..=crate::extract::MAX_RAW_VALUES)
            .map(|index| value(&format!("model-{index}")))
            .collect();
        let options = vec![select_option(
            Some(acp::SessionConfigOptionCategory::Model),
            acp::SessionConfigSelectOptions::Ungrouped(values),
        )];
        assert_eq!(
            translate_options(&options),
            Err(FailureKind::RejectedLimits)
        );
    }
}
