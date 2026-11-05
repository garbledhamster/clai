use tokio::process::Command;

use super::runner::{prepare_stdio, run_spawned_child, spawn_checked};
use super::SandboxCommand;
use crate::windows_console::HideConsoleWindow;

pub async fn run(command: SandboxCommand) -> Result<super::SandboxCommandOutput, String> {
    let mut argv = command.argv.iter();
    let program = argv
        .next()
        .ok_or_else(|| "Sandbox command argv cannot be empty".to_string())?;
    let mut child_command = Command::new(program);
    child_command.args(argv).current_dir(&command.cwd);
    if command.profile.variables.is_some() {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("CLAI_VAR_") {
                child_command.env_remove(key);
            }
        }
    }
    // Shell commands run headless with piped stdio; on Windows an unflagged
    // console child would flash a console window per bash_exec invocation.
    child_command.hide_console_window();
    prepare_stdio(&mut child_command);

    let child = spawn_checked(
        &mut child_command,
        None,
        command.profile.variables.as_ref(),
        |error| format!("Failed to start shell command: {error}"),
    )?
    .ok_or("Shell command admission was abandoned")?;

    run_spawned_child(
        child,
        command.cwd,
        command.timeout_ms,
        command.max_output_chars,
        "Shell command",
    )
    .await
}
