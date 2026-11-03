use std::sync::Arc;

use context_server::ContextServerId;
use extension::ExtensionManifest;
use fs::Fs;
use gpui::WeakEntity;
use language::LanguageRegistry;
use settings::update_settings_file;
use ui::prelude::*;
use util::ResultExt;
use workspace::{MultiWorkspace, Workspace, notifications::DetachAndPromptErr as _};

use crate::agent_configuration::ConfigureContextServerModal;

pub(crate) fn init(language_registry: Arc<LanguageRegistry>, fs: Arc<dyn Fs>, cx: &mut App) {
    cx.observe_new({
        let language_registry = language_registry.clone();
        move |workspace: &mut Workspace, _window, _cx| {
            let language_registry = language_registry.clone();
            workspace.register_action(
                move |workspace,
                      action: &zed_actions::agent::ConfigureContextServer,
                      window,
                      cx| {
                    ConfigureContextServerModal::show_modal_for_existing_server(
                        ContextServerId(action.id.as_str().into()),
                        language_registry.clone(),
                        workspace.weak_handle(),
                        window,
                        cx,
                    )
                    .detach_and_prompt_err(
                        "Failed to configure MCP server",
                        window,
                        cx,
                        |_, _, _| None,
                    );
                },
            );
        }
    })
    .detach();

    let Some(extension_events) = extension::ExtensionEvents::try_global(cx) else {
        log::info!(
            "No extension events global found. Skipping context server configuration wizard"
        );
        return;
    };

    cx.subscribe(&extension_events, move |_, event, cx| match event {
        extension::Event::ExtensionUninstalled(manifest) => {
            remove_context_server_settings(
                manifest.context_servers.keys().cloned().collect(),
                fs.clone(),
                cx,
            );
        }
        extension::Event::ExtensionInstalled(manifest)
        | extension::Event::ConfigureExtensionRequested(manifest) => {
            let Some(multi_workspace) = cx
                .active_window()
                .and_then(|window| window.downcast::<MultiWorkspace>())
            else {
                return;
            };
            multi_workspace
                .update(cx, |multi_workspace, window, cx| {
                    show_configure_mcp_modal(
                        language_registry.clone(),
                        manifest,
                        multi_workspace.workspace().downgrade(),
                        window,
                        cx,
                    );
                })
                .log_err();
        }
        extension::Event::ExtensionsInstalledChanged => {}
    })
    .detach();
}

fn remove_context_server_settings(
    context_server_ids: Vec<Arc<str>>,
    fs: Arc<dyn Fs>,
    cx: &mut App,
) {
    update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .context_servers
            .retain(|server_id, _| !context_server_ids.contains(server_id));
    });
}

fn show_configure_mcp_modal(
    language_registry: Arc<LanguageRegistry>,
    manifest: &Arc<ExtensionManifest>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<'_, MultiWorkspace>,
) {
    let ids = manifest.context_servers.keys().cloned().collect::<Vec<_>>();
    if ids.is_empty() {
        return;
    }

    window
        .spawn(cx, async move |cx| {
            for id in ids {
                let Some(task) = cx
                    .update(|window, cx| {
                        ConfigureContextServerModal::show_modal_for_existing_server(
                            ContextServerId(id.clone()),
                            language_registry.clone(),
                            workspace.clone(),
                            window,
                            cx,
                        )
                    })
                    .ok()
                else {
                    continue;
                };
                task.await.log_err();
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_server::ContextServerCommand;
    use gpui::{Focusable as _, TestAppContext};
    use project::{
        FakeFs, Project,
        project_settings::{ContextServerSettings, ProjectSettings},
    };
    use settings::Settings as _;

    #[gpui::test]
    async fn test_configure_extension_only_opens_modal_in_active_workspace(
        cx: &mut TestAppContext,
    ) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        cx.update(|cx| {
            extension::init(cx);
            let mut settings = ProjectSettings::get_global(cx).clone();
            settings.context_servers.insert(
                "slack".into(),
                ContextServerSettings::Stdio {
                    // The modal only reads these settings; an enabled server would be
                    // spawned as a real process by the worktree-less test projects.
                    enabled: false,
                    remote: false,
                    command: ContextServerCommand {
                        path: "slack-mcp-server".into(),
                        args: Vec::new(),
                        env: None,
                        timeout: None,
                    },
                },
            );
            ProjectSettings::override_global(settings, cx);
            init(language_registry, fs.clone(), cx);
        });

        let active_project = Project::test(fs.clone(), [], cx).await;
        let background_project = Project::test(fs, [], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(active_project, window, cx));
        let (active_workspace, background_workspace) = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                let active_workspace = multi_workspace.workspace().clone();
                let background_workspace =
                    cx.new(|cx| Workspace::test_new(background_project, window, cx));
                multi_workspace.add(background_workspace.clone(), window, cx);
                (active_workspace, background_workspace)
            })
            .expect("multi-workspace window should exist");
        cx.run_until_parked();

        let manifest = Arc::new(
            serde_json::from_value::<ExtensionManifest>(serde_json::json!({
                "id": "slack",
                "name": "Slack",
                "version": "1.0.0",
                "schema_version": 1,
                "context_servers": {
                    "slack": {}
                }
            }))
            .expect("test extension manifest should deserialize"),
        );
        cx.update(|cx| {
            let extension_events = extension::ExtensionEvents::try_global(cx)
                .expect("extension events should be initialized");
            extension_events.update(cx, |extension_events, cx| {
                extension_events.emit(extension::Event::ConfigureExtensionRequested(manifest), cx);
            });
        });
        cx.run_until_parked();

        assert!(
            active_workspace.read_with(cx, |workspace, cx| workspace
                .active_modal::<ConfigureContextServerModal>(cx)
                .is_some()),
            "the active workspace should show the configuration modal"
        );
        assert!(
            background_workspace.read_with(cx, |workspace, cx| workspace
                .active_modal::<ConfigureContextServerModal>(cx)
                .is_none()),
            "a background workspace should not receive a hidden configuration modal"
        );
    }

    struct FakeDescriptor;

    impl project::context_server_store::registry::ContextServerDescriptor for FakeDescriptor {
        fn command(
            &self,
            _worktree_store: gpui::Entity<project::worktree_store::WorktreeStore>,
            _cx: &gpui::AsyncApp,
        ) -> gpui::Task<anyhow::Result<ContextServerCommand>> {
            gpui::Task::ready(Err(anyhow::anyhow!("not started in tests")))
        }

        fn configuration(
            &self,
            _worktree_store: gpui::Entity<project::worktree_store::WorktreeStore>,
            _cx: &gpui::AsyncApp,
        ) -> gpui::Task<anyhow::Result<Option<extension::ContextServerConfiguration>>> {
            gpui::Task::ready(Ok(Some(extension::ContextServerConfiguration {
                installation_instructions: "Set the key.".into(),
                settings_schema: serde_json::json!({"type": "object"}),
                default_settings: r#"{"api_key": ""}"#.into(),
            })))
        }
    }

    #[gpui::test]
    async fn test_configure_context_server_action_opens_modal_for_descriptor(
        cx: &mut TestAppContext,
    ) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        cx.update(|cx| {
            extension::init(cx);
            let app_state = workspace::AppState::test(cx);
            extension_host::init(
                Arc::new(extension::ExtensionHostProxy::new()),
                fs.clone(),
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                cx,
            );
            project::context_server_store::registry::ContextServerDescriptorRegistry::default_global(cx)
                .update(cx, |registry, cx| {
                    registry.register_context_server_descriptor(
                        "descriptor-server".into(),
                        Arc::new(FakeDescriptor),
                        cx,
                    );
                });
            init(language_registry, fs.clone(), cx);
        });

        let project = Project::test(fs, [], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        cx.run_until_parked();

        let workspace = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.focus_handle(cx).dispatch_action(
                    &zed_actions::agent::ConfigureContextServer {
                        id: "descriptor-server".into(),
                    },
                    window,
                    cx,
                );
                workspace
            })
            .expect("multi-workspace window should exist");
        cx.run_until_parked();

        assert!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_modal::<ConfigureContextServerModal>(cx)
                .is_some()),
            "the action should open the configuration modal for a descriptor-provided server"
        );
    }
}
