use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use async_process::Child;
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _, io::BufReader,
};
use gpui::BackgroundExecutor;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::*;

pub const CODEX_ARGS: &[&str] = &[
    "cli",
    "-c",
    "approval_policy=never",
    "-c",
    "features.plugins=false",
    "-s",
    "read-only",
    "-a",
    "never",
    "app-server",
];
const OUTPUT_LIMIT: usize = 512 * 1024;

pub struct Launch {
    pub node: PathBuf,
    pub entrypoint: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub directory: PathBuf,
    pub version: String,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
}

pub async fn installed_entrypoint(
    root: &Path,
    package_spec: &str,
) -> Result<(PathBuf, String), ErrorCode> {
    let package = package_spec
        .strip_prefix("@agentclientprotocol/codex-acp@")
        .map(|_| "@agentclientprotocol/codex-acp")
        .or_else(|| (package_spec == "@agentclientprotocol/codex-acp").then_some(package_spec))
        .ok_or(ErrorCode::UnsupportedVersion)?;
    let bytes = smol::fs::read(root.join(package).join("package.json"))
        .await
        .map_err(|_| ErrorCode::Transport)?;
    if bytes.len() > 64 * 1024 {
        return Err(ErrorCode::InvalidData);
    }
    let metadata: Package = serde_json::from_slice(&bytes).map_err(|_| ErrorCode::InvalidData)?;
    let version =
        semver::Version::parse(&metadata.version).map_err(|_| ErrorCode::UnsupportedVersion)?;
    if metadata.name != package
        || version < semver::Version::new(2, 5, 0)
        || version >= semver::Version::new(3, 0, 0)
        || !version.pre.is_empty()
    {
        return Err(ErrorCode::UnsupportedVersion);
    }
    let executable = node_runtime::read_package_executable(root.to_path_buf(), package)
        .await
        .map_err(|_| ErrorCode::InvalidData)?;
    let canonical_package = smol::fs::canonicalize(root.join(package))
        .await
        .map_err(|_| ErrorCode::Transport)?;
    let executable = smol::fs::canonicalize(executable)
        .await
        .map_err(|_| ErrorCode::Transport)?;
    if !executable.starts_with(&canonical_package)
        || !smol::fs::metadata(&executable)
            .await
            .map_err(|_| ErrorCode::Transport)?
            .is_file()
    {
        return Err(ErrorCode::InvalidData);
    }
    Ok((executable, metadata.version))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadResult {
    rate_limits_by_limit_id: Option<BTreeMap<String, Bucket>>,
    rate_limits: Option<Bucket>,
    ordinary_usage_allowed: Option<bool>,
    rate_limit_reset_credits: Option<ResetCredits>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetCredits {
    available_count: Option<u64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    limit_id: Option<String>,
    normal_model_slug: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
    plan_type: Option<String>,
    spend_control_reached: Option<bool>,
    credits: Option<Credits>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credits {
    has_credits: Option<bool>,
    unlimited: Option<bool>,
    balance: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    #[serde(default, deserialize_with = "supplied")]
    used_percent: Patch<Option<f64>>,
    window_duration_mins: Option<u64>,
    #[serde(default, deserialize_with = "supplied")]
    resets_at: Patch<Option<i64>>,
}
fn supplied<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Patch<T>, D::Error> {
    T::deserialize(deserializer).map(Patch::Set)
}

pub fn decode(
    value: &Value,
    scope: &str,
    generation: u64,
    version: &str,
    now: i64,
    sequence: u64,
    start_sequence: u64,
) -> Result<Observation, RefreshError> {
    if value.get("error").is_some() {
        // These RPC codes also represent non-auth failures; only explicit structured evidence permits NotLoggedIn.
        let auth = value.pointer("/error/data/type").and_then(Value::as_str);
        return Err(
            if matches!(
                auth,
                Some("not_authenticated" | "unauthorized" | "auth_required")
            ) {
                RefreshError::NotLoggedIn
            } else {
                RefreshError::Failed(ErrorCode::Protocol)
            },
        );
    }
    let result: ReadResult = serde_json::from_value(
        value
            .get("result")
            .cloned()
            .ok_or(RefreshError::Failed(ErrorCode::Protocol))?,
    )
    .map_err(|_| RefreshError::Failed(ErrorCode::InvalidData))?;
    let buckets = match result.rate_limits_by_limit_id {
        Some(buckets) if !buckets.is_empty() => buckets,
        _ => result
            .rate_limits
            .map(|bucket| {
                BTreeMap::from([(
                    bucket.limit_id.clone().unwrap_or_else(|| "codex".into()),
                    bucket,
                )])
            })
            .unwrap_or_default(),
    };
    if buckets.len() > 128 {
        return Err(RefreshError::Failed(ErrorCode::InvalidData));
    }
    let mut metadata = AccountMetadata {
        ordinary_usage_allowed: result.ordinary_usage_allowed,
        available_reset_credits: result
            .rate_limit_reset_credits
            .and_then(|credits| credits.available_count),
        ..Default::default()
    };
    let mut windows = Vec::new();
    for (identifier, bucket) in buckets {
        let identifier = bucket.limit_id.unwrap_or(identifier);
        if !valid_text(&identifier, 512) {
            return Err(RefreshError::Failed(ErrorCode::InvalidData));
        }
        let applicability = bucket
            .normal_model_slug
            .map(Applicability::Model)
            .unwrap_or(Applicability::AllModels);
        if metadata.plan.is_none() {
            metadata.plan = bucket.plan_type.map(|name| PlanInfo {
                name,
                observed_at: now,
                source: SourceKind::CodexAppServerRead,
            });
        }
        metadata.spend_control_reached = metadata
            .spend_control_reached
            .or(bucket.spend_control_reached);
        if let Some(credits) = bucket.credits {
            metadata.has_credits = metadata.has_credits.or(credits.has_credits);
            metadata.unlimited_credits = metadata.unlimited_credits.or(credits.unlimited);
            if let Some(balance) = credits
                .balance
                .and_then(|balance| balance.parse::<f64>().ok())
                .filter(|balance| balance.is_finite())
            {
                metadata.credit_balance = Some(Amount {
                    value: balance,
                    unit: Unit::Credits { denomination: None },
                });
            }
        }
        for (role, window) in [("primary", bucket.primary), ("secondary", bucket.secondary)] {
            let Some(window) = window else {
                continue;
            };
            let used = match window.used_percent {
                Patch::Missing => Patch::Missing,
                Patch::Set(value) => {
                    Patch::Set(normalize_percent(value).map_err(RefreshError::Failed)?)
                }
            };
            let duration = window
                .window_duration_mins
                .map(|minutes| {
                    minutes
                        .checked_mul(60)
                        .ok_or(RefreshError::Failed(ErrorCode::InvalidData))
                })
                .transpose()?;
            windows.push(WindowPatch {
                key: WindowKey {
                    bucket: BucketKey {
                        scope: scope.into(),
                        bucket: identifier.clone(),
                        applicability: applicability.clone(),
                    },
                    role: role.into(),
                    duration_seconds: duration,
                },
                used_fraction: used.clone(),
                remaining: Patch::Missing,
                limit: Patch::Missing,
                reset_at: window.resets_at,
                admission: match used {
                    Patch::Set(Some(used)) => Patch::Set(if used >= 1.0 {
                        Admission::Exhausted
                    } else {
                        Admission::Allowed
                    }),
                    _ => Patch::Missing,
                },
                disabled: Patch::Missing,
                precision: Precision::Fraction,
            });
        }
    }
    if windows.is_empty() {
        return Err(RefreshError::Unknown);
    }
    let observation = Observation {
        scope: scope.into(),
        generation,
        source: SourceKind::CodexAppServerRead,
        source_version: version.into(),
        observed_at: now,
        provider_sample_at: None,
        sequence,
        start_sequence: Some(start_sequence),
        windows,
        complete: false,
        metadata,
    };
    validate_observation(&observation, now).map_err(RefreshError::Failed)?;
    Ok(observation)
}

pub(super) struct OwnedChild {
    pub(super) child: Child,
    #[cfg(unix)]
    pub(super) group: Option<i32>,
}

impl OwnedChild {
    pub(super) fn terminate(&mut self) -> Result<(), RefreshError> {
        #[cfg(unix)]
        if let Some(group) = self.group {
            // This group belongs exclusively to this read, including descendants whose launcher already exited.
            if unsafe { libc::kill(-group, libc::SIGKILL) } != 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            {
                return Err(RefreshError::Failed(ErrorCode::CleanupFailed));
            }
            self.group = None;
        }
        match self.child.try_status() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.child.kill() {
                    #[cfg(unix)]
                    let already_gone = error.raw_os_error() == Some(libc::ESRCH);
                    #[cfg(not(unix))]
                    let already_gone = false;
                    if !already_gone && error.kind() != std::io::ErrorKind::InvalidInput {
                        return Err(RefreshError::Failed(ErrorCode::CleanupFailed));
                    }
                }
            }
            Err(_) => return Err(RefreshError::Failed(ErrorCode::CleanupFailed)),
        }
        Ok(())
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.terminate().is_err() {
            log::warn!("crew_quota: child_cleanup_failed");
        }
    }
}

pub(super) fn spawn_owned(mut command: std::process::Command) -> Result<OwnedChild, RefreshError> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
        let child = async_process::Command::from(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
        let group = i32::try_from(child.id())
            .map_err(|_| RefreshError::Failed(ErrorCode::CleanupFailed))?;
        Ok(OwnedChild {
            child,
            group: Some(group),
        })
    }
    #[cfg(not(unix))]
    {
        // std/async-process cannot atomically assign a Windows Job Object before the launcher can spawn children.
        // Reject the operation until this adapter can guarantee ownership from process creation.
        let _command = &mut command;
        Err(RefreshError::UnsupportedPlatform)
    }
}

#[derive(Clone, Copy)]
struct ReadTimeouts {
    initialize: Duration,
    read: Duration,
    cleanup: Duration,
}
impl Default for ReadTimeouts {
    fn default() -> Self {
        Self {
            initialize: Duration::from_secs(15),
            read: Duration::from_secs(25),
            cleanup: Duration::from_secs(3),
        }
    }
}

async fn bounded_response<R: futures::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    identifier: u64,
    remaining: &mut usize,
) -> Result<Value, RefreshError> {
    loop {
        let mut line = Vec::new();
        let count = reader
            .take((*remaining as u64).saturating_add(1))
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
        if count == 0 || count > *remaining {
            return Err(RefreshError::Failed(ErrorCode::InvalidData));
        }
        *remaining -= count;
        let value: Value =
            serde_json::from_slice(&line).map_err(|_| RefreshError::Failed(ErrorCode::Protocol))?;
        if value.get("id").and_then(Value::as_u64) == Some(identifier) {
            return Ok(value);
        }
        if value.get("id").is_some() {
            return Err(RefreshError::Failed(ErrorCode::Protocol));
        }
        if let Some(method) = value.get("method").and_then(Value::as_str)
            && !matches!(
                method,
                "account/updated" | "account/rateLimits/updated" | "remoteControl/status/changed"
            )
        {
            return Err(RefreshError::Failed(ErrorCode::Protocol));
        }
    }
}

pub(super) async fn deadline<T>(
    executor: &BackgroundExecutor,
    duration: Duration,
    future: impl std::future::Future<Output = Result<T, RefreshError>>,
) -> Result<T, RefreshError> {
    futures::select_biased! { value = future.fuse() => value, _ = executor.timer(duration).fuse() => Err(RefreshError::Failed(ErrorCode::Timeout)) }
}

pub async fn read(launch: Launch, executor: BackgroundExecutor) -> Result<Value, RefreshError> {
    read_with_timeouts(launch, executor, ReadTimeouts::default()).await
}

async fn read_with_timeouts(
    launch: Launch,
    executor: BackgroundExecutor,
    timeouts: ReadTimeouts,
) -> Result<Value, RefreshError> {
    let mut command = std::process::Command::new(&launch.node);
    command
        .arg(&launch.entrypoint)
        .args(CODEX_ARGS)
        .env_clear()
        .envs(&launch.environment)
        .current_dir(&launch.directory);
    let mut owned = spawn_owned(command)?;
    let mut stdin = owned
        .child
        .stdin
        .take()
        .ok_or(RefreshError::Failed(ErrorCode::Transport))?;
    let stdout = owned
        .child
        .stdout
        .take()
        .ok_or(RefreshError::Failed(ErrorCode::Transport))?;
    let mut stderr = owned
        .child
        .stderr
        .take()
        .ok_or(RefreshError::Failed(ErrorCode::Transport))?;
    let drain = executor.spawn(async move {
        let mut buffer = [0; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    log::warn!("crew_quota: stderr_drain_failed");
                    break;
                }
            }
        }
    });
    let mut reader = BufReader::new(stdout);
    let result = exchange(&mut stdin, &mut reader, &executor, timeouts).await;
    drop(stdin);
    let cleanup = deadline(&executor, timeouts.cleanup, async {
        owned
            .child
            .status()
            .await
            .map_err(|_| RefreshError::Failed(ErrorCode::CleanupFailed))
    })
    .await;
    owned.terminate()?;
    if cleanup.is_err() {
        deadline(&executor, timeouts.cleanup, async {
            owned
                .child
                .status()
                .await
                .map_err(|_| RefreshError::Failed(ErrorCode::CleanupFailed))
        })
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::CleanupFailed))?;
    }
    drop(owned);
    drop(drain);
    result
}

async fn exchange<W: futures::AsyncWrite + Unpin, R: futures::AsyncRead + Unpin>(
    stdin: &mut W,
    reader: &mut BufReader<R>,
    executor: &BackgroundExecutor,
    timeouts: ReadTimeouts,
) -> Result<Value, RefreshError> {
    let initialize = serde_json::to_vec(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"crew_quota","version":"0.1.0"}}})).map_err(|_| RefreshError::Failed(ErrorCode::Protocol))?;
    let mut remaining = OUTPUT_LIMIT;
    deadline(executor, timeouts.initialize, async {
        stdin
            .write_all(&initialize)
            .await
            .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
        stdin
            .flush()
            .await
            .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
        let response = bounded_response(reader, 1, &mut remaining).await?;
        if response.get("error").is_some() || !response.get("result").is_some_and(Value::is_object)
        {
            return Err(RefreshError::Failed(ErrorCode::Protocol));
        }
        Ok(())
    })
    .await?;
    deadline(executor, timeouts.read, async {
            stdin.write_all(b"{\"method\":\"initialized\",\"params\":{}}\n{\"id\":2,\"method\":\"account/rateLimits/read\"}\n").await.map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
            stdin.flush().await.map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
            bounded_response(reader, 2, &mut remaining).await
        }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn handshake_sends_only_three_non_generative_methods(executor: BackgroundExecutor) {
        let input = b"{\"id\":1,\"result\":{}}\n{\"method\":\"account/updated\",\"params\":{}}\n{\"id\":2,\"result\":{}}\n";
        let mut reader = BufReader::new(futures::io::Cursor::new(input));
        let mut writer = futures::io::Cursor::new(Vec::new());
        assert!(
            exchange(&mut writer, &mut reader, &executor, ReadTimeouts::default())
                .await
                .is_ok()
        );
        let methods = writer
            .into_inner()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice::<Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("method")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec![
                Some("initialize".into()),
                Some("initialized".into()),
                Some("account/rateLimits/read".into())
            ]
        );
    }

    #[gpui::test]
    async fn protocol_rejects_oversized_output_and_conversation_notifications(
        executor: BackgroundExecutor,
    ) {
        for input in [
            vec![b'x'; OUTPUT_LIMIT + 1],
            b"{\"method\":\"turn/started\",\"params\":{}}\n".to_vec(),
        ] {
            let mut reader = BufReader::new(futures::io::Cursor::new(input));
            let mut writer = futures::io::Cursor::new(Vec::new());
            assert!(
                exchange(&mut writer, &mut reader, &executor, ReadTimeouts::default())
                    .await
                    .is_err()
            );
        }
    }

    #[test]
    fn sparse_and_null_windows_are_distinct() {
        let sparse =
            json!({"result":{"rateLimits":{"limitId":"codex","primary":{"resetsAt":123}}}});
        let null =
            json!({"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":null}}}});
        let sparse = decode(&sparse, "scope", 1, "2.5.0", 100, 1, 0);
        let null = decode(&null, "scope", 1, "2.5.0", 100, 1, 0);
        assert!(matches!(
            sparse
                .ok()
                .and_then(|observation| observation.windows.into_iter().next())
                .map(|window| window.used_fraction),
            Some(Patch::Missing)
        ));
        assert!(matches!(
            null.ok()
                .and_then(|observation| observation.windows.into_iter().next())
                .map(|window| window.used_fraction),
            Some(Patch::Set(None))
        ));
    }

    #[test]
    fn spike_map_has_two_windows_and_no_duplicate_fallback() -> anyhow::Result<()> {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/live.sanitized.json"))?;
        let response = fixture
            .pointer("/results/1/shape")
            .ok_or_else(|| anyhow::anyhow!("missing shape"))?;
        // Shape-only placeholders for reset-credit counts are not numeric wire data.
        let mut response = response.clone();
        response["result"]
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("missing result"))?
            .remove("rateLimitResetCredits");
        let observation = decode(&response, "scope", 1, "2.5.0", 1793700000, 1, 0)
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(observation.windows.len(), 2);
        assert_eq!(
            observation
                .windows
                .first()
                .map(|window| &window.used_fraction),
            Some(&Patch::Set(Some(0.6)))
        );
        assert_eq!(
            observation
                .windows
                .first()
                .and_then(|window| window.key.duration_seconds),
            Some(18000)
        );
        assert!(!observation.complete);
        let mut core = crate::QuotaCore::default();
        core.inventory(
            crate::AccountScope {
                category: crate::AccountCategory::ExternalAgent,
                native_provider_id: None,
                id: "scope".into(),
                provider: crate::ProviderId::Codex,
                runtime: "local".into(),
                identity_known: false,
            },
            1,
            crate::Availability::Ready,
        )
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let selector = crate::Selector {
            windows: observation
                .windows
                .iter()
                .map(|window| window.key.clone())
                .collect(),
            ..Default::default()
        };
        core.apply(observation, 1793700000)
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert!(
            core.fresh(
                &crate::FreshRequest {
                    selector,
                    max_age: Duration::from_secs(120),
                    required_measurements: Vec::new(),
                    deadline: 1793700120,
                    allow_inference: false,
                },
                1793700000
            )
            .is_ok()
        );
        Ok(())
    }

    #[test]
    fn rpc_codes_alone_do_not_claim_auth_failure() -> anyhow::Result<()> {
        for fixture in [
            include_str!("../../tests/fixtures/logged-out.sanitized.json"),
            include_str!("../../tests/fixtures/offline.sanitized.json"),
        ] {
            let fixture: Value = serde_json::from_str(fixture)?;
            let response = fixture
                .pointer("/results/1/shape")
                .ok_or_else(|| anyhow::anyhow!("missing shape"))?;
            assert!(matches!(
                decode(response, "scope", 1, "2.5.0", 0, 1, 0),
                Err(RefreshError::Failed(ErrorCode::Protocol))
            ));
        }
        Ok(())
    }

    #[test]
    fn installed_tree_uses_bin_and_rejects_unsupported_versions() -> anyhow::Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let package = directory.path().join("@agentclientprotocol/codex-acp");
            std::fs::create_dir_all(package.join("dist"))?;
            std::fs::write(package.join("dist/index.js"), "")?;
            for (version, supported) in [
                ("2.4.9", false),
                ("2.5.0", true),
                ("2.8.1", true),
                ("3.0.0", false),
            ] {
                std::fs::write(
                    package.join("package.json"),
                    serde_json::to_vec(
                        &json!({"name":"@agentclientprotocol/codex-acp","version":version,"bin":{"codex-acp":"dist/index.js"}}),
                    )?,
                )?;
                let result =
                    installed_entrypoint(directory.path(), "@agentclientprotocol/codex-acp").await;
                assert_eq!(result.is_ok(), supported);
                if let Ok((entrypoint, _)) = result {
                    assert_eq!(
                        entrypoint,
                        std::fs::canonicalize(package.join("dist/index.js"))?
                    );
                }
            }
            Ok(())
        })
    }
    fn fixture_launch(directory: &Path, mode: &str) -> anyhow::Result<Launch> {
        let test_binary = std::env::current_exe()?;
        let executable = test_binary
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow::anyhow!("missing target directory"))?
            .join(format!(
                "crew_quota_process_fixture{}",
                std::env::consts::EXE_SUFFIX
            ));
        anyhow::ensure!(
            executable.is_file(),
            "Cargo did not build the process fixture"
        );
        Ok(Launch {
            node: executable,
            entrypoint: directory.join("unused"),
            directory: directory.into(),
            version: "synthetic".into(),
            environment: BTreeMap::from([
                (
                    "CREW_QUOTA_FIXTURE_DIRECTORY".into(),
                    directory.to_string_lossy().into_owned(),
                ),
                ("CREW_QUOTA_FIXTURE_MODE".into(), mode.into()),
            ]),
        })
    }

    #[cfg(unix)]
    fn assert_fixture_terminated(directory: &Path) -> anyhow::Result<()> {
        for file in ["launcher.pid", "descendant.pid"] {
            let identifier = std::fs::read_to_string(directory.join(file))?.parse::<i32>()?;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let alive = unsafe { libc::kill(identifier, 0) } == 0;
                #[cfg(target_os = "linux")]
                let zombie = std::fs::read_to_string(format!("/proc/{identifier}/stat"))
                    .ok()
                    .is_some_and(|status| {
                        status
                            .split_once(") ")
                            .is_some_and(|(_, status)| status.starts_with('Z'))
                    });
                #[cfg(not(target_os = "linux"))]
                let zombie = false;
                if !alive || zombie {
                    break;
                }
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "fixture process remained alive: {file}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }

    #[test]
    fn synthetic_process_success_protocol_and_timeout_cleanup() -> anyhow::Result<()> {
        let executor =
            BackgroundExecutor::new(std::sync::Arc::new(gpui::ThreadedDispatcher::new()));
        for mode in ["success", "protocol", "timeout"] {
            let directory = tempfile::tempdir()?;
            let result = smol::block_on(read_with_timeouts(
                fixture_launch(directory.path(), mode)?,
                executor.clone(),
                ReadTimeouts {
                    initialize: Duration::from_secs(5),
                    read: Duration::from_millis(300),
                    cleanup: Duration::from_millis(300),
                },
            ));
            #[cfg(windows)]
            {
                assert!(matches!(result, Err(RefreshError::UnsupportedPlatform)));
                assert!(!directory.path().join("launcher.pid").exists());
            }
            #[cfg(unix)]
            {
                match mode {
                    "success" => assert!(result.is_ok()),
                    "protocol" => assert!(matches!(
                        result,
                        Err(RefreshError::Failed(ErrorCode::Protocol))
                    )),
                    "timeout" => assert!(matches!(
                        result,
                        Err(RefreshError::Failed(ErrorCode::Timeout))
                    )),
                    _ => unreachable!(),
                }
                assert_fixture_terminated(directory.path())?;
            }
        }
        Ok(())
    }

    #[test]
    fn synthetic_process_cancellation_cleanup() -> anyhow::Result<()> {
        let executor =
            BackgroundExecutor::new(std::sync::Arc::new(gpui::ThreadedDispatcher::new()));
        let directory = tempfile::tempdir()?;
        let launch = fixture_launch(directory.path(), "cancel")?;
        smol::block_on(async {
            let operation = read(launch, executor.clone()).fuse();
            futures::pin_mut!(operation);
            #[cfg(windows)]
            assert!(matches!(
                operation.await,
                Err(RefreshError::UnsupportedPlatform)
            ));
            #[cfg(unix)]
            {
                let ready = async {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !directory.path().join("heartbeat").exists() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fixture failed to launch descendant"
                        );
                        executor.timer(Duration::from_millis(20)).await;
                    }
                }
                .fuse();
                futures::pin_mut!(ready);
                futures::select_biased! { _ = operation => panic!("read completed before cancellation"), _ = ready => {} }
            }
        });
        #[cfg(unix)]
        assert_fixture_terminated(directory.path())?;
        #[cfg(windows)]
        assert!(!directory.path().join("launcher.pid").exists());
        Ok(())
    }
}
