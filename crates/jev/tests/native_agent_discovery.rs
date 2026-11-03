//! Runs the real embedded server with Node, so it lives in its own test binary: the custom
//! data directory must be set before any other code initializes `paths::data_dir`.

use anyhow::{Context as _, Result};
use context_server::{ContextServerId, types::requests::CallTool};
use futures::channel::oneshot;
use gpui::{AppContext as _, TestAppContext};
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project::{
    DisableAiSettings, context_server_store::ContextServerStore, project_settings::ProjectSettings,
    worktree_store::WorktreeStore,
};
use settings::Settings as _;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

// Only `node` is required: the npm path is stored but never run by the MCP server path.
fn node_on_path() -> Option<(PathBuf, PathBuf)> {
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
        let node = directory.join("node");
        node.is_file().then(|| (node, directory.join("npm")))
    })
}

#[gpui::test]
async fn native_agent_lists_jev_tools_without_key(cx: &mut TestAppContext) {
    keyless_discovery(cx)
        .await
        .expect("Keyless native agent discovery failed");
}

async fn keyless_discovery(cx: &mut TestAppContext) -> Result<()> {
    // Skips are deliberate (Node is an optional local dependency) but must be visible,
    // because the test otherwise reports success without exercising discovery.
    let Some((node, npm)) = node_on_path() else {
        eprintln!(
            "SKIPPED native_agent_lists_jev_tools_without_key: no `node` executable on PATH; \
             Jev native discovery was NOT exercised"
        );
        return Ok(());
    };
    if std::env::var_os("JEV_API_KEY").is_some() {
        eprintln!(
            "SKIPPED native_agent_lists_jev_tools_without_key: JEV_API_KEY is set; \
             the keyless path was NOT exercised"
        );
        return Ok(());
    }
    let data_directory = tempfile::tempdir()?;
    paths::set_custom_data_dir(&data_directory.path().to_string_lossy());
    cx.executor().allow_parking();

    cx.update(|cx| {
        settings::init(cx);
        ProjectSettings::register(cx);
        DisableAiSettings::register(cx);
    });
    let fs = fs::RealFs::new(None, cx.executor());
    let (_, node_options) = watch::channel(Some(NodeBinaryOptions {
        allow_path_lookup: false,
        allow_binary_download: false,
        use_paths: Some((node, npm)),
    }));
    let node_runtime =
        NodeRuntime::new(Arc::new(http_client::BlockedHttpClient), None, node_options);
    let (shell_environment_sender, shell_environment_loaded) = oneshot::channel();
    drop(shell_environment_sender);
    cx.update(|cx| jev::init(node_runtime, fs.clone(), shell_environment_loaded, cx));

    let worktrees = cx.new(|_| WorktreeStore::local(false, fs, Default::default()));
    let store = cx.new(|cx| ContextServerStore::local(worktrees, None, false, cx));
    let registry = cx.new(|cx| agent::ContextServerRegistry::new(store.clone(), cx));
    let jev = ContextServerId("jev".into());

    let deadline = Instant::now() + Duration::from_secs(30);
    let tool_names = loop {
        cx.run_until_parked();
        let tool_names = registry.read_with(cx, |registry, _| {
            let mut names = registry
                .tools_for_server(&jev)
                .map(|tool| tool.name().to_string())
                .collect::<Vec<_>>();
            names.sort();
            names
        });
        if !tool_names.is_empty() || Instant::now() > deadline {
            break tool_names;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        tool_names,
        ["jev_decide", "jev_score", "jev_yes_no"],
        "Node is on PATH but the jev server did not list its tools within 30 seconds"
    );

    let client = store
        .read_with(cx, |store, _| store.get_running_server(&jev))
        .context("Jev is not running")?
        .client()
        .context("Jev has no client")?;
    let response = client
        .request::<CallTool>(context_server::types::CallToolParams {
            name: "jev_yes_no".into(),
            arguments: Some(serde_json::json!({
                "context": "Keyless discovery test",
                "question": "Is a key configured?",
                "criteria": {"true": "A key is configured", "false": "No key is configured"},
            })),
            meta: None,
        })
        .await?;
    assert_eq!(response.is_error, Some(true));
    let text = serde_json::to_string(&response.content)?;
    assert!(
        text.contains("Settings -> MCP Servers -> jev -> Configure"),
        "Unexpected missing-key result: {text}"
    );
    assert!(
        !data_directory.path().join("jev/events.jsonl").exists(),
        "A missing key must not produce a failure notification event"
    );
    Ok(())
}
