//! Captured schedule scripts and their transient, workspace-owned approvals.
//! Script consent never changes the Main agent's execution policy.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
            return Err("Select a script file.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub enum ScriptPhase {
    #[default]
    Condition,
    Before,
    After,
}

impl ScriptPhase {
    pub fn script(
        self,
        schedule: &crate::config::workspace_config::WorkspaceSchedule,
    ) -> Option<&ScriptConfig> {
        match self {
            Self::Condition => schedule.condition.as_ref(),
            Self::Before => schedule.before.as_ref(),
            Self::After => schedule.after.as_ref(),
        }
    }
    pub fn consent(
        self,
        schedule: &crate::config::workspace_config::WorkspaceSchedule,
    ) -> Option<&ScriptConsent> {
        match self {
            Self::Condition => schedule.condition_consent.as_ref(),
            Self::Before => schedule.before_consent.as_ref(),
            Self::After => schedule.after_consent.as_ref(),
        }
    }
    pub fn set_consent(
        self,
        schedule: &mut crate::config::workspace_config::WorkspaceSchedule,
        consent: Option<ScriptConsent>,
    ) {
        match self {
            Self::Condition => schedule.condition_consent = consent,
            Self::Before => schedule.before_consent = consent,
            Self::After => schedule.after_consent = consent,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct ScriptApproval {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub phase: Option<ScriptPhase>,
    pub id: String,
    pub purpose: String,
    pub content: String,
    pub interpreter: String,
    pub timeout_seconds: u32,
    pub access: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct ScriptResult {
    /// Some(false): known not launched. Some(true): known launched and collected.
    /// None: legacy or uncertain launch/completion; cleanup must remain actionable.
    #[serde(default)]
    #[ts(optional)]
    pub started: Option<bool>,
    #[serde(skip)]
    #[ts(skip)]
    pub(crate) start_disposition: StartDisposition,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum StartDisposition {
    #[default]
    Attempted,
    Abandoned,
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
    start_lease: Option<super::scheduler::DispatchLease>,
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
    pub confirmed_hooks: HashMap<ScriptPhase, (String, CapturedScript)>,
    pub phase: ScriptPhase,
    pub results: HashMap<ScriptPhase, ScriptResult>,
    /// Holds manual dispatch until preparation/model and required cleanup finish.
    pub cleanup_owned: bool,
    pub cleanup_interrupted: bool,
    /// One detached recovery task, including its approval wait, per workspace.
    pub recovery_active: bool,
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
    pub fn status_for(&self, phase: ScriptPhase) -> ScriptStatus {
        if self.phase == phase {
            return self.status();
        }
        let result = self.results.get(&phase).cloned();
        ScriptStatus {
            error: result.as_ref().and_then(|r| r.error.clone()),
            result,
            pending: None,
            running: false,
        }
    }
    pub fn record_result(&mut self, phase: ScriptPhase, result: ScriptResult) {
        self.phase = phase;
        self.running = false;
        self.error = result.error.clone();
        self.result = Some(result.clone());
        self.results.insert(phase, result);
    }
    pub fn confirmed_for(&self, phase: ScriptPhase, id: Option<&String>) -> Option<CapturedScript> {
        let value = if phase == ScriptPhase::Condition {
            self.confirmed.as_ref()
        } else {
            self.confirmed_hooks.get(&phase)
        };
        value
            .filter(|(key, _)| Some(key) == id)
            .map(|(_, value)| value.clone())
    }
    pub fn request(
        &mut self,
        snapshot: CapturedScript,
        purpose: &str,
        decision: Option<oneshot::Sender<Option<CapturedScript>>>,
    ) -> ScriptApproval {
        self.request_for(snapshot, purpose, ScriptPhase::Condition, decision)
    }
    pub fn request_for(
        &mut self,
        snapshot: CapturedScript,
        purpose: &str,
        phase: ScriptPhase,
        decision: Option<oneshot::Sender<Option<CapturedScript>>>,
    ) -> ScriptApproval {
        self.phase = phase;
        self.result = self.results.get(&phase).cloned();
        let approval = ScriptApproval {
            phase: Some(phase),
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
    saved_phase_consent(
        state,
        workspace,
        ScriptPhase::Condition,
        condition,
        unattended,
        confirmed,
    )
}

pub(crate) fn saved_phase_consent(
    state: &AppState,
    workspace: &crate::config::WorkspaceConfig,
    phase: ScriptPhase,
    script: &Option<ScriptConfig>,
    unattended: Option<bool>,
    confirmed: Option<&CapturedScript>,
) -> Result<Option<ScriptConsent>, String> {
    if unattended == Some(false) || script.is_none() {
        return Ok(None);
    }
    if unattended.is_none() {
        return Ok((phase.script(&workspace.schedule) == script.as_ref())
            .then(|| phase.consent(&workspace.schedule).cloned())
            .flatten());
    }
    let current = capture_for_config(state, workspace, script.as_ref().unwrap())?;
    let approved = confirmed.is_some_and(|snapshot| {
        snapshot.config == current.config && snapshot.same_approval(&current)
    }) || phase
        .consent(&workspace.schedule)
        .is_some_and(|consent| consent.allows(&current));
    if !approved {
        return Err(format!(
            "Review and explicitly approve the {phase:?} script for unattended execution."
        ));
    }
    Ok(Some(current.consent()))
}

fn capture_with_execution(
    root: &Path,
    execution: &ExecutionCapabilityConfig,
    script: &ScriptConfig,
) -> Result<CapturedScript, String> {
    script.validate()?;
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err("Schedule scripts are unavailable: filesystem and network restrictions cannot be enforced on this platform.".into());
    }
    let content = read_script_content(root, script)?;
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
        variables: None,
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
        start_lease: None,
    })
}

fn read_script_content(root: &Path, script: &ScriptConfig) -> Result<String, String> {
    use std::io::Read;
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
    Ok(content)
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
    pub(crate) fn guard_start(mut self, lease: &super::scheduler::DispatchLease) -> Self {
        self.start_lease = Some(lease.clone());
        self
    }

    pub async fn execute(self, state: &AppState, workspace_id: &str) -> ScriptResult {
        self.execute_with_clock(state, workspace_id, || {
            chrono::Utc::now().timestamp_millis()
        })
        .await
    }

    async fn execute_with_clock(
        self,
        state: &AppState,
        workspace_id: &str,
        clock: impl Fn() -> i64 + Send + Sync,
    ) -> ScriptResult {
        let started = std::time::Instant::now();
        let preparation = (|| {
            let current = capture(state, workspace_id, &self.config)?;
            if !self.same_approval(&current) {
                return Err("Script contents, interpreter or effective access changed. Review a fresh approval.".to_string());
            }
            invocation_policy(
                &current_execution(state, workspace_id)?,
                &self.invocation,
                true,
            )?;
            interpreter_flag(&self.config.interpreter)
        })();
        let flag = match preparation {
            Ok(flag) => flag,
            Err(error) => {
                return ScriptResult {
                    started: Some(false),
                    error: Some(error),
                    ..Default::default()
                }
            }
        };
        let check = || {
            self.start_lease
                .as_ref()
                .is_none_or(|lease| lease.try_start_script(&clock))
        };
        let result = crate::assistant::sandbox::runner::run_command_guarded(
            SandboxCommand {
                argv: vec![
                    self.interpreter.clone().into_os_string(),
                    OsString::from(flag),
                    self.content.clone().into(),
                ],
                cwd: self.profile.filesystem.workspace_root.clone(),
                timeout_ms: u64::from(self.config.timeout_seconds) * 1000,
                max_output_chars: OUTPUT_LIMIT,
                profile: self.profile.clone(),
            },
            Some(&check),
        )
        .await;
        match result {
            Ok(None) => ScriptResult {
                started: Some(false),
                start_disposition: StartDisposition::Abandoned,
                ..Default::default()
            },
            other => script_result(
                other.map(|output| output.expect("admitted command output")),
                started,
            ),
        }
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
                    Some(code) => Some(format!("Script exited with status {code}.")),
                    None => Some("Script terminated without an exit status (signal).".into()),
                }
            };
            ScriptResult {
                started: Some(true),
                start_disposition: StartDisposition::Attempted,
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

pub(crate) fn normalize_result(phase: ScriptPhase, result: &mut ScriptResult) {
    if phase != ScriptPhase::Condition && result.error.is_none() && result.exit_code != Some(0) {
        result.error = Some(format!(
            "{phase:?} script failed with exit status {:?}.",
            result.exit_code
        ));
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
        snapshot.guard_start(lease).execute(state, workspace_id)
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
        {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.phase = ScriptPhase::Condition;
            runtime.result = runtime.results.get(&ScriptPhase::Condition).cloned();
            runtime.error = None;
            runtime.running = true;
        }
        let result = execute(snapshot.guard_start(lease)).await;
        let abandoned = result.start_disposition == StartDisposition::Abandoned;
        {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.record_result(ScriptPhase::Condition, result.clone());
        }
        if abandoned || !condition_eligible(pool, session_id, lease, clock()).await? {
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
        pending_for(state, ScriptPhase::Condition).await
    }
    async fn pending_for(state: &AppState, phase: ScriptPhase) -> ScriptApproval {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(approval) = state
                    .scheduler
                    .lock()
                    .await
                    .script_status_for("ws", phase)
                    .pending
                {
                    return approval;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    async fn respond(f: &Fixture, run: bool) {
        respond_for(f, ScriptPhase::Condition, run).await;
    }
    async fn respond_for(f: &Fixture, phase: ScriptPhase, run: bool) {
        let approval = pending_for(&f.state, phase).await;
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

    async fn hook_fixture(before: bool, before_consent: bool, after_consent: bool) -> Fixture {
        let f = fixture(None).await;
        let prep = before.then(|| inline("prepare"));
        let cleanup = inline("cleanup");
        let prep_consent = prep
            .as_ref()
            .filter(|_| before_consent)
            .map(|script| capture(&f.state, "ws", script).unwrap().consent());
        let cleanup_consent =
            after_consent.then(|| capture(&f.state, "ws", &cleanup).unwrap().consent());
        f.state
            .update_workspace_config("ws", |workspace| {
                workspace.schedule.before = prep;
                workspace.schedule.after = Some(cleanup);
                workspace.schedule.before_consent = prep_consent;
                workspace.schedule.after_consent = cleanup_consent;
                Ok(())
            })
            .unwrap();
        f
    }

    #[tokio::test]
    async fn preparation_outcomes_cleanup_partial_work_and_never_call_model_on_failure() {
        for (code, started, timed_out, expected_agent, expected_cleanup) in [
            (Some(0), true, false, 1, 1),
            (Some(1), true, false, 0, 1),
            (Some(2), true, false, 0, 1),
            (None, true, true, 0, 1),
            (None, true, false, 0, 1),
            (None, false, false, 0, 0),
        ] {
            let f = hook_fixture(true, true, true).await;
            let models = AtomicUsize::new(0);
            let cleanups = AtomicUsize::new(0);
            crate::agents::hooks::run_prepared_with_executor(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.run.id,
                &f.lease,
                tokio_util::sync::CancellationToken::new(),
                |_| {},
                |snapshot, phase| {
                    let f = &f;
                    let cleanups = &cleanups;
                    async move {
                        let saved = workspace_config::load(&f.root).unwrap();
                        let intent = saved.schedule.cleanup_intent.unwrap();
                        assert_eq!(intent.content, "cleanup");
                        assert!(intent
                            .identity
                            .allows(&capture(&f.state, "ws", &intent.script).unwrap()));
                        assert!(f.state.scheduler.lock().await.script_is_running("ws"));
                        if phase == ScriptPhase::Before {
                            assert_eq!(snapshot.content, "prepare");
                            ScriptResult {
                                started: Some(started),
                                exit_code: code,
                                timed_out,
                                error: (code.is_none()).then(|| "failed".into()),
                                ..Default::default()
                            }
                        } else {
                            assert_eq!(phase, ScriptPhase::After);
                            cleanups.fetch_add(1, Ordering::SeqCst);
                            ScriptResult {
                                started: Some(true),
                                exit_code: Some(0),
                                ..Default::default()
                            }
                        }
                    }
                },
                || async {
                    models.fetch_add(1, Ordering::SeqCst);
                    assert!(f.lease.try_commit());
                    repository::update_run_status(&f.pool, &f.run.id, RunStatus::Completed, None)
                        .await
                        .unwrap();
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert_eq!(models.load(Ordering::SeqCst), expected_agent);
            assert_eq!(cleanups.load(Ordering::SeqCst), expected_cleanup);
            assert!(workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_intent
                .is_none());
            assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
            if code == Some(1) {
                assert!(f
                    .state
                    .scheduler
                    .lock()
                    .await
                    .script_status_for("ws", ScriptPhase::Before)
                    .error
                    .is_some());
            }
        }
    }

    #[tokio::test]
    async fn skipped_before_has_no_cleanup_intent_or_agent_call() {
        let f = hook_fixture(true, false, false).await;
        let attempt = crate::agents::hooks::run_prepared_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.run.id,
            &f.lease,
            tokio_util::sync::CancellationToken::new(),
            |_| {},
            |_, _| async { panic!("skipped preparation executed") },
            || async { panic!("skipped preparation called model") },
        );
        let skip = async {
            assert_eq!(
                pending_for(&f.state, ScriptPhase::Before).await.phase,
                Some(ScriptPhase::Before)
            );
            assert!(workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_intent
                .is_none());
            respond_for(&f, ScriptPhase::Before, false).await;
        };
        let (result, ()) = tokio::join!(attempt, skip);
        result.unwrap();
        assert!(workspace_config::load(&f.root)
            .unwrap()
            .schedule
            .cleanup_intent
            .is_none());
        assert!(!repository::get_run(&f.pool, &f.run.id)
            .await
            .unwrap()
            .unwrap()
            .status
            .is_active());
    }

    #[tokio::test]
    async fn manual_disable_and_cancel_during_preparation_still_cleanup_before_release() {
        for interruption in ["manual", "disable", "cancel"] {
            let f = hook_fixture(true, true, true).await;
            let cancel = tokio_util::sync::CancellationToken::new();
            let cleanups = AtomicUsize::new(0);
            crate::agents::hooks::run_prepared_with_executor(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.run.id,
                &f.lease,
                cancel.clone(),
                |_| {},
                |_, phase| {
                    let f = &f;
                    let cancel = &cancel;
                    let cleanups = &cleanups;
                    async move {
                        if phase == ScriptPhase::Before {
                            match interruption {
                                "manual" => f.state.scheduler.lock().await.yield_to_manual("ws"),
                                "disable" => f
                                    .state
                                    .scheduler
                                    .lock()
                                    .await
                                    .set_instance_enabled("main", false),
                                _ => cancel.cancel(),
                            }
                        } else {
                            cleanups.fetch_add(1, Ordering::SeqCst);
                            assert!(f.state.scheduler.lock().await.script_is_running("ws"));
                        }
                        ScriptResult {
                            started: Some(true),
                            exit_code: Some(0),
                            ..Default::default()
                        }
                    }
                },
                || async { panic!("revoked preparation called model") },
            )
            .await
            .unwrap();
            assert_eq!(cleanups.load(Ordering::SeqCst), 1);
            assert!(workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_intent
                .is_none());
        }
    }

    #[tokio::test]
    async fn after_approval_survives_disable_and_owns_workspace_until_process_finishes() {
        let f = hook_fixture(true, true, false).await;
        let (finish, finished) = tokio::sync::oneshot::channel();
        let finished = std::sync::Mutex::new(Some(finished));
        let (started, started_wait) = tokio::sync::oneshot::channel();
        let started = std::sync::Mutex::new(Some(started));
        let attempt = crate::agents::hooks::run_prepared_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.run.id,
            &f.lease,
            tokio_util::sync::CancellationToken::new(),
            |_| {},
            |_, phase| {
                let finished = &finished;
                let started = &started;
                async move {
                    if phase == ScriptPhase::After {
                        let wait = finished.lock().unwrap().take().unwrap();
                        started.lock().unwrap().take().unwrap().send(()).unwrap();
                        wait.await.unwrap();
                    }
                    ScriptResult {
                        started: Some(true),
                        exit_code: Some(0),
                        ..Default::default()
                    }
                }
            },
            || async {
                assert!(f.lease.try_commit());
                Ok(())
            },
        );
        let responder = async {
            let approval = pending_for(&f.state, ScriptPhase::After).await;
            assert_eq!(approval.phase, Some(ScriptPhase::After));
            {
                let mut scheduler = f.state.scheduler.lock().await;
                scheduler.set_instance_enabled("main", false);
                scheduler.yield_to_manual("ws");
                assert!(scheduler.script_is_running("ws"));
            }
            respond_for(&f, ScriptPhase::After, true).await;
            started_wait.await.unwrap();
            assert!(f.state.scheduler.lock().await.script_is_running("ws"));
            assert!(workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_intent
                .is_some());
            finish.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(attempt, responder);
        result.unwrap();
        assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
    }

    #[tokio::test]
    async fn no_preparation_only_cleans_after_agent_commit_and_completed_cleanup_failure_releases()
    {
        for agent_starts in [false, true] {
            let f = hook_fixture(false, false, true).await;
            let cleanups = AtomicUsize::new(0);
            let result = crate::agents::hooks::run_prepared_with_executor(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.run.id,
                &f.lease,
                tokio_util::sync::CancellationToken::new(),
                |_| {},
                |_, phase| {
                    let cleanups = &cleanups;
                    async move {
                        assert_eq!(phase, ScriptPhase::After);
                        cleanups.fetch_add(1, Ordering::SeqCst);
                        ScriptResult {
                            started: Some(true),
                            exit_code: None,
                            timed_out: true,
                            error: Some("cleanup timed out".into()),
                            ..Default::default()
                        }
                    }
                },
                || async {
                    if agent_starts {
                        assert!(f.lease.try_commit());
                    }
                    Err(crate::agents::runner::RunnerError::AssistantPersistence(
                        "model failed".into(),
                    ))
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(cleanups.load(Ordering::SeqCst), usize::from(agent_starts));
            assert!(workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_intent
                .is_none());
            assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
            if agent_starts {
                assert!(
                    f.state
                        .scheduler
                        .lock()
                        .await
                        .script_status_for("ws", ScriptPhase::After)
                        .result
                        .unwrap()
                        .timed_out
                );
            }
        }
    }
    async fn interrupted_cleanup(
        f: &Fixture,
        script: &ScriptConfig,
        unattended: bool,
    ) -> crate::agents::hooks::CleanupIntent {
        let snapshot = capture(&f.state, "ws", script).unwrap();
        let consent = unattended.then(|| snapshot.consent());
        f.state
            .update_workspace_config("ws", |workspace| {
                workspace.schedule.after = Some(script.clone());
                workspace.schedule.after_consent = consent.clone();
                workspace.schedule.enabled = false;
                Ok(())
            })
            .unwrap();
        let intent = crate::agents::hooks::persist_intent(
            &f.state,
            "ws",
            &f.session.id,
            &f.run.id,
            &snapshot,
            consent,
        )
        .unwrap();
        repository::complete_run(&f.pool, &f.run.id, RunStatus::Cancelled, None)
            .await
            .unwrap();
        *f.state.scheduler.lock().await = crate::agents::scheduler::Scheduler::new();
        intent
    }

    async fn recovery_pending(state: &AppState) -> ScriptApproval {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(pending) = state
                    .scheduler
                    .lock()
                    .await
                    .script_status_for("ws", ScriptPhase::After)
                    .pending
                {
                    return pending;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn cleanup_restart_forgets_run_once_and_preserves_intent_until_explicit_skip() {
        let f = fixture(None).await;
        let intent = interrupted_cleanup(&f, &inline("exit 0"), false).await;
        let entered = Arc::new(AtomicUsize::new(0));
        let count = entered.clone();
        let first = crate::agents::cleanup_recovery::recover_with_executor(
            &f.state,
            "ws",
            move |_| async move {
                count.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<ScriptResult>().await
            },
        );
        let decision = async {
            let approval = recovery_pending(&f.state).await;
            assert_eq!(approval.purpose, "recovery");
            crate::commands::schedule_scripts::resolve_approval(
                &f.state,
                &f.pool,
                "ws",
                &approval.id,
                true,
            )
            .await
            .unwrap();
            while entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            approval.id
        };
        let first_id = tokio::select! {
            outcome = first => panic!("cleanup should be interrupted, got {outcome:?}"),
            approval_id = decision => approval_id,
        };
        assert_eq!(
            workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_intent
                .unwrap()
                .id,
            intent.id
        );
        *f.state.scheduler.lock().await = crate::agents::scheduler::Scheduler::new();
        let retry =
            crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
                panic!("Skip cannot execute cleanup")
            });
        let skip = async {
            let approval = recovery_pending(&f.state).await;
            assert_ne!(approval.id, first_id);
            assert!(f.state.scheduler.lock().await.script_is_running("ws"));
            crate::commands::schedule_scripts::resolve_approval(
                &f.state,
                &f.pool,
                "ws",
                &approval.id,
                false,
            )
            .await
            .unwrap();
        };
        let (result, _) = tokio::join!(retry, skip);
        result.unwrap();
        let saved = workspace_config::load(&f.root).unwrap();
        assert!(!saved.schedule.enabled);
        assert!(saved.schedule.cleanup_intent.is_none());
        assert!(saved.schedule.cleanup_completion.unwrap().skipped);
        assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
    }

    #[tokio::test]
    async fn cleanup_restart_records_completed_failure_and_restores_it_without_retry() {
        let f = fixture(None).await;
        interrupted_cleanup(&f, &inline("exit 2"), true).await;
        let calls = AtomicUsize::new(0);
        crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            ScriptResult {
                started: Some(true),
                exit_code: Some(2),
                ..Default::default()
            }
        })
        .await
        .unwrap();
        let saved = workspace_config::load(&f.root).unwrap();
        assert!(saved.schedule.cleanup_intent.is_none());
        assert_eq!(
            saved
                .schedule
                .cleanup_completion
                .as_ref()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .exit_code,
            Some(2)
        );
        *f.state.scheduler.lock().await = crate::agents::scheduler::Scheduler::new();
        crate::agents::cleanup_recovery::restore_runtime(
            &mut *f.state.scheduler.lock().await,
            &saved,
        );
        assert!(f
            .state
            .scheduler
            .lock()
            .await
            .script_status_for("ws", ScriptPhase::After)
            .result
            .unwrap()
            .error
            .is_some());
        crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            ScriptResult::default()
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cleanup_restart_changed_bound_file_requires_restore_or_explicit_skip() {
        let f = fixture(None).await;
        let path = f.root.join("cleanup.sh");
        std::fs::write(&path, "exit 0").unwrap();
        let script = ScriptConfig {
            source: ScriptSource::File {
                path: "cleanup.sh".into(),
            },
            ..inline("")
        };
        let intent = interrupted_cleanup(&f, &script, true).await;
        std::fs::write(&path, "touch unapproved").unwrap();
        for _ in 0..2 {
            let result =
                crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
                    panic!("changed bytes must not execute")
                })
                .await;
            assert!(result.is_err());
            let scheduler = f.state.scheduler.lock().await;
            assert!(scheduler.script_is_running("ws"));
            assert!(scheduler
                .script_status_for("ws", ScriptPhase::After)
                .pending
                .is_none());
            assert!(scheduler
                .script_status_for("ws", ScriptPhase::After)
                .error
                .is_some());
        }
        assert!(
            crate::agents::cleanup_recovery::skip_unavailable(&f.state, "ws", "stale-id")
                .await
                .is_err()
        );
        assert!(workspace_config::load(&f.root)
            .unwrap()
            .schedule
            .cleanup_intent
            .is_some());
        crate::agents::cleanup_recovery::skip_unavailable(&f.state, "ws", &intent.id)
            .await
            .unwrap();
        assert!(
            workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_completion
                .unwrap()
                .skipped
        );
    }

    #[tokio::test]
    async fn cleanup_restart_retries_uncertain_intent_gap_even_when_scheduling_is_disabled() {
        let f = fixture(None).await;
        interrupted_cleanup(&f, &inline("exit 0"), true).await;
        let calls = AtomicUsize::new(0);
        crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            ScriptResult {
                started: Some(true),
                exit_code: Some(0),
                ..Default::default()
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(workspace_config::load(&f.root)
            .unwrap()
            .schedule
            .cleanup_intent
            .is_none());
    }
    #[tokio::test]
    async fn real_cleanup_launch_failure_retains_intent_until_explicit_skip() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture(None).await;
        let interpreter = f.root.join("sh");
        std::fs::write(
            &interpreter,
            format!("#!{}\n", f.root.join("missing-interpreter").display()),
        )
        .unwrap();
        std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = ScriptConfig {
            interpreter: interpreter.display().to_string(),
            ..inline("printf unexpected > effect")
        };
        let intent = interrupted_cleanup(&f, &script, true).await;
        assert!(crate::agents::cleanup_recovery::recover(&f.state, "ws")
            .await
            .is_err());
        assert!(!f.root.join("effect").exists());
        let saved = workspace_config::load(&f.root).unwrap();
        assert_eq!(saved.schedule.cleanup_intent.unwrap().id, intent.id);
        assert!(saved.schedule.cleanup_completion.is_none());
        let scheduler = f.state.scheduler.lock().await;
        assert!(scheduler.script_is_running("ws"));
        assert!(scheduler.script_notice("ws").is_some());
        assert!(scheduler
            .script_status_for("ws", ScriptPhase::After)
            .error
            .is_some());
        drop(scheduler);
        assert_eq!(
            repository::current_runs(&f.pool, &f.session.id)
                .await
                .unwrap()
                .len(),
            1
        );
        crate::agents::cleanup_recovery::skip_unavailable(&f.state, "ws", &intent.id)
            .await
            .unwrap();
        assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
        assert!(
            workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_completion
                .unwrap()
                .skipped
        );
    }

    #[tokio::test]
    async fn live_after_final_capture_failure_preserves_ownership_and_original_intent() {
        let f = hook_fixture(true, true, true).await;
        let result = crate::agents::hooks::run_prepared_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.run.id,
            &f.lease,
            tokio_util::sync::CancellationToken::new(),
            |_| {},
            |snapshot, phase| {
                let f = &f;
                async move {
                    if phase == ScriptPhase::Before {
                        return ScriptResult {
                            started: Some(true),
                            exit_code: Some(0),
                            ..Default::default()
                        };
                    }
                    f.state
                        .update_workspace_config("ws", |workspace| {
                            workspace.main_agent.as_mut().unwrap().execution.shell.mode =
                                ShellAccessMode::Off;
                            Ok(())
                        })
                        .unwrap();
                    let result = snapshot.execute(&f.state, "ws").await;
                    assert_eq!(result.started, Some(false));
                    assert!(result.error.is_some());
                    result
                }
            },
            || async {
                assert!(f.lease.try_commit());
                Ok(())
            },
        )
        .await;
        assert!(result.is_err());
        let saved = workspace_config::load(&f.root).unwrap();
        assert_eq!(
            saved.schedule.cleanup_intent.as_ref().unwrap().content,
            "cleanup"
        );
        assert!(saved.schedule.cleanup_completion.is_none());
        assert!(f.state.scheduler.lock().await.script_is_running("ws"));
        repository::complete_run(&f.pool, &f.run.id, RunStatus::Cancelled, None)
            .await
            .unwrap();
        let id = saved.schedule.cleanup_intent.unwrap().id;
        crate::agents::cleanup_recovery::skip_unavailable(&f.state, "ws", &id)
            .await
            .unwrap();
        assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
    }

    #[tokio::test]
    async fn uncertain_before_collection_requires_cleanup_without_model_entry() {
        let f = hook_fixture(true, true, true).await;
        let cleanup = AtomicUsize::new(0);
        crate::agents::hooks::run_prepared_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.run.id,
            &f.lease,
            tokio_util::sync::CancellationToken::new(),
            |_| {},
            |_, phase| {
                let cleanup = &cleanup;
                async move {
                    if phase == ScriptPhase::Before {
                        script_result(
                            Err("collection failed after uncertain launch".into()),
                            Instant::now(),
                        )
                    } else {
                        cleanup.fetch_add(1, Ordering::SeqCst);
                        ScriptResult {
                            started: Some(true),
                            exit_code: Some(0),
                            ..Default::default()
                        }
                    }
                }
            },
            || async { panic!("uncertain preparation entered model") },
        )
        .await
        .unwrap();
        assert_eq!(cleanup.load(Ordering::SeqCst), 1);
        assert!(workspace_config::load(&f.root)
            .unwrap()
            .schedule
            .cleanup_intent
            .is_none());
    }

    async fn assert_manual_and_due_admission(f: &Fixture) {
        use crate::assistant::types::{AuthMode, ContentPart, ProviderConnection};
        let message = repository::create_user_message_with_content(
            &f.pool,
            f.session.id.clone(),
            vec![ContentPart::Text {
                text: "queued manual after cleanup".into(),
            }],
            Some("conn"),
        )
        .await
        .unwrap();
        let admitted = crate::assistant::dispatcher::claim_with_cleanup_gate(
            &f.state.scheduler,
            "ws",
            &f.session.id,
            crate::assistant::dispatcher::claim(&f.pool, "ws", &f.session.id, |id| {
                Some(ProviderConnection {
                    id: id.into(),
                    name: id.into(),
                    protocol_id: "openai".into(),
                    provider_id: "openai".into(),
                    auth_mode: AuthMode::DeveloperApiKey,
                    base_url: None,
                    secret_ref: String::new(),
                    model_id: "model".into(),
                    account_label: None,
                    enabled: true,
                    created_at: 0,
                    updated_at: 0,
                })
            }),
        )
        .await
        .unwrap();
        let crate::assistant::dispatcher::Kick::Started {
            run, message_ids, ..
        } = admitted
        else {
            panic!("cleanup gate stranded queued manual input");
        };
        assert_eq!(message_ids, vec![message.id]);
        repository::complete_run(&f.pool, &run.id, RunStatus::Completed, None)
            .await
            .unwrap();
        let mut scheduler = f.state.scheduler.lock().await;
        assert!(!scheduler.script_is_running("ws"));
        assert!(scheduler.script_notice("ws").is_none());
        // A subsequent due schedule can claim this workspace again.
        let now = Instant::now() - Duration::from_secs(30);
        scheduler.set_wake_monitor_available(true, 0, now);
        scheduler.register_definition(
            crate::agents::AgentDefinition::new("next", "Next").with_workspace_id("ws"),
        );
        scheduler.create_instance("next").unwrap();
        scheduler
            .configure_enhanced(
                "next",
                workspace_config::ScheduleKind::EnhancedInterval {
                    interval_seconds: 15,
                },
                0,
                now,
            )
            .unwrap();
        assert_eq!(
            scheduler
                .next_ready_at(15_000, now + Duration::from_secs(15))
                .as_deref(),
            Some("next")
        );
    }

    #[tokio::test]
    async fn stale_prefilter_cannot_restore_completed_or_skipped_cleanup() {
        for completion in ["live", "skip", "recovery"] {
            let f = fixture(None).await;
            let intent = interrupted_cleanup(&f, &inline("exit 0"), true).await;
            let (observed, observed_wait) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let reader = async {
                // This is the same cheap prefilter used by recovery polling.
                let stale = workspace_config::load(&f.root).unwrap();
                assert_eq!(stale.schedule.cleanup_intent.unwrap().id, intent.id);
                observed.send(()).unwrap();
                released.await.unwrap();
                let mut scheduler = f.state.scheduler.lock().await;
                let current = crate::agents::cleanup_recovery::restore_current(
                    &f.state,
                    &mut scheduler,
                    "ws",
                )
                .unwrap();
                assert!(current.schedule.cleanup_intent.is_none());
                assert!(!scheduler.script_is_running("ws"));
                assert!(scheduler.script_notice("ws").is_none());
            };
            let writer = async {
                observed_wait.await.unwrap();
                if completion == "skip" {
                    let mut scheduler = f.state.scheduler.lock().await;
                    crate::agents::cleanup_recovery::restore_current(
                        &f.state,
                        &mut scheduler,
                        "ws",
                    )
                    .unwrap();
                    drop(scheduler);
                    crate::agents::cleanup_recovery::skip_unavailable(&f.state, "ws", &intent.id)
                        .await
                        .unwrap();
                } else if completion == "recovery" {
                    crate::agents::cleanup_recovery::recover_with_executor(
                        &f.state,
                        "ws",
                        |_| async {
                            ScriptResult {
                                started: Some(true),
                                exit_code: Some(0),
                                ..Default::default()
                            }
                        },
                    )
                    .await
                    .unwrap();
                } else {
                    crate::agents::cleanup_recovery::record_completion(
                        &f.state,
                        "ws",
                        &intent.id,
                        Some(ScriptResult {
                            started: Some(true),
                            exit_code: Some(0),
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap();
                }
                release.send(()).unwrap();
            };
            tokio::join!(reader, writer);
            // Also repair a stale runtime gate when a recovery caller finds no
            // authoritative intent; that early return must release ownership.
            {
                let mut scheduler = f.state.scheduler.lock().await;
                let runtime = scheduler.scripts.entry("ws".into()).or_default();
                runtime.cleanup_owned = true;
                runtime.cleanup_interrupted = true;
            }
            crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
                panic!("completed cleanup replayed")
            })
            .await
            .unwrap();
            assert_manual_and_due_admission(&f).await;
        }
    }

    async fn interrupt_attempt_at_phase(f: &Fixture, held_phase: &str) {
        let entered = tokio::sync::Notify::new();
        {
            let attempt = crate::agents::hooks::run_prepared_with_executor(
                &f.state,
                "ws",
                &f.pool,
                &f.session.id,
                &f.run.id,
                &f.lease,
                tokio_util::sync::CancellationToken::new(),
                |_| {},
                |_, phase| {
                    let entered = &entered;
                    async move {
                        assert_eq!(phase, ScriptPhase::Before);
                        if held_phase == "before" {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                        }
                        ScriptResult {
                            started: Some(true),
                            exit_code: Some(0),
                            ..Default::default()
                        }
                    }
                },
                || async {
                    assert_eq!(held_phase, "model");
                    assert!(f.lease.try_commit());
                    entered.notify_one();
                    std::future::pending::<Result<(), crate::agents::runner::RunnerError>>().await
                },
            );
            tokio::select! {
                result = attempt => panic!("attempt was meant to be interrupted: {result:?}"),
                () = entered.notified() => {},
            }
        }
    }

    #[tokio::test]
    async fn restarts_during_before_and_model_recover_only_bound_after_even_after_another_interruption(
    ) {
        for (held_phase, disabled) in [("before", false), ("model", true)] {
            let f = hook_fixture(true, true, true).await;
            interrupt_attempt_at_phase(&f, held_phase).await;
            let intent = workspace_config::load(&f.root)
                .unwrap()
                .schedule
                .cleanup_intent
                .unwrap();
            repository::complete_run(&f.pool, &f.run.id, RunStatus::Cancelled, None)
                .await
                .unwrap();
            f.state
                .update_workspace_config("ws", |workspace| {
                    workspace.schedule.enabled = !disabled;
                    Ok(())
                })
                .unwrap();
            *f.state.scheduler.lock().await = crate::agents::scheduler::Scheduler::new();
            let recovery_entered = tokio::sync::Notify::new();
            {
                let recovery = crate::agents::cleanup_recovery::recover_with_executor(
                    &f.state,
                    "ws",
                    |snapshot| {
                        let intent = &intent;
                        let recovery_entered = &recovery_entered;
                        async move {
                            assert_eq!(snapshot.content, intent.content);
                            assert_eq!(snapshot.content, "cleanup");
                            recovery_entered.notify_one();
                            std::future::pending::<ScriptResult>().await
                        }
                    },
                );
                tokio::select! {
                    result = recovery => panic!("recovery was meant to be interrupted: {result:?}"),
                    () = recovery_entered.notified() => {},
                }
            }
            assert_eq!(
                workspace_config::load(&f.root)
                    .unwrap()
                    .schedule
                    .cleanup_intent
                    .unwrap()
                    .id,
                intent.id
            );
            *f.state.scheduler.lock().await = crate::agents::scheduler::Scheduler::new();
            let calls = AtomicUsize::new(0);
            crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |snapshot| {
                let calls = &calls;
                async move {
                    assert_eq!(snapshot.content, "cleanup");
                    calls.fetch_add(1, Ordering::SeqCst);
                    ScriptResult {
                        started: Some(true),
                        exit_code: Some(0),
                        ..Default::default()
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let saved = workspace_config::load(&f.root).unwrap();
            assert_eq!(saved.schedule.enabled, !disabled);
            assert!(saved.schedule.cleanup_intent.is_none());
            assert_eq!(
                saved.schedule.cleanup_completion.unwrap().intent_id,
                intent.id
            );
            assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
        }
    }

    async fn revoke_start(f: &Fixture, reason: &str, wall: &std::sync::atomic::AtomicI64) {
        if reason == "block" {
            wall.fetch_add(30 * 60_000, Ordering::SeqCst);
            return;
        }
        let mut scheduler = f.state.scheduler.lock().await;
        match reason {
            "manual" => scheduler.yield_to_manual("ws"),
            "disable" => {
                scheduler.set_instance_enabled("main", false);
            }
            "pause" => scheduler.pause("test pause".into()),
            "wake" => {
                scheduler.wake_signal().will_sleep();
                scheduler.wake_signal().did_wake();
            }
            _ => panic!("unknown revocation"),
        }
    }

    fn current_block() -> (i64, crate::agents::weekly_blocks::WeeklyAllowedBlocks) {
        use chrono::{Datelike, Timelike};
        let now = chrono::Utc::now();
        let slot = now.weekday().num_days_from_monday() * 96 + now.hour() * 4 + now.minute() / 15;
        (
            now.timestamp_millis(),
            crate::agents::weekly_blocks::WeeklyAllowedBlocks {
                timezone: "UTC".into(),
                // Keep setup independent of proximity to a real quarter-hour.
                // The controlled clock closes both adjacent slots at final admission.
                slots: vec![slot as u16, ((slot + 1) % 672) as u16],
            },
        )
    }

    #[tokio::test]
    async fn final_spawn_rechecks_condition_and_before_after_early_eligibility() {
        for phase in [ScriptPhase::Condition, ScriptPhase::Before] {
            for reason in ["block", "manual", "disable", "pause", "wake"] {
                let (open, blocks) = current_block();
                let script = inline("printf started > forbidden-start");
                let f = fixture_at(
                    (phase == ScriptPhase::Condition).then(|| script.clone()),
                    Some(blocks),
                    open,
                )
                .await;
                let consent = capture(&f.state, "ws", &script).unwrap().consent();
                f.state
                    .update_workspace_config("ws", |workspace| {
                        if phase == ScriptPhase::Condition {
                            workspace.schedule.condition_consent = Some(consent);
                        } else {
                            workspace.schedule.before = Some(script);
                            workspace.schedule.before_consent = Some(consent);
                        }
                        Ok(())
                    })
                    .unwrap();
                let wall = std::sync::atomic::AtomicI64::new(open);
                let boundary = tokio::sync::Notify::new();
                let release = tokio::sync::Notify::new();
                let execute = |snapshot: CapturedScript| {
                    let f = &f;
                    let wall = &wall;
                    let boundary = &boundary;
                    let release = &release;
                    async move {
                        // Earlier eligibility passed. Hold final setup until the
                        // lease is revoked, then use the actual sandbox boundary.
                        boundary.notify_one();
                        release.notified().await;
                        snapshot
                            .execute_with_clock(&f.state, "ws", || wall.load(Ordering::SeqCst))
                            .await
                    }
                };
                let revoke = async {
                    boundary.notified().await;
                    revoke_start(&f, reason, &wall).await;
                    release.notify_one();
                };
                if phase == ScriptPhase::Condition {
                    let attempt = condition_attempt(
                        &f.state,
                        "ws",
                        &f.pool,
                        &f.session.id,
                        &f.lease,
                        execute,
                        || wall.load(Ordering::SeqCst),
                    );
                    let (result, ()) = tokio::join!(attempt, revoke);
                    let outcome = result.unwrap();
                    assert!(matches!(outcome, ConditionOutcome::Abandoned));
                    let model = crate::agents::runner::run_condition_then(
                        async { outcome },
                        &f.pool,
                        &f.run.id,
                        |_| {},
                        || async { panic!("abandoned condition entered model") },
                    )
                    .await;
                    assert!(matches!(
                        model,
                        Err(crate::agents::runner::RunnerError::ScheduledDispatchAbandoned)
                    ));
                } else {
                    let attempt = crate::agents::hooks::run_prepared_with_executor(
                        &f.state,
                        "ws",
                        &f.pool,
                        &f.session.id,
                        &f.run.id,
                        &f.lease,
                        tokio_util::sync::CancellationToken::new(),
                        |_| {},
                        |snapshot, phase| {
                            assert_eq!(phase, ScriptPhase::Before);
                            execute(snapshot)
                        },
                        || async { panic!("abandoned before entered model") },
                    );
                    let (result, ()) = tokio::join!(attempt, revoke);
                    assert!(matches!(
                        result,
                        Err(crate::agents::runner::RunnerError::ScheduledDispatchAbandoned)
                    ));
                }
                assert!(
                    !f.root.join("forbidden-start").exists(),
                    "{phase:?}: {reason}"
                );
                assert!(!f.lease.is_committed());
                assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
            }
        }
    }

    #[tokio::test]
    async fn condition_launch_does_not_commit_and_later_manual_revocation_discards_true_result() {
        let script = inline("printf started > condition-started");
        let f = fixture(Some(script.clone())).await;
        let consent = capture(&f.state, "ws", &script).unwrap().consent();
        f.state
            .update_workspace_config("ws", |workspace| {
                workspace.schedule.condition_consent = Some(consent);
                Ok(())
            })
            .unwrap();
        let outcome = condition_with_executor(
            &f.state,
            "ws",
            &f.pool,
            &f.session.id,
            &f.lease,
            |snapshot| async {
                let result = snapshot.execute(&f.state, "ws").await;
                assert_eq!(result.started, Some(true));
                assert_eq!(result.exit_code, Some(0));
                assert!(!f.lease.is_committed());
                f.state.scheduler.lock().await.yield_to_manual("ws");
                result
            },
        )
        .await;
        assert!(matches!(outcome, ConditionOutcome::Abandoned));
        assert!(f.root.join("condition-started").exists());
        assert!(!f.lease.is_committed());
    }

    #[tokio::test]
    async fn real_after_executes_outside_closed_start_block_and_disabled_schedule() {
        let f = fixture(None).await;
        f.state
            .update_workspace_config("ws", |workspace| {
                workspace.schedule.enabled = false;
                Ok(())
            })
            .unwrap();
        f.state
            .scheduler
            .lock()
            .await
            .set_instance_enabled("main", false);
        let result = capture(&f.state, "ws", &inline("printf cleaned > after-started"))
            .unwrap()
            .execute_with_clock(&f.state, "ws", || i64::MAX)
            .await;
        assert_eq!(result.started, Some(true));
        assert_eq!(result.exit_code, Some(0));
        assert!(f.root.join("after-started").exists());
    }
    #[tokio::test]
    async fn recovery_records_completed_timeout_once_and_does_not_retry_it() {
        let f = fixture(None).await;
        let intent = interrupted_cleanup(&f, &inline("exit 0"), true).await;
        crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
            ScriptResult {
                started: Some(true),
                timed_out: true,
                error: Some("cleanup timed out".into()),
                ..Default::default()
            }
        })
        .await
        .unwrap();
        let saved = workspace_config::load(&f.root).unwrap();
        assert!(saved.schedule.cleanup_intent.is_none());
        let completion = saved.schedule.cleanup_completion.unwrap();
        assert_eq!(completion.intent_id, intent.id);
        assert!(completion.result.unwrap().timed_out);
        *f.state.scheduler.lock().await = crate::agents::scheduler::Scheduler::new();
        crate::agents::cleanup_recovery::recover_with_executor(&f.state, "ws", |_| async {
            panic!("completed timeout retried")
        })
        .await
        .unwrap();
        assert!(!f.state.scheduler.lock().await.script_is_running("ws"));
    }
}
