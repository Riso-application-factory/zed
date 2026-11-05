use super::*;
use futures::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, io::BufReader};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

pub(super) const AGENT: &str = "antigravity-acp";
const SCOPE: &str = "antigravity-registry";
const LIMIT: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
static READ_LOCK: futures::lock::Mutex<()> = futures::lock::Mutex::new(());
const CLEANUP: Duration = Duration::from_secs(3);
fn protocol() -> RefreshError {
    RefreshError::Failed(ErrorCode::Protocol)
}
fn cleanup_error() -> RefreshError {
    RefreshError::Failed(ErrorCode::CleanupFailed)
}

pub(super) fn parse(text: &str, scope: &str) -> Result<Vec<WindowPatch>, RefreshError> {
    let (session, quota) = text.split_once("### Quota Limits\n").ok_or_else(protocol)?;
    if session.trim() != "### Session Usage\n- **Session total:** data not available yet" {
        return Err(protocol());
    }
    let mut group = None;
    let mut windows = Vec::new();
    let mut numeric = false;
    for line in quota.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(label) = line
            .strip_prefix("- **")
            .and_then(|line| line.strip_suffix("**"))
        {
            group = Some(label);
            continue;
        }
        if line.starts_with("  *(") && line.ends_with(")*") && group.is_some() {
            continue;
        }
        if line == "  - No active bucket limits" && group.is_some() {
            continue;
        }
        let (label, status) = line
            .strip_prefix("  - **")
            .and_then(|line| line.split_once(":** "))
            .ok_or_else(protocol)?;
        let group = group.ok_or_else(protocol)?;
        let (status, reset) = if let Some((status, reset)) = status.rsplit_once(" (resets ") {
            (
                status,
                reset
                    .strip_suffix(')')
                    .and_then(|reset| {
                        chrono::NaiveDateTime::parse_from_str(reset, "%b %d, %Y, %H:%M UTC").ok()
                    })
                    .map(|reset| reset.and_utc().timestamp()),
            )
        } else {
            (status, None)
        };
        let mut used = None;
        let mut remaining = None;
        let disabled = status == "Disabled" || status.starts_with("Disabled: ");
        if let Some(percent) = status.strip_suffix("% remaining") {
            if percent.is_empty()
                || percent.len() > 3
                || !percent.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(protocol());
            }
            let percent: u8 = percent.parse().map_err(|_| protocol())?;
            if percent > 100 {
                return Err(protocol());
            }
            used = Some(f64::from(100 - percent) / 100.0);
        } else if let Some(amount) = status.strip_suffix(" units remaining") {
            let digits = amount.strip_prefix('-').unwrap_or(amount);
            let parts: Vec<_> = digits.split(',').collect();
            if parts
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
                || (parts.len() > 1
                    && (parts.first().is_none_or(|part| part.len() > 3)
                        || parts.iter().skip(1).any(|part| part.len() != 3)))
            {
                return Err(protocol());
            }
            let amount: i64 = amount.replace(',', "").parse().map_err(|_| protocol())?;
            remaining = Some(Amount {
                value: amount as f64,
                unit: Unit::UnspecifiedUnits,
            });
        } else if !disabled && status != "Available" {
            return Err(protocol());
        }
        let duration = match label {
            "Weekly Limit Remaining" => Some(604800),
            "Five Hour Limit Remaining" => Some(18000),
            _ => None,
        };
        if !matches!(group, "Gemini Models" | "Claude and GPT models") || duration.is_none() {
            continue;
        }
        numeric |= used.is_some() || remaining.is_some();
        windows.push(WindowPatch {
            key: WindowKey {
                bucket: BucketKey {
                    scope: scope.into(),
                    bucket: format!("agy-v1/{group}"),
                    applicability: Applicability::Family(group.into()),
                },
                role: label.into(),
                duration_seconds: duration,
            },
            used_fraction: Patch::Set(used),
            remaining: Patch::Set(remaining),
            limit: Patch::Missing,
            reset_at: Patch::Set(reset),
            admission: Patch::Set(Admission::Unknown),
            disabled: Patch::Set(disabled),
            precision: if used.is_some() {
                Precision::RoundedPercent
            } else {
                Precision::Exact
            },
        });
    }
    if !numeric {
        return Err(protocol());
    }
    Ok(windows)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    jsonrpc: String,
    id: Option<u64>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<Value>,
}
#[derive(Default)]
struct Updates {
    session: Option<String>,
    usage: bool,
    text: String,
    baseline: Option<u64>,
    prompting: bool,
}
impl Updates {
    fn accept(&mut self, value: &Envelope) -> Result<(), RefreshError> {
        if value.result.is_some()
            || value.error.is_some()
            || value.id.is_some()
            || value.method.as_deref() != Some("session/update")
        {
            return Err(protocol());
        }
        let params = value.params.as_ref().ok_or_else(protocol)?;
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(protocol)?;
        if self
            .session
            .as_deref()
            .is_some_and(|owned| owned != session)
        {
            return Err(protocol());
        }
        // Session notifications can precede the session/new response; bind them immediately.
        self.session = Some(session.into());
        let update = params.get("update").ok_or_else(protocol)?;
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("available_commands_update") => {
                let commands = update
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .ok_or_else(protocol)?;
                self.usage = commands
                    .iter()
                    .any(|command| command.get("name").and_then(Value::as_str) == Some("usage"));
            }
            Some("agent_message_chunk") if self.prompting => {
                let content = update.get("content").ok_or_else(protocol)?;
                if content.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(protocol());
                }
                let text = content
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(protocol)?;
                if self.text.len().saturating_add(text.len()) > LIMIT {
                    return Err(protocol());
                }
                self.text.push_str(text);
            }
            Some("usage_update") => {
                let used = update
                    .get("used")
                    .and_then(Value::as_u64)
                    .ok_or_else(protocol)?;
                if used > self.baseline.unwrap_or(0) {
                    return Err(protocol());
                }
                self.baseline = Some(used);
            }
            Some("current_mode_update" | "config_option_update") if self.text.is_empty() => {}
            _ => return Err(protocol()),
        }
        Ok(())
    }
}
async fn send(
    writer: &mut (impl futures::AsyncWrite + Unpin),
    id: u64,
    method: &str,
    params: Value,
) -> Result<(), RefreshError> {
    let mut bytes =
        serde_json::to_vec(&json!({"jsonrpc":"2.0", "id":id,"method":method,"params":params}))
            .map_err(|_| protocol())?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
    writer
        .flush()
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::Transport))
}
async fn receive(
    reader: &mut BufReader<impl futures::AsyncRead + Unpin>,
    budget: &AtomicUsize,
) -> Result<Envelope, RefreshError> {
    let mut line = Vec::new();
    let count = reader
        .take((LIMIT + 1) as u64)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|_| RefreshError::Failed(ErrorCode::Transport))?;
    if count == 0
        || budget
            .fetch_add(count, Ordering::Relaxed)
            .saturating_add(count)
            > LIMIT
        || line.last() != Some(&b'\n')
    {
        return Err(protocol());
    }
    let envelope: Envelope = serde_json::from_slice(&line).map_err(|_| protocol())?;
    if envelope.jsonrpc != "2.0" {
        return Err(protocol());
    }
    Ok(envelope)
}
async fn response(
    reader: &mut BufReader<impl futures::AsyncRead + Unpin>,
    id: u64,
    updates: &mut Updates,
    budget: &AtomicUsize,
) -> Result<Value, RefreshError> {
    loop {
        let value = receive(reader, budget).await?;
        if value.id == Some(id) && value.method.is_none() && value.params.is_none() {
            if value.error.is_some() {
                return Err(protocol());
            }
            return value.result.ok_or_else(protocol);
        }
        updates.accept(&value)?;
    }
}

pub(super) fn installed(
    root: &Path,
    version: &str,
    command: &str,
) -> Result<PathBuf, RefreshError> {
    if command != "./agy_acp_server.par" || version != "1.4.5" {
        return Err(RefreshError::Unsupported);
    }
    let prefix = format!(
        "v_{}_",
        version
            .chars()
            .map(
                |character| if character.is_ascii_alphanumeric() || "._-".contains(character) {
                    character
                } else {
                    '-'
                }
            )
            .collect::<String>()
    );
    let mut found = None;
    for entry in std::fs::read_dir(root).map_err(|_| RefreshError::NotInstalled)? {
        let entry = entry.map_err(|_| RefreshError::NotInstalled)?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        if !entry.file_type().map_err(|_| protocol())?.is_dir() {
            return Err(RefreshError::Unsupported);
        }
        let executable = entry.path().join("agy_acp_server.par");
        if std::fs::symlink_metadata(&executable)
            .is_ok_and(|metadata| metadata.file_type().is_file())
        {
            if found.replace(executable).is_some() {
                return Err(RefreshError::Unsupported);
            }
        }
    }
    found.ok_or(RefreshError::NotInstalled)
}

fn compose_environment(
    mut base: BTreeMap<String, String>,
    target: impl Iterator<Item = (String, String)>,
    settings: impl Iterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    base.extend(target);
    base.extend(settings);
    base
}

struct Launch {
    command: PathBuf,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    directory: PathBuf,
    conversations: PathBuf,
    version: String,
    timeout: Duration,
}
async fn resolve(
    project: Entity<Project>,
    cx: &mut gpui::AsyncApp,
) -> Result<Launch, RefreshError> {
    let store = project.read_with(cx, |project, _| project.agent_server_store().clone());
    let status = store
        .update(cx, |store, cx| {
            store
                .get_external_agent(&AgentId::new(AGENT))
                .map(|agent| agent.install_status(cx))
        })
        .ok_or(RefreshError::NotInstalled)?
        .await
        .map_err(|_| RefreshError::Unsupported)?;
    let AgentInstallStatus::Installed {
        version: Some(version),
    } = status
    else {
        return Err(RefreshError::NotInstalled);
    };
    let (target, settings_env, environment) = cx.update(|cx| {
        let settings = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None);
        let Some(CustomAgentServerSettings::Registry { env, .. }) = settings.get(AGENT) else {
            return Err(RefreshError::Unsupported);
        };
        let settings_env = env.clone();
        let registry = AgentRegistryStore::try_global(cx)
            .and_then(|store| {
                store
                    .read(cx)
                    .agents()
                    .iter()
                    .find(|agent| agent.id().0.as_ref() == AGENT)
                    .cloned()
            })
            .ok_or(RefreshError::Unsupported)?;
        let RegistryAgent::Binary(registry) = registry else {
            return Err(RefreshError::Unsupported);
        };
        if registry.metadata.version.as_ref() != version.as_str() {
            return Err(RefreshError::Unsupported);
        }
        let platform = format!(
            "{}-{}",
            if cfg!(target_os = "macos") {
                "darwin"
            } else {
                std::env::consts::OS
            },
            std::env::consts::ARCH
        );
        let target = registry
            .targets
            .get(&platform)
            .cloned()
            .ok_or(RefreshError::Unsupported)?;
        let environment = project
            .read(cx)
            .environment()
            .clone()
            .update(cx, |environment, cx| environment.default_environment(cx));
        Ok((target, settings_env, environment))
    })?;
    let mut environment: BTreeMap<_, _> = environment
        .await
        .ok_or(RefreshError::Unsupported)?
        .into_iter()
        .collect();
    environment = compose_environment(
        environment,
        target.env.into_iter(),
        settings_env.into_iter(),
    );
    let command = installed(
        &paths::external_agents_dir().join("registry").join(AGENT),
        &version,
        &target.cmd,
    )?;
    let home = environment
        .get("GEMINI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            environment
                .get("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths::home_dir().into())
                .join(".gemini")
        });
    let directory = paths::data_dir().join("crew/quota/antigravity");
    smol::fs::create_dir_all(&directory)
        .await
        .map_err(|_| cleanup_error())?;
    Ok(Launch {
        command,
        args: target.args,
        environment,
        directory,
        conversations: home
            .join("antigravity-acp/conversations")
            .canonicalize()
            .map_err(conversations_error)?,
        version: version.to_string(),
        timeout: TIMEOUT,
    })
}

fn conversations_error(_error: std::io::Error) -> RefreshError {
    RefreshError::Failed(ErrorCode::Transport)
}

#[test]
fn missing_conversations_is_not_evidence_of_missing_auth() {
    assert_eq!(
        conversations_error(std::io::Error::from(std::io::ErrorKind::NotFound)),
        RefreshError::Failed(ErrorCode::Transport)
    );
}

#[cfg(unix)]
mod files {
    use super::*;
    use std::{
        ffi::CString,
        os::fd::{AsRawFd as _, FromRawFd as _},
    };
    pub(super) struct Directory(std::fs::File);
    impl Directory {
        pub(super) fn open(path: &Path) -> Result<Self, RefreshError> {
            if !path.is_absolute() {
                return Err(cleanup_error());
            }
            let mut directory = std::fs::File::open("/").map_err(|_| cleanup_error())?;
            for component in path.components() {
                let std::path::Component::Normal(component) = component else {
                    if component == std::path::Component::RootDir {
                        continue;
                    }
                    return Err(cleanup_error());
                };
                let component =
                    CString::new(component.as_encoded_bytes()).map_err(|_| cleanup_error())?;
                let descriptor = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        component.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if descriptor < 0 {
                    return Err(cleanup_error());
                }
                directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
            }
            Ok(Self(directory))
        }
        fn exists(&self, name: &str) -> Result<bool, RefreshError> {
            let name = CString::new(name).map_err(|_| cleanup_error())?;
            let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    status.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                    return Ok(false);
                }
                return Err(cleanup_error());
            }
            let status = unsafe { status.assume_init() };
            if status.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(cleanup_error());
            }
            Ok(true)
        }
        pub(super) fn identity(&self) -> Result<(u64, u64), RefreshError> {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = self.0.metadata().map_err(|_| cleanup_error())?;
            Ok((metadata.dev(), metadata.ino()))
        }
        pub(super) fn remove(
            &self,
            session: &str,
            before: &BTreeSet<String>,
        ) -> Result<(), RefreshError> {
            if session.is_empty()
                || session.len() > 128
                || !session
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Err(cleanup_error());
            }
            let names: Vec<_> = [".db", ".db-shm", ".db-wal", ".meta"]
                .iter()
                .map(|suffix| format!("{session}{suffix}"))
                .collect();
            // Validate the entire set before removing anything; pre-existing names never belong to us.
            for name in &names {
                if before.contains(name) {
                    return Err(cleanup_error());
                }
                self.exists(name)?;
            }
            for name in names {
                if self.exists(&name)? {
                    let component = CString::new(name.clone()).map_err(|_| cleanup_error())?;
                    if unsafe { libc::unlinkat(self.0.as_raw_fd(), component.as_ptr(), 0) } != 0 {
                        return Err(cleanup_error());
                    }
                }
                if self.exists(&name)? {
                    return Err(cleanup_error());
                }
            }
            Ok(())
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Journal {
    session: Option<String>,
    group: i32,
    directory: PathBuf,
    identity: (u64, u64),
    before: BTreeSet<String>,
}
fn save_journal(path: &Path, journal: &Journal) -> Result<(), RefreshError> {
    use std::io::Write as _;
    let temporary = path.with_extension("new");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary).map_err(|_| cleanup_error())?;
    let bytes = serde_json::to_vec(journal).map_err(|_| cleanup_error())?;
    if bytes.len() > LIMIT {
        return Err(cleanup_error());
    }
    file.write_all(&bytes).map_err(|_| cleanup_error())?;
    file.sync_all().map_err(|_| cleanup_error())?;
    std::fs::rename(temporary, path).map_err(|_| cleanup_error())?;
    std::fs::File::open(path.parent().ok_or_else(cleanup_error)?)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| cleanup_error())
}
#[cfg(unix)]
fn group_exited(group: i32) -> bool {
    group > 0
        && unsafe { libc::kill(-group, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(unix)]
async fn read(
    launch: Launch,
    executor: gpui::BackgroundExecutor,
) -> Result<(String, Option<String>), RefreshError> {
    use providers::{deadline, spawn_owned};
    let _read_guard = READ_LOCK.lock().await;
    let journal_path = launch.directory.join("pending-session.json");
    if journal_path.exists() {
        let bytes = std::fs::read(&journal_path).map_err(|_| cleanup_error())?;
        if bytes.len() > 512 * 1024 {
            return Err(cleanup_error());
        }
        let journal: Journal = serde_json::from_slice(&bytes).map_err(|_| cleanup_error())?;
        if journal.directory != launch.conversations || !group_exited(journal.group) {
            return Err(cleanup_error());
        }
        let directory = files::Directory::open(&journal.directory)?;
        if directory.identity()? != journal.identity {
            return Err(cleanup_error());
        }
        if let Some(session) = journal.session.as_deref() {
            directory.remove(session, &journal.before)?;
        }
        std::fs::remove_file(&journal_path).map_err(|_| cleanup_error())?;
    }
    // Missing conversations directories are not created on behalf of the producer.
    let directory = files::Directory::open(&launch.conversations)?;
    let before: BTreeSet<String> = std::fs::read_dir(&launch.conversations)
        .map_err(|_| cleanup_error())?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|_| cleanup_error())?;
    let mut command = std::process::Command::new(launch.command);
    command
        .args(launch.args)
        .env_clear()
        .envs(launch.environment)
        .current_dir(&launch.directory);
    let mut owned = spawn_owned(command)?;
    let mut journal = Journal {
        session: None,
        group: owned.group.ok_or_else(cleanup_error)?,
        directory: launch.conversations,
        identity: directory.identity()?,
        before,
    };
    save_journal(&journal_path, &journal)?;
    let mut writer = owned.child.stdin.take().ok_or_else(protocol)?;
    let mut reader = BufReader::new(owned.child.stdout.take().ok_or_else(protocol)?);
    let mut stderr = owned.child.stderr.take().ok_or_else(protocol)?;
    let budget = Arc::new(AtomicUsize::new(0));
    let (overflow, overflow_receiver) = async_channel::bounded::<()>(1);
    let drain = executor.spawn({
        let budget = budget.clone();
        async move {
            let mut bytes = [0; 4096];
            loop {
                match stderr.read(&mut bytes).await {
                    Ok(0) => break,
                    Ok(count)
                        if budget
                            .fetch_add(count, Ordering::Relaxed)
                            .saturating_add(count)
                            <= LIMIT => {}
                    Ok(_) | Err(_) => {
                        if overflow.send(()).await.is_err() {
                            log::debug!("crew_quota: quota_reader_closed");
                        }
                        break;
                    }
                }
            }
        }
    });
    let mut updates = Updates::default();
    let mut delete = false;
    let operation = async {
        send(&mut writer, 1, "initialize", json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"Crew quota","version":"0.1.0"}})).await?;
        let initialize = response(&mut reader, 1, &mut updates, &budget).await?;
        if initialize.get("protocolVersion").and_then(Value::as_u64) != Some(1)
            || !initialize
                .get("agentCapabilities")
                .is_some_and(Value::is_object)
        {
            return Err(protocol());
        }
        delete = initialize
            .pointer("/agentCapabilities/sessionCapabilities/delete")
            .is_some_and(|value| value.is_object() || value == &Value::Bool(true));
        send(
            &mut writer,
            2,
            "session/new",
            json!({"cwd":launch.directory,"mcpServers":[]}),
        )
        .await?;
        let session = response(&mut reader, 2, &mut updates, &budget).await?;
        let identifier = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(protocol)?;
        journal.session = Some(identifier.into());
        save_journal(&journal_path, &journal)?;
        if updates
            .session
            .as_deref()
            .is_some_and(|session| session != identifier)
        {
            return Err(protocol());
        }
        updates.session = Some(identifier.into());
        while !updates.usage {
            updates.accept(&receive(&mut reader, &budget).await?)?;
        }
        if !updates.text.is_empty() {
            return Err(protocol());
        }
        updates.prompting = true;
        send(
            &mut writer,
            3,
            "session/prompt",
            json!({"sessionId":identifier,"prompt":[{"type":"text","text":"/usage"}]}),
        )
        .await?;
        let result = response(&mut reader, 3, &mut updates, &budget).await?;
        updates.prompting = false;
        if result
            .as_object()
            .is_none_or(|object| object.keys().any(|key| key != "stopReason"))
        {
            return Err(protocol());
        }
        if result.get("stopReason").and_then(Value::as_str) != Some("end_turn") {
            return Err(protocol());
        }
        parse(&updates.text, SCOPE)?;
        let tier = session
            .pointer("/_meta/antigravity/tier/name")
            .and_then(Value::as_str)
            .filter(|tier| valid_text(tier, 128))
            .map(str::to_owned);
        Ok((updates.text.clone(), tier))
    };
    let result = deadline(&executor, launch.timeout, async {
        futures::select_biased! { result = operation.fuse() => result, _ = overflow_receiver.recv().fuse() => Err(protocol()) }
    }).await;
    if journal.session.is_none() {
        journal.session = updates.session.clone();
        save_journal(&journal_path, &journal)?;
    }
    updates.prompting = false;
    let close = if let Some(session) = &journal.session {
        deadline(&executor, CLEANUP, async {
            send(
                &mut writer,
                4,
                "session/close",
                json!({"sessionId":session}),
            )
            .await?;
            response(&mut reader, 4, &mut updates, &budget).await?;
            if delete {
                send(
                    &mut writer,
                    5,
                    "session/delete",
                    json!({"sessionId":session}),
                )
                .await?;
                response(&mut reader, 5, &mut updates, &budget).await?;
            }
            Ok(())
        })
        .await
    } else {
        Err(cleanup_error())
    };
    drop(writer);
    owned.terminate()?;
    deadline(&executor, CLEANUP, async {
        owned.child.status().await.map_err(|_| cleanup_error())?;
        while !group_exited(journal.group) {
            executor.timer(Duration::from_millis(20)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| cleanup_error())?;
    drop(drain);
    if files::Directory::open(&journal.directory)?.identity()? != journal.identity {
        return Err(cleanup_error());
    }
    if let Some(session) = journal.session.as_deref() {
        directory.remove(session, &journal.before)?;
    }
    std::fs::remove_file(journal_path).map_err(|_| cleanup_error())?;
    close.map_err(|_| cleanup_error())?;
    result
}
#[cfg(not(unix))]
async fn read(
    _launch: Launch,
    _executor: gpui::BackgroundExecutor,
) -> Result<(String, Option<String>), RefreshError> {
    Err(RefreshError::UnsupportedPlatform)
}

pub(super) fn refresh(
    project: Entity<Project>,
    scope: String,
    generation: u64,
    sequence: u64,
    cx: &mut App,
) -> Task<Result<Observation, RefreshError>> {
    cx.spawn(async move |cx| {
        if !cfg!(unix) {
            return Err(RefreshError::UnsupportedPlatform);
        }
        let launch = resolve(project, cx).await?;
        let version = launch.version.clone();
        let executor = cx.background_executor().clone();
        // Cancellation of a consumer must not cancel session cleanup or release single flight early.
        let (sender, receiver) = async_channel::bounded(1);
        cx.background_spawn(async move {
            let result = read(launch, executor).await;
            if sender.send(result).await.is_err() {
                log::debug!("crew_quota: antigravity_consumer_closed");
            }
        })
        .detach();
        let (text, tier) = receiver.recv().await.map_err(|_| cleanup_error())??;
        let now = chrono::Utc::now().timestamp();
        let observation = Observation {
            scope: scope.clone(),
            generation,
            source: SourceKind::AntigravityAcpUsage,
            source_version: version,
            observed_at: now,
            provider_sample_at: None,
            sequence: sequence.saturating_add(1),
            start_sequence: Some(sequence),
            windows: parse(&text, &scope)?,
            complete: false,
            metadata: AccountMetadata {
                plan: tier.map(|name| PlanInfo {
                    name,
                    observed_at: now,
                    source: SourceKind::AntigravityAcpUsage,
                }),
                ..Default::default()
            },
        };
        validate_observation(&observation, now).map_err(RefreshError::Failed)?;
        Ok(observation)
    })
}

pub(super) fn init(service: &Entity<QuotaService>, project: &Entity<Project>, cx: &mut App) {
    sync(service, project, cx);
    let store = project.read(cx).agent_server_store().clone();
    let subscription = cx.subscribe(&store, {
        let service = service.downgrade();
        let project = project.downgrade();
        move |_, _: &AgentServersUpdated, cx| {
            if let (Some(service), Some(project)) = (service.upgrade(), project.upgrade()) {
                sync(&service, &project, cx);
            }
        }
    });
    service.update(cx, |service, _| service._subscriptions.push(subscription));
}
fn configuration_identity(cx: &App) -> Vec<String> {
    let mut identity = Vec::new();
    let settings = cx
        .global::<SettingsStore>()
        .get::<AllAgentServersSettings>(None);
    if let Some(CustomAgentServerSettings::Registry { env, .. }) = settings.get(AGENT) {
        let sorted: BTreeMap<_, _> = env.iter().collect();
        for (name, value) in sorted {
            identity.extend([name.clone(), value.clone()]);
        }
    }
    if let Some(RegistryAgent::Binary(registry)) =
        AgentRegistryStore::try_global(cx).and_then(|store| {
            store
                .read(cx)
                .agents()
                .iter()
                .find(|agent| agent.id().0.as_ref() == AGENT)
                .cloned()
        })
    {
        identity.push(registry.metadata.version.to_string());
        let platform = format!(
            "{}-{}",
            if cfg!(target_os = "macos") {
                "darwin"
            } else {
                std::env::consts::OS
            },
            std::env::consts::ARCH
        );
        if let Some(target) = registry.targets.get(&platform) {
            identity.extend([target.cmd.clone(), target.archive.clone()]);
            identity.extend(target.args.clone());
            let sorted: BTreeMap<_, _> = target.env.iter().collect();
            for (name, value) in sorted {
                identity.extend([name.clone(), value.clone()]);
            }
        }
    }
    identity
}
pub(super) fn sync(service: &Entity<QuotaService>, project: &Entity<Project>, cx: &mut App) {
    // This comparison is memory-only: environment values never enter a scope key or persistence.
    let identity = configuration_identity(cx);
    let status = project
        .read(cx)
        .agent_server_store()
        .clone()
        .update(cx, |store, cx| {
            store
                .get_external_agent(&AgentId::new(AGENT))
                .map(|agent| agent.install_status(cx))
        });
    service.update(cx, |service, cx| {
        service.antigravity_inventory = Some(cx.spawn(async move |this, cx| {
            let availability = match status {
                Some(status) => match status.await {
                    Ok(AgentInstallStatus::Installed { .. }) => Availability::Ready,
                    Ok(AgentInstallStatus::NotInstalled) => Availability::NotSetUp,
                    _ => Availability::Unsupported(ErrorCode::UnsupportedVersion),
                },
                None => Availability::NotInstalled,
            };
            let attempt = this
                .read_with(cx, |service, cx| {
                    service.ports.store.load(format!("attempt/{SCOPE}"), cx)
                })
                .map_err(|_| ());
            let attempt = match attempt {
                Ok(attempt) => attempt.await,
                Err(()) => return,
            };
            if this
                .update(cx, |service, cx| {
                    let scope = AccountScope {
                        category: crate::AccountCategory::ExternalAgent,
                        native_provider_id: None,
                        id: SCOPE.into(),
                        provider: ProviderId::Antigravity,
                        runtime: "local".into(),
                        identity_known: false,
                    };
                    log::debug!(
                        "crew_quota: inventory agent=antigravity installation={}",
                        installation_code(&availability)
                    );
                    if service.antigravity_identity.as_ref()
                        == Some(&(identity.clone(), availability.clone()))
                    {
                        return;
                    }
                    if service
                        .antigravity_identity
                        .as_ref()
                        .map(|(_, availability)| installation_code(availability))
                        != Some(installation_code(&availability))
                    {
                        log::info!(
                            "crew_quota: inventory agent=antigravity installation={}",
                            installation_code(&availability)
                        );
                    }
                    service.antigravity_identity = Some((identity, availability.clone()));
                    let generation = service
                        .core
                        .snapshot
                        .accounts
                        .get(SCOPE)
                        .map_or(1, |account| account.generation.saturating_add(1));
                    if let Some(cancellation) = service.cancellations.remove(SCOPE) {
                        cancellation.abort();
                    }
                    service.tasks.remove(SCOPE);
                    if service
                        .core
                        .inventory(scope.clone(), generation, availability)
                        .is_err()
                    {
                        service.persistence_failure(SCOPE.into(), cx);
                        return;
                    }
                    service
                        .scheduler
                        .track(SCOPE.into(), ProviderId::Antigravity, generation);
                    match attempt {
                        Ok(Some(record)) => {
                            match decode_attempt(&record, &scope, service.ports.clock.now()) {
                                Ok(attempt) => service.scheduler.restore_timing(
                                    SCOPE,
                                    Some(attempt.at),
                                    0,
                                    0,
                                    service.ports.clock.now(),
                                    service.ports.clock.monotonic(),
                                ),
                                Err(_) => service.persistence_failure(SCOPE.into(), cx),
                            }
                        }
                        Err(_) => service.persistence_failure(SCOPE.into(), cx),
                        Ok(None) => {}
                    }
                    if let Some(schedule) = service.scheduler.scopes.get(SCOPE) {
                        let now = service.ports.clock.now();
                        let next = Scheduler::next_for(schedule, now, service.ports.clock.monotonic());
                        if next > now {
                            log::info!(
                                "crew_quota: agent=Antigravity source=Active outcome=Deferred next_in={} reason=RestoredTiming",
                                next.saturating_sub(now)
                            );
                        }
                    }
                    cx.emit(QuotaEvent::AvailabilityChanged(scope.id));
                    service.wake();
                    cx.notify();
                })
                .is_err()
            {
                log::debug!("crew_quota: antigravity_service_closed");
            }
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    const DUMP: &str = include_str!("../../tests/fixtures/antigravity.sanitized.md");
    #[test]
    fn sanitized_live_grammar_and_optional_forms() -> anyhow::Result<()> {
        let windows = parse(DUMP, "scope").map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(windows.len(), 4);
        assert_eq!(
            windows.first().map(|window| &window.used_fraction),
            Some(&Patch::Set(Some(0.1)))
        );
        let mixed = DUMP
            .replace("90% remaining (resets <date-time>)", "Disabled: no access")
            .replace(
                "99% remaining (resets <date-time>)",
                "1,234 units remaining (resets Nov 04, 2026, 12:30 UTC)",
            );
        let windows = parse(&mixed, "scope").map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(
            windows.first().map(|window| &window.disabled),
            Some(&Patch::Set(true))
        );
        assert!(windows.get(1).is_some_and(
            |window| matches!(&window.remaining, Patch::Set(Some(amount)) if amount.value == 1234.0)
                && matches!(window.reset_at, Patch::Set(Some(_)))
        ));
        for malformed in [
            DUMP.replace("90%", "101%"),
            DUMP.replace("90% remaining", "1,23 units remaining"),
            DUMP.replace("  *(Models", "  (Models"),
            DUMP.replace("### Quota Limits", "### Wrong"),
            DUMP.replace("% remaining", " percent"),
        ] {
            assert!(parse(&malformed, "scope").is_err());
        }
        assert!(
            parse(
                &DUMP
                    .replace("Gemini Models", "Unknown")
                    .replace("Claude and GPT models", "Unknown"),
                "scope"
            )
            .is_err()
        );
        Ok(())
    }
    fn notification(update: Value) -> Envelope {
        Envelope {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some("session/update".into()),
            params: Some(json!({"sessionId":"owned", "update": update})),
            result: None,
            error: None,
        }
    }
    #[test]
    fn abort_gates_reject_activity_and_wrong_session() {
        for update in [
            json!({"sessionUpdate":"tool_call"}),
            json!({"sessionUpdate":"tool_call_update"}),
            json!({"sessionUpdate":"usage_update","used":1}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"image"}}),
            json!({"sessionUpdate":"auth_required"}),
            json!({"sessionUpdate":"elicitation"}),
        ] {
            assert!(Updates::default().accept(&notification(update)).is_err());
        }
        for method in [
            "session/request_permission",
            "terminal/create",
            "fs/read_text_file",
            "elicitation/create",
            "authenticate",
            "unknown",
        ] {
            let mut envelope = notification(json!({}));
            envelope.method = Some(method.into());
            envelope.id = Some(10);
            assert!(Updates::default().accept(&envelope).is_err());
        }
        let mut updates = Updates {
            session: Some("another".into()),
            ..Default::default()
        };
        assert!(
            updates
                .accept(&notification(
                    json!({"sessionUpdate":"usage_update","used":0})
                ))
                .is_err()
        );
    }
    #[gpui::test]
    async fn fake_connection_requires_matching_response(executor: gpui::BackgroundExecutor) {
        for input in [
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n".as_slice(),
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"terminal/create\",\"params\":{}}\n"
                .as_slice(),
            b"{\"id\":1,\"result\":{}}\n".as_slice(),
        ] {
            let mut reader = BufReader::new(futures::io::Cursor::new(input));
            let result = providers::deadline(
                &executor,
                TIMEOUT,
                response(
                    &mut reader,
                    1,
                    &mut Updates::default(),
                    &AtomicUsize::new(0),
                ),
            )
            .await;
            assert!(result.is_err());
        }
    }
    #[test]
    fn environment_overlay_contract() {
        let base = BTreeMap::from([
            ("HOME".into(), "/producer-home".into()),
            ("SHARED".into(), "base".into()),
        ]);
        let environment = compose_environment(
            base,
            [("SHARED".into(), "target".into())].into_iter(),
            [("SHARED".into(), "settings".into())].into_iter(),
        );
        assert_eq!(
            environment.get("SHARED").map(String::as_str),
            Some("settings")
        );
        assert_eq!(
            environment.get("HOME").map(String::as_str),
            Some("/producer-home")
        );
        assert!(!environment.contains_key("GEMINI_HOME"));
    }
    #[test]
    fn installed_tree_contract() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        assert!(installed(root.path(), "1.4.5", "./agy_acp_server.par").is_err());
        let version = root.path().join("v_1.4.5_archive_checksum");
        std::fs::create_dir(&version)?;
        std::fs::write(version.join("agy_acp_server.par"), "fixture")?;
        assert_eq!(
            installed(root.path(), "1.4.5", "./agy_acp_server.par").ok(),
            Some(version.join("agy_acp_server.par"))
        );
        assert!(installed(root.path(), "1.4.6", "./agy_acp_server.par").is_err());
        assert!(installed(root.path(), "1.4.5", "../agy_acp_server.par").is_err());
        let duplicate = root.path().join("v_1.4.5_other");
        std::fs::create_dir(&duplicate)?;
        std::fs::write(duplicate.join("agy_acp_server.par"), "fixture")?;
        assert!(installed(root.path(), "1.4.5", "./agy_acp_server.par").is_err());
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn exact_id_cleanup_preserves_other_sessions_and_rejects_symlinks() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let directory = files::Directory::open(&temporary.path().canonicalize()?)
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        for suffix in [".db", ".db-shm", ".db-wal", ".meta"] {
            std::fs::write(temporary.path().join(format!("owned{suffix}")), "")?;
            std::fs::write(temporary.path().join(format!("other{suffix}")), "")?;
        }
        assert!(
            directory
                .remove("owned", &BTreeSet::from(["owned.db".into()]))
                .is_err()
        );
        assert!(temporary.path().join("owned.db").exists());
        directory
            .remove("owned", &BTreeSet::new())
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        for suffix in [".db", ".db-shm", ".db-wal", ".meta"] {
            assert!(!temporary.path().join(format!("owned{suffix}")).exists());
            assert!(temporary.path().join(format!("other{suffix}")).exists());
        }
        std::os::unix::fs::symlink(
            temporary.path().join("other.db"),
            temporary.path().join("owned.db"),
        )?;
        assert!(directory.remove("owned", &BTreeSet::new()).is_err());
        assert!(directory.remove("../other", &BTreeSet::new()).is_err());
        assert!(temporary.path().join("other.db").exists());
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn recovery_never_unlinks_while_a_writer_group_is_live() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let conversations = temporary.path().join("conversations");
        std::fs::create_dir(&conversations)?;
        let conversations = conversations.canonicalize()?;
        std::fs::write(conversations.join("owned.db"), "must remain")?;
        let directory =
            files::Directory::open(&conversations).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let group = unsafe { libc::getpgrp() };
        let journal = Journal {
            session: Some("owned".into()),
            group,
            directory: conversations.clone(),
            identity: directory
                .identity()
                .map_err(|error| anyhow::anyhow!("{error:?}"))?,
            before: BTreeSet::new(),
        };
        save_journal(&temporary.path().join("pending-session.json"), &journal)
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let launch = Launch {
            command: PathBuf::from("/must-not-spawn"),
            args: Vec::new(),
            environment: BTreeMap::new(),
            directory: temporary.path().into(),
            conversations: conversations.clone(),
            version: "fixture".into(),
            timeout: TIMEOUT,
        };
        let executor = gpui::BackgroundExecutor::new(Arc::new(gpui::ThreadedDispatcher::new()));
        assert_eq!(smol::block_on(read(launch, executor)), Err(cleanup_error()));
        assert_eq!(
            std::fs::read_to_string(conversations.join("owned.db"))?,
            "must remain"
        );
        assert!(temporary.path().join("pending-session.json").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_without_session_preserves_unattributed_files() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let conversations = temporary.path().join("conversations");
        std::fs::create_dir(&conversations)?;
        let conversations = conversations.canonicalize()?;
        std::fs::write(conversations.join("owned.db"), "must remain")?;
        let directory =
            files::Directory::open(&conversations).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let group = i32::MAX;
        let journal = Journal {
            session: None,
            group,
            directory: conversations.clone(),
            identity: directory
                .identity()
                .map_err(|error| anyhow::anyhow!("{error:?}"))?,
            before: BTreeSet::new(),
        };
        save_journal(&temporary.path().join("pending-session.json"), &journal)
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let launch = Launch {
            command: PathBuf::from("/must-not-spawn"),
            args: Vec::new(),
            environment: BTreeMap::new(),
            directory: temporary.path().into(),
            conversations: conversations.clone(),
            version: "fixture".into(),
            timeout: TIMEOUT,
        };
        let executor = gpui::BackgroundExecutor::new(Arc::new(gpui::ThreadedDispatcher::new()));
        assert!(smol::block_on(read(launch, executor)).is_err());
        assert_eq!(
            std::fs::read_to_string(conversations.join("owned.db"))?,
            "must remain"
        );
        assert!(!temporary.path().join("pending-session.json").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn synthetic_process_lifecycle() -> anyhow::Result<()> {
        let executor = gpui::BackgroundExecutor::new(Arc::new(gpui::ThreadedDispatcher::new()));
        for mode in ["agy-success", "agy-tool", "agy-timeout"] {
            let temporary = tempfile::tempdir()?;
            let conversations = temporary.path().join("conversations");
            std::fs::create_dir(&conversations)?;
            std::fs::write(conversations.join("other.db"), "untouched")?;
            let binary = std::env::current_exe()?
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| anyhow::anyhow!("missing target"))?
                .join("crew_quota_process_fixture");
            let launch = Launch {
                command: binary,
                args: Vec::new(),
                environment: BTreeMap::from([
                    (
                        "CREW_QUOTA_FIXTURE_DIRECTORY".into(),
                        temporary.path().to_string_lossy().into_owned(),
                    ),
                    ("CREW_QUOTA_FIXTURE_MODE".into(), mode.into()),
                ]),
                directory: temporary.path().into(),
                conversations: conversations.canonicalize()?,
                version: "fixture".into(),
                timeout: if mode == "agy-timeout" {
                    Duration::from_millis(800)
                } else {
                    Duration::from_secs(5)
                },
            };
            let result = smol::block_on(read(launch, executor.clone()));
            if mode == "agy-success" {
                assert!(
                    result.is_ok(),
                    "{result:?}, closed={}, journal={}, owned={}",
                    temporary.path().join("closed").exists(),
                    temporary.path().join("pending-session.json").exists(),
                    conversations.join("fixture-owned-session.db").exists()
                );
            } else {
                assert!(result.is_err());
            }
            assert_eq!(
                std::fs::read_to_string(conversations.join("other.db"))?,
                "untouched"
            );
            assert!(!conversations.join("fixture-owned-session.db").exists());
            assert!(!temporary.path().join("pending-session.json").exists());
            for file in ["launcher.pid", "descendant.pid"] {
                let identifier: i32 =
                    std::fs::read_to_string(temporary.path().join(file))?.parse()?;
                assert!(unsafe { libc::kill(identifier, 0) } != 0);
            }
        }
        Ok(())
    }
}
