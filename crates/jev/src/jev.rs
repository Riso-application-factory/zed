use anyhow::{Context as _, Result, ensure};
use context_server::ContextServerCommand;
use extension::ContextServerConfiguration;
use fs::Fs;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::oneshot,
    future::{BoxFuture, Shared},
};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Task};
use node_runtime::NodeRuntime;
use project::{
    context_server_store::registry::{ContextServerDescriptor, ContextServerDescriptorRegistry},
    project_settings::{ContextServerSettings, ProjectSettings},
    worktree_store::WorktreeStore,
};
use serde_json::{Value, json};
use settings::{Settings as _, SettingsLocation};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

const SERVER: &str = include_str!("../server.cjs");

pub fn init(
    node_runtime: NodeRuntime,
    fs: Arc<dyn Fs>,
    shell_environment_loaded: oneshot::Receiver<()>,
    cx: &mut App,
) {
    // Terminal launches cancel the signal because their environment is already inherited.
    let shell_environment_loaded = shell_environment_loaded.map(|_| ()).boxed().shared();
    let descriptor = Arc::new(JevDescriptor {
        node_runtime,
        fs: fs.clone(),
        shell_environment_loaded,
    });
    ContextServerDescriptorRegistry::default_global(cx).update(cx, |registry, cx| {
        registry.register_context_server_descriptor("jev".into(), descriptor, cx);
    });
    let monitor = cx.new(|cx: &mut Context<EventMonitor>| {
        let task = cx.spawn(async move |this, cx| {
            let directory = paths::data_dir().join("jev");
            if fs.create_dir(&directory).await.log_err().is_none() {
                return;
            }
            let path = directory.join("events.jsonl");
            let mut state = FailureEvents::default();
            if fs.is_file(&path).await {
                if let Some(contents) = fs.load(&path).await.log_err() {
                    state.previous = contents
                        .rfind('\n')
                        .map(|index| contents[..=index].to_string())
                        .unwrap_or_default();
                }
            }
            let (changes, _watcher) = fs.watch(&directory, Duration::from_millis(200)).await;
            let mut changes = futures::stream::once(async {})
                .chain(changes.map(|_| ()))
                .boxed();
            while changes.next().await.is_some() {
                if !fs.is_file(&path).await {
                    continue;
                }
                if let Some(contents) = fs.load(&path).await.log_err() {
                    let messages = state.consume(&contents, Instant::now());
                    cx.update(|cx| {
                        for (kind, message) in messages {
                            show_app_notification(
                                NotificationId::named(format!("jev-{kind}").into()),
                                cx,
                                move |cx| {
                                    cx.new(|cx| MessageNotification::new(message.clone(), cx))
                                },
                            );
                            this.update(cx, |_, cx| cx.emit(NotificationDispatched))
                                .log_err();
                            log::debug!("Jev failure notification dispatched: {kind}");
                        }
                    });
                }
            }
        });
        EventMonitor { _task: task }
    });
    cx.set_global(GlobalJevMonitor { _monitor: monitor });
}

struct GlobalJevMonitor {
    _monitor: Entity<EventMonitor>,
}
impl gpui::Global for GlobalJevMonitor {}
struct NotificationDispatched;
impl gpui::EventEmitter<NotificationDispatched> for EventMonitor {}

struct EventMonitor {
    _task: Task<()>,
}

#[derive(Default)]
struct FailureEvents {
    previous: String,
    recent: HashMap<String, Instant>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureEvent {
    timestamp: u64,
    tool: String,
    kind: String,
    status: Option<u16>,
    message: String,
}

impl FailureEvents {
    fn consume(&mut self, contents: &str, now: Instant) -> Vec<(String, String)> {
        let complete = contents
            .rfind('\n')
            .map(|index| &contents[..=index])
            .unwrap_or_default();
        let added = complete.strip_prefix(&self.previous).unwrap_or(complete);
        let mut messages = Vec::new();
        for line in added.lines() {
            let Ok(event) = serde_json::from_str::<FailureEvent>(line) else {
                continue;
            };
            if event.timestamp == 0
                || event.message.len() > 500
                || !["jev_decide", "jev_yes_no", "jev_score"].contains(&event.tool.as_str())
            {
                continue;
            }
            let message = match (event.kind.as_str(), event.status) {
                ("http_401", Some(401)) => "Jev request failed: HTTP 401 API key rejected — check context_servers.jev.settings.api_key".to_string(),
                (kind, Some(status)) if (100..600).contains(&status) && !(200..300).contains(&status) && kind == format!("http_{status}") => format!("Jev request failed: HTTP {status} — check settings or try again"),
                ("network", None) => "Jev request failed: check your network and context_servers.jev.settings.endpoint".into(),
                ("timeout", None) => "Jev request timed out — try again or increase context_servers.jev.settings.timeout".into(),
                ("invalid_response", None) => "Jev returned an invalid response — check model and endpoint settings".into(),
                _ => continue,
            };
            if self
                .recent
                .get(&event.kind)
                .is_some_and(|last| now.duration_since(*last) < Duration::from_secs(60))
            {
                continue;
            }
            self.recent.insert(event.kind.clone(), now);
            messages.push((event.kind, message));
        }
        self.previous = complete.to_string();
        messages
    }
}

struct JevDescriptor {
    node_runtime: NodeRuntime,
    fs: Arc<dyn Fs>,
    shell_environment_loaded: Shared<BoxFuture<'static, ()>>,
}

fn server_settings(worktree_store: &Entity<WorktreeStore>, cx: &App) -> Value {
    let settings_at = |location| match ProjectSettings::get(location, cx)
        .context_servers
        .get("jev")
    {
        Some(ContextServerSettings::Extension { settings, .. }) if settings.is_object() => {
            settings.clone()
        }
        _ => json!({}),
    };
    let global = settings_at(None);
    for worktree in worktree_store.read(cx).visible_worktrees(cx) {
        let settings = settings_at(Some(SettingsLocation {
            worktree_id: worktree.read(cx).id(),
            path: RelPath::empty(),
        }));
        if settings != global {
            return settings;
        }
    }
    global
}

fn resolve_key(settings: &Value, inherited_key: Option<&str>) -> Result<Option<String>> {
    let settings_object = settings
        .as_object()
        .context("Jev settings must be an object")?;
    let configured_key = settings_object.get("api_key");
    let configured_key = configured_key
        .map(|key| key.as_str().context("Jev setting api_key must be a string"))
        .transpose()?;
    let endpoint = settings_object
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_ENDPOINT);
    let api_key = configured_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .or_else(|| {
            inherited_key
                .filter(|_| endpoint == DEFAULT_ENDPOINT)
                .map(str::trim)
                .filter(|key| !key.is_empty())
        });
    Ok(api_key.map(str::to_owned))
}

fn script_name() -> String {
    format!("server-{:x}.cjs", Sha256::digest(SERVER.as_bytes()))
}

fn server_command(
    node: PathBuf,
    script: PathBuf,
    mut settings: Value,
    inherited_key: Option<String>,
) -> Result<ContextServerCommand> {
    ensure!(
        node.is_absolute() && script.is_absolute(),
        "Jev command paths must be absolute"
    );
    let api_key = resolve_key(&settings, inherited_key.as_deref())?;
    if let Some(settings) = settings.as_object_mut() {
        settings.remove("api_key");
    }
    let mut environment = std::collections::HashMap::new();
    // An empty override prevents the child from inheriting a key blocked for a custom endpoint.
    environment.insert("JEV_API_KEY".into(), api_key.unwrap_or_default());
    environment.insert("JEV_SETTINGS".into(), settings.to_string());
    environment.insert(
        "JEV_EVENTS_PATH".into(),
        paths::data_dir()
            .join("jev/events.jsonl")
            .to_string_lossy()
            .into_owned(),
    );
    Ok(ContextServerCommand {
        path: node,
        args: vec![script.to_string_lossy().into_owned()],
        env: Some(environment.into_iter().collect()),
        timeout: None,
    })
}

impl ContextServerDescriptor for JevDescriptor {
    fn command(
        &self,
        worktree_store: Entity<WorktreeStore>,
        cx: &AsyncApp,
    ) -> Task<Result<ContextServerCommand>> {
        let node_runtime = self.node_runtime.clone();
        let fs = self.fs.clone();
        let shell_environment_loaded = self.shell_environment_loaded.clone();
        cx.spawn(async move |cx| {
            let result = async {
                let settings = cx.update(|cx| server_settings(&worktree_store, cx));
                shell_environment_loaded.await;
                let inherited_key = std::env::var_os("JEV_API_KEY")
                    .map(|key| key.into_string())
                    .transpose()
                    .map_err(|_| anyhow::anyhow!("JEV_API_KEY must contain valid Unicode"))?;
                let node = node_runtime.binary_path().await?;
                let script = paths::data_dir().join("jev").join(script_name());
                let directory = script
                    .parent()
                    .context("Jev script has no parent directory")?;
                fs.create_dir(directory).await?;
                if !fs.is_file(&script).await || fs.load_bytes(&script).await? != SERVER.as_bytes()
                {
                    fs.atomic_write(script.clone(), SERVER.into()).await?;
                    fs.remove_file(
                        &directory.join("server.cjs"),
                        fs::RemoveOptions {
                            recursive: false,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await
                    .log_err();
                }
                server_command(node, script, settings, inherited_key)
            }
            .await;
            result
        })
    }

    fn configuration(
        &self,
        _worktree_store: Entity<WorktreeStore>,
        _cx: &AsyncApp,
    ) -> Task<Result<Option<ContextServerConfiguration>>> {
        Task::ready((|| {
            Ok(Some(ContextServerConfiguration {
                installation_instructions: INSTALLATION_INSTRUCTIONS.into(),
                settings_schema: serde_json::from_str(include_str!("../settings-schema.json"))?,
                default_settings: CONFIGURE_FORM_DEFAULTS.into(),
            }))
        })())
    }
}

const INSTALLATION_INSTRUCTIONS: &str = "Jev is built into this Zed fork; nothing needs installing. Set context_servers.jev.settings.api_key in user settings, or launch Zed with JEV_API_KEY. Jev starts without a key so its tools and this Configure form are always available. Missing-key calls return setup instructions to the agent without a notification. Failed HTTP calls show a Zed notification for native and ACP agents. Save a settings key here; the server restarts automatically without restarting Zed. The form shows your current settings, including the key in plain text. It is local-only; remote:true is unsupported. Use Write or a custom MCP-enabled agent profile. Context is sent to TypeSafe.";

// Optional settings stay commented out so saving the form does not pin today's defaults
// into user settings; the modal prefills current user values over this object itself.
const CONFIGURE_FORM_DEFAULTS: &str = r#"{
  "api_key": ""
  // Optional overrides:
  // "model": "jev-1.14.0",
  // "endpoint": "https://api.typesafe.ai/v1/systemone",
  // "timeout": 60
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_command_separates_key_and_forwards_fallback() -> Result<()> {
        let node = std::env::current_dir()?.join("node");
        let script = std::env::current_dir()?.join("server.cjs");
        for (settings, expected) in [
            (
                json!({"api_key": " settings-secret ", "model": "custom"}),
                Some("settings-secret"),
            ),
            (json!({"api_key": " "}), Some("inherited-secret")),
            (json!({}), Some("inherited-secret")),
        ] {
            let command = server_command(
                node.clone(),
                script.clone(),
                settings,
                Some("inherited-secret".into()),
            )?;
            assert_eq!(command.path, node);
            assert_eq!(command.args, vec![script.to_string_lossy().into_owned()]);
            let environment = command.env.context("Missing command environment")?;
            assert_eq!(environment.get("JEV_API_KEY").map(String::as_str), expected);
            let settings: Value = serde_json::from_str(
                environment
                    .get("JEV_SETTINGS")
                    .context("Missing settings")?,
            )?;
            assert!(settings.get("api_key").is_none());
        }
        let keyless = server_command(node.clone(), script.clone(), json!({}), None)?;
        assert_eq!(
            keyless
                .env
                .context("Missing environment")?
                .get("JEV_API_KEY")
                .map(String::as_str),
            Some("")
        );
        assert!(server_command(node, script, json!({"api_key": 42}), None).is_err());
        Ok(())
    }

    #[gpui::test]
    fn builtin_reaches_configured_store_without_user_settings(cx: &mut App) {
        use gpui::AppContext as _;
        use project::{DisableAiSettings, context_server_store::ContextServerStore};

        settings::init(cx);
        ProjectSettings::register(cx);
        DisableAiSettings::register(cx);
        let fs = fs::FakeFs::new(cx.background_executor().clone());
        let (sender, receiver) = oneshot::channel();
        drop(sender);
        init(NodeRuntime::unavailable(), fs.clone(), receiver, cx);
        let worktrees = cx.new(|_| WorktreeStore::local(false, fs, Default::default()));
        let store = cx.new(|cx| ContextServerStore::local(worktrees, None, true, cx));
        assert!(
            store
                .read(cx)
                .configured_server_ids()
                .iter()
                .any(|id| id.0.as_ref() == "jev")
        );
        let registry = ContextServerDescriptorRegistry::default_global(cx);
        assert!(registry.read(cx).context_server_descriptor("jev").is_some());
    }

    #[test]
    fn custom_endpoint_requires_explicit_key() -> Result<()> {
        assert_eq!(
            resolve_key(
                &json!({"endpoint": "https://example.test/api"}),
                Some("inherited-secret")
            )?,
            None
        );
        assert_eq!(
            resolve_key(
                &json!({"endpoint": "https://example.test/api", "api_key": "explicit-secret"}),
                Some("inherited-secret")
            )?,
            Some("explicit-secret".into())
        );
        assert_eq!(
            resolve_key(&json!({}), Some("inherited-secret"))?,
            Some("inherited-secret".into())
        );
        let node = std::env::current_dir()?.join("node");
        let script = std::env::current_dir()?.join("server.cjs");
        let command = server_command(
            node,
            script,
            json!({"endpoint":"https://example.test/api"}),
            Some("inherited-secret".into()),
        )?;
        assert_eq!(
            command
                .env
                .context("Missing environment")?
                .get("JEV_API_KEY")
                .map(String::as_str),
            Some("")
        );
        Ok(())
    }

    #[gpui::test]
    fn disabling_then_reenabling_preserves_empty_settings(cx: &mut App) {
        use gpui::{AppContext as _, UpdateGlobal as _};
        settings::init(cx);
        ProjectSettings::register(cx);
        let fs = fs::FakeFs::new(cx.background_executor().clone());
        let worktrees = cx.new(|_| WorktreeStore::local(false, fs, Default::default()));
        for content in [
            r#"{"context_servers":{"jev":{"enabled":false}}}"#,
            r#"{"context_servers":{"jev":{"enabled":true,"settings":null}}}"#,
            r#"{"context_servers":{"jev":{"enabled":true,"settings":[]}}}"#,
        ] {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(content, cx)
                    .result()
                    .expect("Valid settings");
            });
            assert_eq!(server_settings(&worktrees, cx), json!({}));
        }
    }

    #[gpui::test]
    async fn keyless_command_waits_for_shell_and_resolves_node(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, UpdateGlobal as _};
        cx.update(|cx| {
            settings::init(cx);
            ProjectSettings::register(cx);
        });
        let fs = fs::RealFs::new(None, cx.executor());
        let worktrees = cx.new(|_| WorktreeStore::local(false, fs.clone(), Default::default()));
        // A custom endpoint deliberately blocks any real inherited credential in the test environment.
        cx.update(|cx| settings::SettingsStore::update_global(cx, |store, cx| { store.set_user_settings(r#"{"context_servers":{"jev":{"settings":{"endpoint":"https://example.test/api"}}}}"#, cx).result().expect("Valid settings"); }));
        let (sender, receiver) = oneshot::channel();
        let descriptor = JevDescriptor {
            node_runtime: NodeRuntime::unavailable(),
            fs,
            shell_environment_loaded: receiver.map(|_| ()).boxed().shared(),
        };
        let mut task = Box::pin(descriptor.command(worktrees, &cx.to_async()));
        cx.run_until_parked();
        assert!(task.as_mut().now_or_never().is_none());
        sender.send(()).expect("Shell signal receiver is alive");
        task.await
            .expect_err("Unavailable Node runtime should fail even without a key");
    }

    #[test]
    fn event_parser_sanitizes_and_deduplicates_notifications() {
        let mut state = FailureEvents::default();
        let event = json!({"timestamp": 1, "tool": "jev_yes_no", "kind": "http_401", "status": 401, "message": "DO_NOT_DISPLAY_SECRET"}).to_string() + "\n";
        let now = Instant::now();
        let messages = state.consume(&event, now);
        assert_eq!(messages.len(), 1);
        assert!(messages.first().is_some_and(|(_, message)| {
            message.contains("check context_servers.jev.settings.api_key")
                && !message.contains("SECRET")
        }));
        assert!(state.consume(&(event.clone() + &event), now).is_empty());
        assert_eq!(
            state
                .consume(
                    &(event.clone() + &event + &event),
                    now + Duration::from_secs(61)
                )
                .len(),
            1
        );
        assert!(
            state
                .consume("{bad json}\n", now + Duration::from_secs(62))
                .is_empty()
        );
        assert!(state.consume("{\"timestamp\":1,\"tool\":\"jev_yes_no\",\"kind\":\"network\",\"message\":\"x\",\"state\":\"private\"}\n", now).is_empty());
    }

    #[gpui::test]
    fn keyless_descriptor_is_always_available_and_settings_change_keeps_it(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::UpdateGlobal as _;
        cx.update(|cx| {
            settings::init(cx);
            ProjectSettings::register(cx);
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.set_user_settings(r#"{"context_servers":{"jev":{"settings":{"endpoint":"https://example.test/api"}}}}"#, cx).result().expect("Valid settings");
            });
        });
        let fs = fs::FakeFs::new(cx.executor());
        let (sender, receiver) = oneshot::channel();
        drop(sender);
        cx.update(|cx| init(NodeRuntime::unavailable(), fs, receiver, cx));
        cx.run_until_parked();
        cx.update(|cx| {
            let registry = ContextServerDescriptorRegistry::default_global(cx);
            assert!(registry.read(cx).context_server_descriptor("jev").is_some());
            settings::SettingsStore::update_global(cx, |store, cx| { store.set_user_settings(r#"{"context_servers":{"jev":{"settings":{"api_key":"configured-secret"}}}}"#, cx).result().expect("Valid settings"); });
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let registry = ContextServerDescriptorRegistry::default_global(cx);
            assert!(registry.read(cx).context_server_descriptor("jev").is_some());
        });
    }

    #[gpui::test]
    async fn watched_failure_event_dispatches_notification(cx: &mut gpui::TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        cx.update(|cx| {
            settings::init(cx);
            ProjectSettings::register(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        let (sender, receiver) = oneshot::channel();
        drop(sender);
        cx.update(|cx| init(NodeRuntime::unavailable(), fs.clone(), receiver, cx));
        let count = Rc::new(Cell::new(0));
        let monitor = cx.read(|cx| cx.global::<GlobalJevMonitor>()._monitor.clone());
        let subscription = cx.update(|cx| {
            cx.subscribe(&monitor, {
                let count = count.clone();
                move |_, _: &NotificationDispatched, _| count.set(count.get() + 1)
            })
        });
        cx.run_until_parked();
        fs.atomic_write(paths::data_dir().join("jev/events.jsonl"), json!({"timestamp": 1, "tool": "jev_yes_no", "kind": "http_401", "status": 401, "message": "Unauthorized"}).to_string() + "\n").await.expect("Write event");
        cx.run_until_parked();
        assert_eq!(count.get(), 1);
        cx.update(|cx| {
            workspace::notifications::dismiss_app_notification(
                &NotificationId::named("jev-http_401".into()),
                cx,
            )
        });
        drop(subscription);
    }

    #[test]
    fn configure_form_defaults_only_set_the_key() -> Result<()> {
        let defaults: Value = settings::parse_json_with_comments(CONFIGURE_FORM_DEFAULTS)?;
        assert_eq!(defaults, json!({"api_key": ""}));
        Ok(())
    }

    #[test]
    fn shipped_defaults_enable_jev_without_user_settings() -> Result<()> {
        let defaults: settings::SettingsContent = settings::parse_json_with_comments(
            include_str!("../../../assets/settings/default.json"),
        )?;
        let configured = defaults
            .project
            .context_servers
            .get("jev")
            .context("Jev missing from shipped defaults")?;
        let configured: ContextServerSettings = configured.clone().into();
        assert!(configured.enabled());
        assert!(matches!(
            configured,
            ContextServerSettings::Extension { .. }
        ));
        Ok(())
    }
}
