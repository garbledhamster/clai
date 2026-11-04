//! Schedule settings script tests and exact-content per-run consent.
use crate::agents::scripts::{self, ScriptApproval, ScriptConfig, ScriptResult};
use crate::assistant::{dispatcher, engine::AssistantDeps, repository, signals};
use crate::AppState;
use tauri::{AppHandle, State};

#[tauri::command]
pub async fn workspace_script_preview(
    workspace_id: String,
    script: ScriptConfig,
    purpose: Option<String>,
    state: State<'_, AppState>,
) -> Result<ScriptApproval, String> {
    let purpose = purpose.as_deref().unwrap_or("test");
    if !matches!(purpose, "test" | "unattended") {
        return Err("Invalid script preview purpose.".into());
    }
    let snapshot = scripts::capture(state.inner(), &workspace_id, &script)?;
    let mut scheduler = state.scheduler.lock().await;
    let runtime = scheduler.scripts.entry(workspace_id).or_default();
    if runtime.running
        || runtime
            .pending
            .as_ref()
            .is_some_and(|p| p.approval.purpose == "scheduled")
    {
        return Err(
            "A scheduled condition already owns this workspace. Resolve its approval first.".into(),
        );
    }
    Ok(runtime.request(snapshot, purpose, None))
}

#[tauri::command]
pub async fn workspace_script_decide(
    workspace_id: String,
    approval_id: String,
    run: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ScriptResult>, String> {
    let pool = state.workspace_db(&workspace_id).await?;
    let Some(snapshot) =
        resolve_approval(state.inner(), &pool, &workspace_id, &approval_id, run).await?
    else {
        return Ok(None);
    };
    let result = snapshot.execute(state.inner(), &workspace_id).await;
    {
        let mut scheduler = state.scheduler.lock().await;
        let runtime = scheduler.scripts.entry(workspace_id.clone()).or_default();
        runtime.running = false;
        runtime.error = result.error.clone();
        runtime.result = Some(result.clone());
    }
    // Manual input queued during a test starts after the script finishes.
    let deps = AssistantDeps {
        workspace_id,
        pool: pool.clone(),
        app,
    };
    for session_id in signals::sessions_with_pending(&pool).await? {
        dispatcher::kick_logged(&deps, &session_id).await;
    }
    Ok(Some(result))
}

pub(crate) async fn resolve_approval(
    state: &AppState,
    pool: &crate::db::DbPool,
    workspace_id: &str,
    approval_id: &str,
    run: bool,
) -> Result<Option<scripts::CapturedScript>, String> {
    let snapshot = {
        let mut scheduler = state.scheduler.lock().await;
        let pending = scheduler
            .scripts
            .get_mut(workspace_id)
            .and_then(|r| r.pending.take())
            .ok_or("This script approval is no longer pending.")?;
        if pending.approval.id != approval_id {
            scheduler
                .scripts
                .entry(workspace_id.to_owned())
                .or_default()
                .pending = Some(pending);
            return Err("This script approval is stale. Review the current request.".into());
        }
        if !run {
            if let Some(decision) = pending.decision {
                let _ = decision.send(None);
            }
            return Ok(None);
        }
        if pending.approval.purpose == "unattended" {
            scheduler
                .scripts
                .entry(workspace_id.to_owned())
                .or_default()
                .pending = Some(pending);
            return Err("Use Approve unattended to confirm this request.".into());
        }
        let current = scripts::capture(state, workspace_id, &pending.snapshot.config);
        let current = match current {
            Ok(current) if pending.snapshot.same_approval(&current) => current,
            Ok(current) => {
                scheduler
                    .scripts
                    .entry(workspace_id.to_owned())
                    .or_default()
                    .request(current, &pending.approval.purpose, pending.decision);
                return Err("Script contents, interpreter or effective access changed. Review the new approval.".into());
            }
            Err(error) => {
                let runtime = scheduler
                    .scripts
                    .entry(workspace_id.to_owned())
                    .or_default();
                runtime.error = Some(error.clone());
                runtime.pending = Some(pending);
                return Err(error);
            }
        };
        if let Some(decision) = pending.decision {
            let _ = decision.send(Some(pending.snapshot));
            return Ok(None);
        }
        if scheduler.workspace_has_scheduled_claim(workspace_id)
            || repository::workspace_has_active_run(pool).await?
        {
            scheduler
                .scripts
                .entry(workspace_id.to_owned())
                .or_default()
                .pending = Some(pending);
            return Err("The workspace is busy. Wait for its current run before testing.".into());
        }
        scheduler
            .scripts
            .entry(workspace_id.to_owned())
            .or_default()
            .running = true;
        current
    };
    Ok(Some(snapshot))
}

/// Confirms an exact draft for atomic settings Save, or renews a waiting scheduled condition.
#[tauri::command]
pub async fn workspace_script_authorize(
    workspace_id: String,
    approval_id: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    authorize_condition(state.inner(), &workspace_id, &approval_id).await
}

pub(crate) async fn authorize_condition(
    state: &AppState,
    workspace_id: &str,
    approval_id: &str,
) -> Result<String, String> {
    let mut scheduler = state.scheduler.lock().await;
    let runtime = scheduler
        .scripts
        .entry(workspace_id.to_owned())
        .or_default();
    let pending = runtime
        .pending
        .take()
        .ok_or("This script approval is no longer pending.")?;
    if pending.approval.id != approval_id || pending.approval.purpose == "test" {
        runtime.pending = Some(pending);
        return Err("Review the current unattended or scheduled approval first.".into());
    }
    let current = match scripts::capture(state, workspace_id, &pending.snapshot.config) {
        Ok(current) if current.same_approval(&pending.snapshot) => current,
        Ok(current) => {
            runtime.request(current, &pending.approval.purpose, pending.decision);
            return Err("Script contents, Main agent, interpreter or effective access changed. Review the new approval.".into());
        }
        Err(error) => {
            runtime.error = Some(error.clone());
            runtime.pending = Some(pending);
            return Err(error);
        }
    };
    if pending.approval.purpose == "scheduled" {
        let saved = state.update_workspace_config(workspace_id, |workspace| {
            if workspace.schedule.condition.as_ref() != Some(&current.config) {
                return Err("The saved condition changed. Review its new approval.".into());
            }
            let fresh = scripts::capture_for_config(state, workspace, &current.config)?;
            if !fresh.same_approval(&current) {
                return Err(
                    "Main agent or effective access changed. Review a fresh approval.".into(),
                );
            }
            workspace.schedule.condition_consent = Some(fresh.consent());
            Ok(())
        });
        if let Err(error) = saved {
            runtime.pending = Some(pending);
            runtime.error = Some(error.clone());
            return Err(error);
        }
        if let Some(decision) = pending.decision {
            let _ = decision.send(Some(current));
        }
    } else {
        runtime.confirmed = Some((approval_id.to_owned(), current));
    }
    runtime.error = None;
    Ok(approval_id.to_owned())
}
