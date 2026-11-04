//! Captured schedule scripts and their transient, workspace-owned approvals.
//! Script consent never changes the Main agent's execution policy.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tokio::sync::oneshot;
use ts_rs::TS;

use crate::assistant::sandbox::filesystem::EffectiveFilesystemPolicy;
use crate::assistant::sandbox::{
    SandboxCommand, SandboxEnv, SandboxNetworkMode, SandboxProfile, SandboxSessionBusMode,
};
use crate::assistant::tools::local::{evaluate_command_policy, PolicyResult};
use crate::assistant::tools::RunCommandGrant;
use crate::config::{
    ExecutionCapabilityConfig, SandboxNetworkConfig, SandboxSessionBusConfig, ShellAccessMode,
};
use crate::AppState;

const SCRIPT_LIMIT: u64 = 128 * 1024;
pub const OUTPUT_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(export, export_to = "bindings.ts")]
pub enum ScriptSource {
    Inline { content: String },
    File { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct ScriptConfig {
    pub source: ScriptSource,
    /// One executable, without arguments. The selected language determines -c/-e.
    pub interpreter: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}
fn default_timeout() -> u32 {
    30
}

impl ScriptConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.timeout_seconds == 0 {
            return Err("Script timeout must be positive.".into());
        }
        interpreter_flag(&self.interpreter)?;
        if matches!(&self.source, ScriptSource::File { path } if path.trim().is_empty()) {
            return Err("Select a condition script file.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct ScriptApproval {
    pub id: String,
    pub purpose: String,
    pub content: String,
    pub interpreter: String,
    pub timeout_seconds: u32,
    pub access: String,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct ScriptResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub error: Option<String>,
}

impl ScriptResult {
    pub fn condition_allows(&self) -> bool {
        self.error.is_none() && self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct ScriptStatus {
    pub pending: Option<ScriptApproval>,
    pub result: Option<ScriptResult>,
    pub error: Option<String>,
    pub running: bool,
}

#[derive(Clone)]
pub(crate) struct CapturedScript {
    pub config: ScriptConfig,
    pub content: String,
    pub interpreter: PathBuf,
    pub access: String,
    identity: String,
    invocation: String,
    profile: SandboxProfile,
}

/// Backend-only, versioned consent. It is never accepted from an IPC payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptConsent {
    version: u8,
    identity: String,
}

impl ScriptConsent {
    pub(crate) fn allows(&self, snapshot: &CapturedScript) -> bool {
        self.version == 1 && self.identity == snapshot.identity
    }
}

pub(crate) struct PendingScript {
    pub approval: ScriptApproval,
    pub snapshot: CapturedScript,
    pub decision: Option<oneshot::Sender<Option<CapturedScript>>>,
}

#[derive(Default)]
pub(crate) struct ScriptRuntime {
    pub pending: Option<PendingScript>,
    pub result: Option<ScriptResult>,
    pub error: Option<String>,
    pub confirmed: Option<(String, CapturedScript)>,
    /// Includes Run now tests, which do not create a model run.
    pub running: bool,
}

impl ScriptRuntime {
    pub fn status(&self) -> ScriptStatus {
        ScriptStatus {
            pending: self.pending.as_ref().map(|p| p.approval.clone()),
            result: self.result.clone(),
            error: self.error.clone(),
            running: self.running,
        }
    }
    pub fn request(
        &mut self,
        snapshot: CapturedScript,
        purpose: &str,
        decision: Option<oneshot::Sender<Option<CapturedScript>>>,
    ) -> ScriptApproval {
        let approval = ScriptApproval {
            id: uuid::Uuid::new_v4().to_string(),
            purpose: purpose.into(),
            content: snapshot.content.clone(),
            interpreter: snapshot.interpreter.display().to_string(),
            timeout_seconds: snapshot.config.timeout_seconds,
            access: snapshot.access.clone(),
        };
        self.pending = Some(PendingScript {
            approval: approval.clone(),
            snapshot,
            decision,
        });
        self.error = None;
        approval
    }
}

fn interpreter_flag(interpreter: &str) -> Result<&'static str, String> {
    match Path::new(interpreter)
        .file_name()
        .and_then(|name| name.to_str())
    {
        Some("sh" | "bash" | "zsh" | "python" | "python3") => Ok("-c"),
        Some("node") => Ok("-e"),
        _ => {
            Err("Choose sh, bash, zsh, python, python3 or node (a name or executable path).".into())
        }
    }
}

fn resolve_interpreter(interpreter: &str) -> Result<PathBuf, String> {
    let path = Path::new(interpreter);
    let candidates = if path.is_absolute() {
        vec![path.to_path_buf()]
    } else {
        std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
        )
        .map(|dir| dir.join(path))
        .collect()
    };
    candidates
        .into_iter()
        .find_map(|path| {
            let path = path.canonicalize().ok()?;
            let meta = path.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            Some(path)
        })
        .ok_or_else(|| format!("Interpreter is unavailable: {interpreter}"))
}

fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

pub(crate) fn invocation_policy(
    execution: &ExecutionCapabilityConfig,
    invocation: &str,
    approved: bool,
) -> Result<(), String> {
    if execution.shell.mode == ShellAccessMode::Off {
        return Err("Shell access is disabled for the Main agent.".into());
    }
    match evaluate_command_policy(execution, invocation, &[], &[]) {
        PolicyResult::Block {
            segment_text,
            matched_prefix,
        } => Err(format!(
            "Script invocation `{segment_text}` is blocked by `{matched_prefix}`."
        )),
        PolicyResult::NeedsApproval(rows) if approved => {
            let grants: Vec<_> = rows
                .into_iter()
                .map(|row| RunCommandGrant::Exact(row.text))
                .collect();
            match evaluate_command_policy(execution, invocation, &grants, &[]) {
                PolicyResult::Allow => Ok(()),
                _ => Err(
                    "The script invocation is not permitted by the Main agent's current policy."
                        .into(),
                ),
            }
        }
        _ => Ok(()), // A captured request will obtain explicit consent before execution.
    }
}

pub(crate) fn capture(
    state: &AppState,
    workspace_id: &str,
    script: &ScriptConfig,
) -> Result<CapturedScript, String> {
    let root = state
        .workspace_root(workspace_id)
        .ok_or("Workspace not found.")?;
    let (workspace, roster) = state.resolve_workspace_roster(workspace_id)?;
    capture_with_roster(&root, &workspace, &roster, script)
}

fn capture_with_roster(
    root: &Path,
    workspace: &crate::config::WorkspaceConfig,
    roster: &[crate::config::global_agents::ResolvedAgent],
    script: &ScriptConfig,
) -> Result<CapturedScript, String> {
    let main = roster
        .iter()
        .find(|row| row.agent.id == workspace.main_agent_id())
        .ok_or("Workspace Main agent not found.")?;
    let mut captured = capture_with_execution(root, &main.agent.execution, script)?;
    captured.identity = serde_json::to_string(&(workspace.main_agent_id(), captured.identity))
        .map_err(|error| error.to_string())?;
    captured.access = format!(
        "Main agent: {}\n{}",
        workspace.main_agent_id(),
        captured.access
    );
    Ok(captured)
}

/// Capture against the configuration held by the existing atomic update boundary.
pub(crate) fn capture_for_config(
    state: &AppState,
    workspace: &crate::config::WorkspaceConfig,
    script: &ScriptConfig,
) -> Result<CapturedScript, String> {
    let root = state
        .workspace_root(&workspace.id)
        .ok_or("Workspace not found.")?;
    let (_, roster) = state.resolve_roster_for(workspace.clone())?;
    capture_with_roster(&root, workspace, &roster, script)
}

pub(crate) fn saved_consent(
    state: &AppState,
    workspace: &crate::config::WorkspaceConfig,
    condition: &Option<ScriptConfig>,
    unattended: Option<bool>,
    confirmed: Option<&CapturedScript>,
) -> Result<Option<ScriptConsent>, String> {
    if unattended == Some(false) || condition.is_none() {
        return Ok(None);
    }
    if unattended.is_none() {
        return Ok((workspace.schedule.condition == *condition)
            .then(|| workspace.schedule.condition_consent.clone())
            .flatten());
    }
    let current = capture_for_config(state, workspace, condition.as_ref().unwrap())?;
    let approved = confirmed.is_some_and(|snapshot| {
        snapshot.config == current.config && snapshot.same_approval(&current)
    }) || workspace
        .schedule
        .condition_consent
        .as_ref()
        .is_some_and(|consent| consent.allows(&current));
    if !approved {
        return Err(
            "Review and explicitly approve this condition for unattended execution.".into(),
        );
    }
    Ok(Some(current.consent()))
}

fn capture_with_execution(
    root: &Path,
    execution: &ExecutionCapabilityConfig,
    script: &ScriptConfig,
) -> Result<CapturedScript, String> {
    use std::io::Read;
    script.validate()?;
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err("Schedule scripts are unavailable: filesystem and network restrictions cannot be enforced on this platform.".into());
    }
    let content = match &script.source {
        ScriptSource::Inline { content } => content.clone(),
        ScriptSource::File { path } => {
            let path = Path::new(path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let file = options
                .open(&path)
                .map_err(|e| format!("Cannot read script {}: {e}", path.display()))?;
            if !file.metadata().map_err(|e| e.to_string())?.is_file() {
                return Err("Script input must be a regular file.".into());
            }
            let mut content = String::new();
            file.take(SCRIPT_LIMIT + 1)
                .read_to_string(&mut content)
                .map_err(|e| format!("Cannot read script as UTF-8: {e}"))?;
            content
        }
    };
    if content.len() as u64 > SCRIPT_LIMIT {
        return Err("Script is larger than 128 KiB.".into());
    }
    let interpreter = resolve_interpreter(&script.interpreter)?;
    let flag = interpreter_flag(&script.interpreter)?;
    let invocation = format!(
        "{} {} {}",
        quote(&interpreter.display().to_string()),
        flag,
        quote(&content)
    );
    invocation_policy(execution, &invocation, false)?;
    let filesystem = EffectiveFilesystemPolicy::from_config(
        root,
        &execution.filesystem.extra_paths,
        crate::paths::real_home().as_deref(),
    )?;
    let session_bus = match execution.sandbox.session_bus {
        SandboxSessionBusConfig::Deny => SandboxSessionBusMode::Deny,
        SandboxSessionBusConfig::Allow => SandboxSessionBusMode::Allow,
    };
    let network = match execution.sandbox.network {
        SandboxNetworkConfig::Enabled => SandboxNetworkMode::Host,
        SandboxNetworkConfig::Disabled => SandboxNetworkMode::Disabled,
    };
    let env_home = filesystem
        .home
        .as_deref()
        .unwrap_or(&filesystem.workspace_root);
    let profile = SandboxProfile {
        env: SandboxEnv::filtered_from_current(env_home, session_bus),
        scratch_tmp: None,
        filesystem,
        network,
        session_bus,
    };
    let extra_paths = profile
        .filesystem
        .path_grants
        .iter()
        .map(|grant| {
            let access = match grant.access {
                crate::assistant::sandbox::SandboxPathAccess::ReadOnly => "Read only",
                crate::assistant::sandbox::SandboxPathAccess::ReadWrite => "Read/write",
            };
            format!("{access}: {}", grant.host_path.display())
        })
        .collect::<Vec<_>>()
        .join("\n");
    let access = format!(
        "Main shell: {:?}\nWorkspace (read/write): {}\nHome (read only): {}\nAdditional paths:\n{}\nNetwork: {:?}\nSession bus: {:?}\nAllowed invocation prefixes: {:?}\nBlocked invocation prefixes: {:?}\nSibling workspaces remain isolated.\nThe existing invocation checks do not inspect every command reached inside an arbitrary interpreter. Full shell mode permits broader invocations; the filesystem and network sandbox still applies. Scripts can read changing files and services.",
        execution.shell.mode, profile.filesystem.workspace_root.display(), env_home_display(&profile),
        if extra_paths.is_empty() { "None" } else { &extra_paths }, execution.sandbox.network, execution.sandbox.session_bus,
        execution.shell.allowed_command_prefixes, execution.shell.blocked_command_prefixes
    );
    let identity = serde_json::to_string(&(
        content.as_str(),
        &interpreter,
        script.timeout_seconds,
        &script.interpreter,
        &access,
        &profile.filesystem.mask,
        execution,
        profile.env.iter().collect::<Vec<_>>(),
    ))
    .map_err(|e| e.to_string())?;
    Ok(CapturedScript {
        config: script.clone(),
        content,
        interpreter,
        access,
        identity,
        invocation,
        profile,
    })
}

fn env_home_display(profile: &SandboxProfile) -> String {
    profile
        .filesystem
        .home
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".into())
}

impl CapturedScript {
    pub(crate) fn consent(&self) -> ScriptConsent {
        ScriptConsent {
            version: 1,
            identity: self.identity.clone(),
        }
    }
    pub fn same_approval(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
    pub async fn execute(self, state: &AppState, workspace_id: &str) -> ScriptResult {
        let started = std::time::Instant::now();
        let result = async {
            let current = capture(state, workspace_id, &self.config)?;
            if !self.same_approval(&current) { return Err("Script contents, interpreter or effective access changed. Review a fresh approval.".to_string()); }
            invocation_policy(&current_execution(state, workspace_id)?, &self.invocation, true)?;
            self.run().await
        }.await;
        script_result(result, started)
    }
    async fn run(self) -> Result<crate::assistant::sandbox::SandboxCommandOutput, String> {
        let flag = interpreter_flag(&self.config.interpreter)?;
        crate::assistant::sandbox::run_command(SandboxCommand {
            argv: vec![
                self.interpreter.into_os_string(),
                OsString::from(flag),
                self.content.into(),
            ],
            cwd: self.profile.filesystem.workspace_root.clone(),
            timeout_ms: u64::from(self.config.timeout_seconds) * 1000,
            max_output_chars: OUTPUT_LIMIT,
            profile: self.profile,
        })
        .await
    }
}

fn current_execution(
    state: &AppState,
    workspace_id: &str,
) -> Result<ExecutionCapabilityConfig, String> {
    let (workspace, roster) = state.resolve_workspace_roster(workspace_id)?;
    roster
        .into_iter()
        .find(|row| row.agent.id == workspace.main_agent_id())
        .map(|row| row.agent.execution)
        .ok_or("Workspace Main agent not found.".into())
}

fn script_result(
    result: Result<crate::assistant::sandbox::SandboxCommandOutput, String>,
    started: std::time::Instant,
) -> ScriptResult {
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    match result {
        Ok(output) => {
            let error = if output.timed_out {
                Some(output.stderr.clone())
            } else {
                match output.exit_code {
                    Some(0 | 1) => None,
                    Some(code) => Some(format!("Condition exited with status {code}.")),
                    None => Some("Condition terminated without an exit status (signal).".into()),
                }
            };
            ScriptResult {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: output.exit_code,
                duration_ms,
                error,
                timed_out: output.timed_out,
            }
        }
        Err(error) => ScriptResult {
            duration_ms,
            timed_out: error.contains("timed out"),
            error: Some(error),
            ..Default::default()
        },
    }
}

pub(crate) enum ConditionOutcome {
    Allow,
    Skip,
    Abandoned,
}

/// A queued Main run holds the session while consent or the process is active.
/// Manual input invalidates this lease; it starts only after this function returns.
pub(crate) async fn check_condition(
    state: &AppState,
    workspace_id: &str,
    pool: &crate::db::DbPool,
    session_id: &str,
    lease: &crate::agents::scheduler::DispatchLease,
) -> ConditionOutcome {
    condition_with_executor(state, workspace_id, pool, session_id, lease, |snapshot| {
        snapshot.execute(state, workspace_id)
    })
    .await
}

async fn condition_with_executor<F, Fut>(
    state: &AppState,
    workspace_id: &str,
    pool: &crate::db::DbPool,
    session_id: &str,
    lease: &crate::agents::scheduler::DispatchLease,
    execute: F,
) -> ConditionOutcome
where
    F: FnOnce(CapturedScript) -> Fut,
    Fut: std::future::Future<Output = ScriptResult>,
{
    let result = condition_attempt(
        state,
        workspace_id,
        pool,
        session_id,
        lease,
        execute,
        || chrono::Utc::now().timestamp_millis(),
    )
    .await;
    match result {
        Ok(outcome) => outcome,
        Err(error) => {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.pending = None;
            runtime.running = false;
            runtime.error = Some(error);
            ConditionOutcome::Skip
        }
    }
}

async fn condition_attempt<F, Fut>(
    state: &AppState,
    workspace_id: &str,
    pool: &crate::db::DbPool,
    session_id: &str,
    lease: &crate::agents::scheduler::DispatchLease,
    execute: F,
    clock: impl Fn() -> i64,
) -> Result<ConditionOutcome, String>
where
    F: FnOnce(CapturedScript) -> Fut,
    Fut: std::future::Future<Output = ScriptResult>,
{
    let (workspace, _) = state.resolve_workspace_roster(workspace_id)?;
    let Some(script) = workspace.schedule.condition else {
        return Ok(ConditionOutcome::Allow);
    };
    let mut snapshot = capture(state, workspace_id, &script)?;
    loop {
        if !condition_eligible(pool, session_id, lease, clock()).await? {
            return Ok(ConditionOutcome::Abandoned);
        }
        let (current_workspace, _) = state.resolve_workspace_roster(workspace_id)?;
        let authorized = current_workspace
            .schedule
            .condition_consent
            .as_ref()
            .is_some_and(|consent| consent.allows(&snapshot));
        if !authorized {
            let (send, mut receive) = oneshot::channel();
            {
                let mut scheduler = state.scheduler.lock().await;
                let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
                runtime.request(snapshot.clone(), "scheduled", Some(send));
            }
            let approved = loop {
                tokio::select! {
                    decision = &mut receive => break decision.unwrap_or(None),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        if !condition_eligible(pool, session_id, lease, clock()).await? {
                            state.scheduler.lock().await.scripts.entry(workspace_id.into()).or_default().pending = None;
                            return Ok(ConditionOutcome::Abandoned);
                        }
                    }
                }
            };
            let Some(approved) = approved else {
                return Ok(ConditionOutcome::Skip);
            };
            snapshot = approved;
        }
        if !condition_eligible(pool, session_id, lease, clock()).await? {
            return Ok(ConditionOutcome::Abandoned);
        }
        let current = capture(state, workspace_id, &script)?;
        if !snapshot.same_approval(&current) {
            snapshot = current;
            continue; // One replacement request, never a backlog per interval.
        }
        state
            .scheduler
            .lock()
            .await
            .scripts
            .entry(workspace_id.into())
            .or_default()
            .running = true;
        let result = execute(snapshot).await;
        {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.running = false;
            runtime.error = result.error.clone();
            runtime.result = Some(result.clone());
        }
        if !condition_eligible(pool, session_id, lease, clock()).await? {
            return Ok(ConditionOutcome::Abandoned);
        }
        return Ok(if result.condition_allows() {
            ConditionOutcome::Allow
        } else {
            ConditionOutcome::Skip
        });
    }
}

async fn condition_eligible(
    pool: &crate::db::DbPool,
    session_id: &str,
    lease: &crate::agents::scheduler::DispatchLease,
    wall_ms: i64,
) -> Result<bool, String> {
    Ok(lease.is_current_at(wall_ms)
        && !crate::assistant::repository::has_pending_user_message(pool, session_id).await?)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use crate::assistant::repository::{self, CreateRunParams};
    use crate::assistant::types::{
        AssistantRun, AssistantSession, RunStatus, RunTrigger, SessionContext,
    };
    use crate::config::{workspace_config, AppConfig, ConfigManager, WorkspaceConfig};
    use crate::workspace_index::WorkspaceIndex;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    struct Fixture {
        _tmp: tempfile::TempDir,
        state: Arc<AppState>,
        pool: crate::db::DbPool,
        root: PathBuf,
        session: AssistantSession,
        run: AssistantRun,
        lease: crate::agents::scheduler::DispatchLease,
    }
    fn inline(content: &str) -> ScriptConfig {
        ScriptConfig {
            source: ScriptSource::Inline {
                content: content.into(),
            },
            interpreter: "/bin/sh".into(),
            timeout_seconds: 30,
        }
    }
    fn execution() -> ExecutionCapabilityConfig {
        let mut execution = ExecutionCapabilityConfig::default();
        execution.shell.mode = ShellAccessMode::Full;
        execution.shell.blocked_command_prefixes.clear();
        execution
    }
    async fn fixture(script: Option<ScriptConfig>) -> Fixture {
        fixture_at(script, None, 0).await
    }
    async fn fixture_at(
        script: Option<ScriptConfig>,
        blocks: Option<crate::agents::weekly_blocks::WeeklyAllowedBlocks>,
        wall_ms: i64,
    ) -> Fixture {
        let (tmp, pool) = crate::db::test_support::workspace_pool().await;
        let root = tmp.path().to_path_buf();
        let mut workspace = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        workspace.main_agent.as_mut().unwrap().execution = execution();
        workspace.schedule.enabled = true;
        workspace.schedule.kind = crate::config::workspace_config::ScheduleKind::EnhancedInterval {
            interval_seconds: 15,
        };
        workspace.schedule.condition = script;
        workspace_config::save(&root, &workspace).unwrap();
        let mut index = WorkspaceIndex::default();
        index.insert_config(root.clone(), &workspace);
        let manager = ConfigManager::new_for_tests(AppConfig::default(), root.join("app.json"));
        let state = Arc::new(AppState::new_for_tests(manager, index).unwrap());
        let lease = {
            let mut scheduler = state.scheduler.lock().await;
            let start = Instant::now() - Duration::from_secs(30);
            scheduler.set_wake_monitor_available(true, 0, start);
            scheduler.register_definition(
                crate::agents::AgentDefinition::new("main", "Main").with_workspace_id("ws"),
            );
            scheduler.create_instance("main").unwrap();
            scheduler
                .configure_enhanced("main", workspace.schedule.kind.clone(), wall_ms, start)
                .unwrap();
            scheduler.configure_allowed_blocks("main", blocks).unwrap();
            assert_eq!(
                scheduler
                    .next_ready_at(wall_ms + 15_000, start + Duration::from_secs(15))
                    .as_deref(),
                Some("main")
            );
            scheduler.take_claimed_dispatch_lease("main").unwrap()
        };
        let session = repository::create_session(&pool, SessionContext::default())
            .await
            .unwrap();
        let run = repository::create_run(
            &pool,
            CreateRunParams {
                session_id: session.id.clone(),
                status: RunStatus::Queued,
                trigger: RunTrigger::Scheduled,
                connection_id: "conn".into(),
                protocol_id: "openai".into(),
                model_id: "model".into(),
                error: None,
            },
        )
        .await
        .unwrap();
        Fixture {
            _tmp: tmp,
            state,
            pool,
            root,
            session,
            run,
            lease,
        }
    }
    async fn pending(state: &AppState) -> ScriptApproval {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(approval) = state.scheduler.lock().await.script_status("ws").pending {
                    return approval;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    async fn respond(f: &Fixture, run: bool) {
        let approval = pending(&f.state).await;
        crate::commands::schedule_scripts::resolve_approval(
            &f.state,
            &f.pool,
            "ws",
            &approval.id,
            run,
        )
        .await
        .unwrap();
    }

    async fn approve_unattended(f: &Fixture, script: &ScriptConfig) -> String {
        let snapshot = capture(&f.state, "ws", script).unwrap();
        let approval = f
            .state
            .scheduler
            .lock()
            .await
            .scripts
            .entry("ws".into())
            .or_default()
            .request(snapshot, "unattended", None);
        crate::commands::schedule_scripts::authorize_condition(&f.state, "ws", &approval.id)
            .await
            .unwrap();
        let confirmed = f.state.scheduler.lock().await.scripts["ws"]
            .confirmed
            .as_ref()
            .unwrap()
            .1
            .clone();
        f.state
            .update_workspace_config("ws", |workspace| {
                workspace.schedule.condition_consent = saved_consent(
                    &f.state,
                    workspace,
                    &Some(script.clone()),
                    Some(true),
                    Some(&confirmed),
                )?;
                workspace.schedule.condition = Some(script.clone());
                Ok(())
            })
            .unwrap();
        approval.id
    }

    #[tokio::test]
    async fn unattended_consent_survives_reload_without_transient_approval() {
        let script = inline("exit 0");
        let f = fixture(Some(script.clone())).await;
        approve_unattended(&f, &script).await;
        let saved = workspace_config::load(&f.root).unwrap();
        assert!(saved
            .schedule
            .condition_consent
            .as_ref()
            .unwrap()
            .allows(&capture(&f.state, "ws", &script).unwrap()));
        // Restart loses all pending/confirmed runtime state; saved workspace consent remains.
        f.state.scheduler.lock().await.scripts.clear();
        let outcome = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |snapshot| async move {
                assert_eq!(snapshot.content, "exit 0");
                ScriptResult {
                    exit_code: Some(0),
                    ..Default::default()
                }
            },
        )
        .await;
        assert!(matches!(outcome, ConditionOutcome::Allow));
        assert!(f
            .state
            .scheduler
            .lock()
            .await
            .script_status("ws")
            .pending
            .is_none());
    }

    #[tokio::test]
    async fn unattended_file_change_waits_once_and_renewal_persists_exact_new_bytes() {
        let f = fixture(None).await;
        let path = f.root.join("check.sh");
        std::fs::write(&path, "exit 1").unwrap();
        let script = ScriptConfig {
            source: ScriptSource::File {
                path: "check.sh".into(),
            },
            ..inline("")
        };
        approve_unattended(&f, &script).await;
        std::fs::write(&path, "exit 0").unwrap();
        let attempt = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |snapshot| async move {
                assert_eq!(snapshot.content, "exit 0");
                ScriptResult {
                    exit_code: Some(0),
                    ..Default::default()
                }
            },
        );
        let renew = async {
            let approval = pending(&f.state).await;
            tokio::time::sleep(Duration::from_millis(220)).await;
            assert_eq!(pending(&f.state).await.id, approval.id);
            crate::commands::schedule_scripts::authorize_condition(&f.state, "ws", &approval.id)
                .await
                .unwrap();
        };
        let (outcome, ()) = tokio::join!(attempt, renew);
        assert!(matches!(outcome, ConditionOutcome::Allow));
        let saved = workspace_config::load(&f.root).unwrap();
        assert!(saved
            .schedule
            .condition_consent
            .unwrap()
            .allows(&capture(&f.state, "ws", &script).unwrap()));
    }

    #[tokio::test]
    async fn main_or_effective_access_change_invalidates_confirmed_and_saved_consent() {
        for change_main in [false, true] {
            let script = inline("exit 0");
            let f = fixture(Some(script.clone())).await;
            approve_unattended(&f, &script).await;
            let before = capture(&f.state, "ws", &script).unwrap();
            f.state
                .update_workspace_config("ws", |workspace| {
                    if change_main {
                        workspace.main_agent.as_mut().unwrap().id = "replacement-main".into();
                    } else {
                        workspace
                            .main_agent
                            .as_mut()
                            .unwrap()
                            .execution
                            .sandbox
                            .network = SandboxNetworkConfig::Disabled;
                    }
                    Ok(())
                })
                .unwrap();
            let workspace = workspace_config::load(&f.root).unwrap();
            assert!(!workspace
                .schedule
                .condition_consent
                .as_ref()
                .unwrap()
                .allows(&capture(&f.state, "ws", &script).unwrap()));
            assert!(saved_consent(
                &f.state,
                &workspace,
                &Some(script),
                Some(true),
                Some(&before)
            )
            .is_err());
        }
    }

    #[tokio::test]
    async fn clearing_unattended_restores_per_run_and_run_now_still_confirms() {
        let script = inline("exit 0");
        let f = fixture(Some(script.clone())).await;
        approve_unattended(&f, &script).await;
        let test = capture(&f.state, "ws", &script).unwrap();
        let request = f
            .state
            .scheduler
            .lock()
            .await
            .scripts
            .entry("ws".into())
            .or_default()
            .request(test, "test", None);
        assert!(crate::commands::schedule_scripts::authorize_condition(
            &f.state,
            "ws",
            &request.id
        )
        .await
        .is_err());
        assert_eq!(pending(&f.state).await.id, request.id);
        crate::commands::schedule_scripts::resolve_approval(
            &f.state,
            &f.pool,
            "ws",
            &request.id,
            false,
        )
        .await
        .unwrap();
        f.state
            .update_workspace_config("ws", |workspace| {
                workspace.schedule.condition_consent = saved_consent(
                    &f.state,
                    workspace,
                    &Some(script.clone()),
                    Some(false),
                    None,
                )?;
                Ok(())
            })
            .unwrap();
        let attempt = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |_| async { panic!("cleared consent executed") },
        );
        let (outcome, ()) = tokio::join!(attempt, respond(&f, false));
        assert!(matches!(outcome, ConditionOutcome::Skip));
    }

    #[tokio::test]
    async fn missing_authorized_file_or_interpreter_blocks_without_execution() {
        for missing_file in [true, false] {
            let f = fixture(None).await;
            let path = f.root.join("check.sh");
            std::fs::write(&path, "exit 0").unwrap();
            let script = ScriptConfig {
                source: ScriptSource::File {
                    path: "check.sh".into(),
                },
                ..inline("")
            };
            approve_unattended(&f, &script).await;
            if missing_file {
                std::fs::remove_file(&path).unwrap();
            } else {
                f.state
                    .update_workspace_config("ws", |workspace| {
                        workspace.schedule.condition.as_mut().unwrap().interpreter =
                            "/missing/sh".into();
                        Ok(())
                    })
                    .unwrap();
            }
            let outcome = condition_with_executor(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.lease,
                |_| async { panic!("unavailable script executed") },
            )
            .await;
            assert!(matches!(outcome, ConditionOutcome::Skip));
            assert!(f
                .state
                .scheduler
                .lock()
                .await
                .script_status("ws")
                .error
                .is_some());
        }
    }

    #[tokio::test]
    async fn disabled_authorized_schedule_does_not_execute() {
        let script = inline("exit 0");
        let f = fixture(Some(script.clone())).await;
        approve_unattended(&f, &script).await;
        f.state
            .scheduler
            .lock()
            .await
            .set_instance_enabled("main", false);
        let outcome = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |_| async { panic!("disabled schedule executed") },
        )
        .await;
        assert!(matches!(outcome, ConditionOutcome::Abandoned));
    }

    #[tokio::test]
    async fn file_replaced_after_authorization_is_rejected_before_process_execution() {
        let f = fixture(None).await;
        let path = f.root.join("check.sh");
        std::fs::write(&path, "exit 0").unwrap();
        let script = ScriptConfig {
            source: ScriptSource::File {
                path: "check.sh".into(),
            },
            ..inline("")
        };
        approve_unattended(&f, &script).await;
        let snapshot = capture(&f.state, "ws", &script).unwrap();
        std::fs::write(&path, "exit 1").unwrap();
        let result = snapshot.execute(&f.state, "ws").await;
        assert!(result.error.as_deref().unwrap().contains("changed"));
        assert!(result.exit_code.is_none());
    }

    #[tokio::test]
    async fn draft_consent_requires_explicit_authorize_and_revalidation_at_atomic_save() {
        let script = inline("exit 0");
        let f = fixture(Some(script.clone())).await;
        let snapshot = capture(&f.state, "ws", &script).unwrap();
        let request = f
            .state
            .scheduler
            .lock()
            .await
            .scripts
            .entry("ws".into())
            .or_default()
            .request(snapshot, "unattended", None);
        assert!(crate::commands::schedule_scripts::resolve_approval(
            &f.state,
            &f.pool,
            "ws",
            &request.id,
            true
        )
        .await
        .is_err());
        crate::commands::schedule_scripts::authorize_condition(&f.state, "ws", &request.id)
            .await
            .unwrap();
        assert!(workspace_config::load(&f.root)
            .unwrap()
            .schedule
            .condition_consent
            .is_none());
        let confirmed = f.state.scheduler.lock().await.scripts["ws"]
            .confirmed
            .as_ref()
            .unwrap()
            .1
            .clone();
        let workspace = workspace_config::load(&f.root).unwrap();
        assert!(saved_consent(
            &f.state,
            &workspace,
            &Some(inline("exit 1")),
            Some(true),
            Some(&confirmed)
        )
        .is_err());
        assert!(saved_consent(
            &f.state,
            &workspace,
            &Some(ScriptConfig {
                interpreter: "sh".into(),
                ..script
            }),
            Some(true),
            Some(&confirmed)
        )
        .is_err());
    }

    #[tokio::test]
    async fn approved_outcomes_gate_real_queued_turn_without_false_or_failed_model_calls() {
        for (exit_code, error, calls) in [
            (Some(0), None, 1),
            (Some(1), None, 0),
            (Some(2), Some("broken".into()), 0),
            (None, Some("signal".into()), 0),
            (None, Some("timed out".into()), 0),
        ] {
            let f = fixture(Some(inline("check work"))).await;
            let model_calls = AtomicUsize::new(0);
            let script_calls = AtomicUsize::new(0);
            let attempt = crate::agents::runner::run_condition_then(
                condition_with_executor(
                    &f.state,
                    "ws",
                    &f.pool,
                    &f.session.id,
                    &f.lease,
                    |_| async {
                        script_calls.fetch_add(1, Ordering::SeqCst);
                        ScriptResult {
                            exit_code,
                            error,
                            ..Default::default()
                        }
                    },
                ),
                &f.pool,
                &f.run.id,
                |_| {},
                || async {
                    model_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            );
            let (_, ()) = tokio::join!(attempt, respond(&f, true));
            assert_eq!(model_calls.load(Ordering::SeqCst), calls);
            assert_eq!(script_calls.load(Ordering::SeqCst), 1);
            if calls == 0 {
                assert!(repository::active_run_id(&f.pool, &f.session.id)
                    .await
                    .unwrap()
                    .is_none());
            }
            assert!(f
                .state
                .scheduler
                .lock()
                .await
                .script_status("ws")
                .pending
                .is_none());
        }
    }

    #[tokio::test]
    async fn unapproved_condition_never_executes_or_calls_model_and_skip_closes_run() {
        let f = fixture(Some(inline("exit 0"))).await;
        let calls = AtomicUsize::new(0);
        let attempt = crate::agents::runner::run_condition_then(
            condition_with_executor(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.lease,
                |_| async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    ScriptResult::default()
                },
            ),
            &f.pool,
            &f.run.id,
            |_| {},
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        let responder = async {
            let first = pending(&f.state).await;
            tokio::task::yield_now().await;
            assert_eq!(pending(&f.state).await.id, first.id);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            respond(&f, false).await;
        };
        let (result, ()) = tokio::join!(attempt, responder);
        result.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(repository::active_run_id(&f.pool, &f.session.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn changed_selected_file_replaces_one_request_then_executes_only_new_approved_bytes() {
        let f = fixture(Some(inline("unused"))).await;
        let path = f.root.join("check.sh");
        std::fs::write(&path, "exit 1").unwrap();
        let script = ScriptConfig {
            source: ScriptSource::File {
                path: path.display().to_string(),
            },
            ..inline("")
        };
        f.state
            .update_workspace_config("ws", |w| {
                w.schedule.condition = Some(script.clone());
                Ok(())
            })
            .unwrap();
        let execution_path = path.clone();
        let attempt = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |snapshot| async move {
                assert_eq!(snapshot.content, "exit 0");
                // No file reopening to choose execution bytes after capture.
                std::fs::write(&execution_path, "unapproved bytes").unwrap();
                assert_eq!(snapshot.content, "exit 0");
                ScriptResult {
                    exit_code: Some(0),
                    ..Default::default()
                }
            },
        );
        let responder = async {
            let first = pending(&f.state).await;
            std::fs::write(&path, "exit 0").unwrap();
            assert!(crate::commands::schedule_scripts::resolve_approval(
                &f.state, &f.pool, "ws", &first.id, true
            )
            .await
            .is_err());
            let replacement = pending(&f.state).await;
            assert_ne!(first.id, replacement.id);
            assert_eq!(replacement.content, "exit 0");
            respond(&f, true).await;
        };
        let (outcome, ()) = tokio::join!(attempt, responder);
        assert!(matches!(outcome, ConditionOutcome::Allow));
    }

    #[tokio::test]
    async fn manual_request_discards_unanswered_approval_and_running_condition_finishes_before_yield(
    ) {
        let f = fixture(Some(inline("exit 0"))).await;
        let attempt = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |_| async { panic!("unapproved script executed") },
        );
        let manual = async {
            pending(&f.state).await;
            f.state.scheduler.lock().await.yield_to_manual("ws");
        };
        let (outcome, ()) = tokio::join!(attempt, manual);
        assert!(matches!(outcome, ConditionOutcome::Abandoned));
        assert!(f
            .state
            .scheduler
            .lock()
            .await
            .script_status("ws")
            .pending
            .is_none());

        let f = fixture(Some(inline("exit 0"))).await;
        let calls = AtomicUsize::new(0);
        let attempt = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                f.state.scheduler.lock().await.yield_to_manual("ws");
                assert!(repository::active_run_id(&f.pool, &f.session.id)
                    .await
                    .unwrap()
                    .is_some());
                assert!(f.state.scheduler.lock().await.script_is_running("ws"));
                ScriptResult {
                    exit_code: Some(0),
                    ..Default::default()
                }
            },
        );
        let (outcome, ()) = tokio::join!(attempt, respond(&f, true));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(outcome, ConditionOutcome::Abandoned));
        assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
    }

    #[tokio::test]
    async fn disabled_schedule_and_wake_expire_pending_approval_without_execution() {
        for wake in [false, true] {
            let f = fixture(Some(inline("exit 0"))).await;
            let attempt = condition_with_executor(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.lease,
                |_| async { panic!("expired script executed") },
            );
            let invalidate = async {
                pending(&f.state).await;
                let mut scheduler = f.state.scheduler.lock().await;
                if wake {
                    scheduler.wake_signal().did_wake();
                } else {
                    scheduler.set_instance_enabled("main", false);
                }
            };
            let (outcome, ()) = tokio::join!(attempt, invalidate);
            assert!(matches!(outcome, ConditionOutcome::Abandoned));
            assert!(f
                .state
                .scheduler
                .lock()
                .await
                .script_status("ws")
                .pending
                .is_none());
        }
    }

    #[tokio::test]
    async fn no_condition_proceeds_without_approval_or_script() {
        let f = fixture(None).await;
        let outcome = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |_| async { panic!("no configured script") },
        )
        .await;
        assert!(matches!(outcome, ConditionOutcome::Allow));
        assert!(f
            .state
            .scheduler
            .lock()
            .await
            .script_status("ws")
            .pending
            .is_none());
    }

    #[tokio::test]
    async fn run_now_captures_unsaved_draft_without_saving_or_advancing_and_access_changes_require_new_approval(
    ) {
        let f = fixture(Some(inline("saved"))).await;
        let before = workspace_config::load(&f.root)
            .unwrap()
            .schedule
            .next_run_at_unix_ms;
        let draft = capture(&f.state, "ws", &inline("unsaved draft")).unwrap();
        let first = f
            .state
            .scheduler
            .lock()
            .await
            .scripts
            .entry("ws".into())
            .or_default()
            .request(draft, "test", None);
        assert_eq!(first.content, "unsaved draft");
        f.state
            .update_workspace_config("ws", |w| {
                w.main_agent.as_mut().unwrap().execution.sandbox.network =
                    SandboxNetworkConfig::Disabled;
                Ok(())
            })
            .unwrap();
        assert!(crate::commands::schedule_scripts::resolve_approval(
            &f.state, &f.pool, "ws", &first.id, true
        )
        .await
        .is_err());
        let next = pending(&f.state).await;
        assert_ne!(next.id, first.id);
        assert!(next.access.contains("Disabled"));
        let saved = workspace_config::load(&f.root).unwrap();
        assert_eq!(saved.schedule.condition, Some(inline("saved")));
        assert_eq!(saved.schedule.next_run_at_unix_ms, before);
    }

    #[tokio::test]
    async fn pending_approval_expires_on_block_close_and_true_result_is_never_carried_forward() {
        use std::sync::atomic::AtomicI64;
        let start = chrono::DateTime::parse_from_rfc3339("2026-11-02T09:00:00Z")
            .unwrap()
            .timestamp_millis();
        let blocks = crate::agents::weekly_blocks::WeeklyAllowedBlocks {
            timezone: "UTC".into(),
            slots: vec![36],
        };
        for running in [false, true] {
            let f = fixture_at(Some(inline("exit 0")), Some(blocks.clone()), start).await;
            let clock = AtomicI64::new(start + 15_000);
            let attempt = condition_attempt(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.lease,
                |_| async {
                    assert!(running, "unapproved script executed");
                    clock.store(start + 15 * 60 * 1000, Ordering::SeqCst);
                    ScriptResult {
                        exit_code: Some(0),
                        ..Default::default()
                    }
                },
                || clock.load(Ordering::SeqCst),
            );
            let closer = async {
                pending(&f.state).await;
                if running {
                    // Resolve with this test's controlled eligibility clock. The
                    // contents and effective permissions still use production capture.
                    let pending = f
                        .state
                        .scheduler
                        .lock()
                        .await
                        .scripts
                        .get_mut("ws")
                        .unwrap()
                        .pending
                        .take()
                        .unwrap();
                    pending.decision.unwrap().send(Some(pending.snapshot)).ok();
                } else {
                    clock.store(start + 15 * 60 * 1000, Ordering::SeqCst);
                }
            };
            let (result, ()) = tokio::join!(attempt, closer);
            assert!(matches!(result.unwrap(), ConditionOutcome::Abandoned));
            assert!(f
                .state
                .scheduler
                .lock()
                .await
                .script_status("ws")
                .pending
                .is_none());
        }
    }

    #[test]
    fn invalid_input_and_current_policy_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let exec = execution();
        for script in [
            ScriptConfig {
                timeout_seconds: 0,
                ..inline("exit 0")
            },
            ScriptConfig {
                interpreter: "/missing/sh".into(),
                ..inline("exit 0")
            },
            ScriptConfig {
                source: ScriptSource::File {
                    path: "/missing/file".into(),
                },
                ..inline("")
            },
        ] {
            assert!(capture_with_execution(dir.path(), &exec, &script).is_err());
        }
        let mut blocked = exec.clone();
        blocked.shell.mode = ShellAccessMode::Off;
        assert!(capture_with_execution(dir.path(), &blocked, &inline("exit 0")).is_err());
        blocked.shell.mode = ShellAccessMode::Restricted;
        blocked.shell.blocked_command_prefixes = vec!["sh".into()];
        assert!(capture_with_execution(dir.path(), &blocked, &inline("exit 0")).is_err());
        assert!(invocation_policy(&blocked, "/bin/sh -c 'exit 0'", true).is_err());
    }

    #[test]
    fn approval_identity_includes_contents_interpreter_timeout_and_resolved_access() {
        let dir = tempfile::tempdir().unwrap();
        let exec = execution();
        let first = capture_with_execution(dir.path(), &exec, &inline("exit 0")).unwrap();
        for script in [
            inline("exit 1"),
            ScriptConfig {
                timeout_seconds: 5,
                ..inline("exit 0")
            },
            ScriptConfig {
                interpreter: "/bin/bash".into(),
                ..inline("exit 0")
            },
        ] {
            assert!(
                !first.same_approval(&capture_with_execution(dir.path(), &exec, &script).unwrap())
            );
        }
        let mut restricted = exec;
        restricted.sandbox.network = SandboxNetworkConfig::Disabled;
        assert!(!first.same_approval(
            &capture_with_execution(dir.path(), &restricted, &inline("exit 0")).unwrap()
        ));
    }
}
