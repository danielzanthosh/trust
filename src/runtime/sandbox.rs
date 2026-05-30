use std::collections::BTreeSet;

use std::env;

use std::fs;

use std::path::{Component, Path, PathBuf};

use std::process::{Command, Stdio};

use std::thread;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot};

use crate::types::*;

pub(crate) fn absolute_path_from(base: &Path, path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);

    if candidate.is_absolute() {
        candidate
    } else {
        base.join(candidate)
    }
}

pub(crate) fn display_path(path: &Path) -> String {
    match env::current_dir() {
        Ok(current_dir) => match path.strip_prefix(&current_dir) {
            Ok(relative) => relative.display().to_string(),

            Err(_) => path.display().to_string(),
        },

        Err(_) => path.display().to_string(),
    }
}

pub(crate) fn validate_relative_path(path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(path.trim());

    if candidate.as_os_str().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    if candidate.is_absolute() {
        return Err("Blocked unsafe path: absolute paths are not allowed".to_string());
    }

    let mut cleaned = PathBuf::new();

    for component in candidate.components() {
        match component {
            Component::CurDir => {}

            Component::Normal(part) => cleaned.push(part),

            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Blocked unsafe path: path traversal is not allowed".to_string());
            }
        }
    }

    if cleaned.as_os_str().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    Ok(cleaned)
}

pub(crate) fn resolve_sandbox_path(
    config: &SandboxConfig,
    requested_path: &str,
) -> Result<PathBuf, String> {
    let normalized = requested_path.trim().replace('\\', "/");

    if normalized.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let (base, suffix) = if normalized == "workspace" {
        (&config.workspace, "")
    } else if let Some(suffix) = normalized.strip_prefix("workspace/") {
        (&config.workspace, suffix)
    } else if normalized == "outputs" {
        (&config.outputs, "")
    } else if let Some(suffix) = normalized.strip_prefix("outputs/") {
        (&config.outputs, suffix)
    } else if normalized == "temp" {
        (&config.temp, "")
    } else if let Some(suffix) = normalized.strip_prefix("temp/") {
        (&config.temp, suffix)
    } else {
        (&config.workspace, normalized.as_str())
    };

    if suffix.is_empty() {
        return Ok(base.to_path_buf());
    }

    Ok(base.join(validate_relative_path(suffix)?))
}

pub(crate) fn should_skip_project_entry(relative_path: &Path) -> bool {
    let normalized = relative_path.to_string_lossy().replace('\\', "/");

    normalized == "sandbox"
        || normalized.starts_with("sandbox/")
        || normalized == "target"
        || normalized.starts_with("target/")
        || normalized == "memory"
        || normalized.starts_with("memory/")
        || normalized == ".git"
        || normalized.starts_with(".git/")
        || normalized == ".env"
}

pub(crate) fn copy_project_tree(
    source: &Path,
    destination: &Path,
    project_root: &Path,
) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| {
        format!(
            "Failed to create sandbox directory {}: {}",
            display_path(destination),
            e
        )
    })?;

    let entries = fs::read_dir(source).map_err(|e| {
        format!(
            "Failed to read project directory {}: {}",
            display_path(source),
            e
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read project entry: {}", e))?;

        let source_path = entry.path();

        let relative_path = source_path
            .strip_prefix(project_root)
            .map_err(|e| format!("Failed to compute relative path for sandbox copy: {}", e))?;

        if should_skip_project_entry(relative_path) {
            continue;
        }

        let destination_path = destination.join(relative_path);

        let file_type = entry
            .file_type()
            .map_err(|e| format!("Failed to inspect {}: {}", display_path(&source_path), e))?;

        if file_type.is_dir() {
            copy_project_tree(&source_path, destination, project_root)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "Failed to create sandbox parent directory {}: {}",
                        display_path(parent),
                        e
                    )
                })?;
            }

            fs::copy(&source_path, &destination_path).map_err(|e| {
                format!(
                    "Failed to copy {} into sandbox: {}",
                    display_path(&source_path),
                    e
                )
            })?;
        }
    }

    Ok(())
}

pub(crate) fn seed_sandbox_workspace() -> bool {
    env::var("SEED_SANDBOX_WORKSPACE")
        .map(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub(crate) fn ensure_sandbox_ready(config: &SandboxConfig) -> Result<(), String> {
    fs::create_dir_all(&config.root).map_err(|e| {
        format!(
            "Failed to create sandbox root {}: {}",
            display_path(&config.root),
            e
        )
    })?;

    fs::create_dir_all(&config.workspace).map_err(|e| {
        format!(
            "Failed to create sandbox workspace {}: {}",
            display_path(&config.workspace),
            e
        )
    })?;

    fs::create_dir_all(&config.outputs).map_err(|e| {
        format!(
            "Failed to create sandbox outputs {}: {}",
            display_path(&config.outputs),
            e
        )
    })?;

    fs::create_dir_all(&config.temp).map_err(|e| {
        format!(
            "Failed to create sandbox temp {}: {}",
            display_path(&config.temp),
            e
        )
    })?;

    if seed_sandbox_workspace() {
        let workspace_is_empty = fs::read_dir(&config.workspace)
            .map_err(|e| {
                format!(
                    "Failed to inspect sandbox workspace {}: {}",
                    display_path(&config.workspace),
                    e
                )
            })?
            .next()
            .is_none();

        if workspace_is_empty {
            let project_root = env::current_dir()
                .map_err(|e| format!("Failed to resolve current project directory: {}", e))?;

            copy_project_tree(&project_root, &config.workspace, &project_root)?;
        }
    }

    Ok(())
}

pub(crate) fn permissions_dir() -> PathBuf {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("permissions")
}

pub(crate) fn allowed_commands_path() -> PathBuf {
    permissions_dir().join("allowed_commands.json")
}

pub(crate) fn load_allowed_commands() -> Result<BTreeSet<String>, String> {
    let path = allowed_commands_path();

    if !path.exists() {
        return Ok(BTreeSet::new());
    }

    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", display_path(&path), e))?;

    if content.trim().is_empty() {
        return Ok(BTreeSet::new());
    }

    let commands = serde_json::from_str::<Vec<String>>(&content)
        .map_err(|e| format!("Failed to parse {}: {}", display_path(&path), e))?;

    Ok(commands
        .into_iter()
        .map(|command| command.trim().to_string())
        .filter(|command| !command.is_empty())
        .collect())
}

pub(crate) fn save_allowed_commands(commands: &BTreeSet<String>) -> Result<(), String> {
    let dir = permissions_dir();

    fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "Failed to create permissions directory {}: {}",
            display_path(&dir),
            e
        )
    })?;

    let path = allowed_commands_path();

    let ordered = commands.iter().cloned().collect::<Vec<_>>();

    let content = serde_json::to_string_pretty(&ordered)
        .map_err(|e| format!("Failed to serialize allowed commands: {}", e))?;

    fs::write(&path, content).map_err(|e| format!("Failed to write {}: {}", display_path(&path), e))
}

pub(crate) fn normalize_command(command: &str) -> String {
    command.trim().to_string()
}

pub(crate) fn command_log_path() -> PathBuf {
    permissions_dir().join("command_log.jsonl")
}

pub(crate) fn current_timestamp_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub(crate) fn append_command_log(
    shell: CommandShell,

    command: &str,

    permission_choice: &str,

    result: &str,

    blocked: bool,
) {
    let dir = permissions_dir();

    if fs::create_dir_all(&dir).is_err() {
        return;
    }

    let entry = CommandLogEntry {
        timestamp_unix_ms: current_timestamp_unix_ms(),

        shell: shell.label().to_string(),

        command: command.to_string(),

        permission_choice: permission_choice.to_string(),

        result: result.to_string(),

        blocked,
    };

    let Ok(mut line) = serde_json::to_string(&entry) else {
        return;
    };

    line.push('\n');

    let path = command_log_path();

    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
}

pub(crate) fn allow_destructive_actions() -> bool {
    env::var("ALLOW_DESTRUCTIVE_ACTIONS")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub(crate) fn command_contains_any(normalized_command: &str, patterns: &[&str]) -> bool {
    patterns
        .iter()
        .any(|pattern| normalized_command.contains(pattern))
}

pub(crate) fn command_uses_destructive_verb(normalized_command: &str) -> bool {
    command_contains_any(
        normalized_command,
        &[
            "remove-item",
            " rm ",
            "rm -",
            "del ",
            "erase ",
            "rmdir",
            "rd /",
            "clear-disk",
            "format",
            "diskpart",
            "bcdedit",
            "reg delete",
            "reg add",
            "set-mppreference",
            "disable-",
            "stop-service",
            "sc delete",
            "takeown",
            "icacls",
        ],
    )
}

pub(crate) fn command_targets_protected_path(
    normalized_command: &str,
    project_root: &Path,
) -> bool {
    let project_root_text = project_root
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();

    command_contains_any(
        normalized_command,
        &[
            "c:\\windows",
            "c:/windows",
            "\\windows\\system32",
            "/windows/system32",
            "system32",
            "boot.ini",
            "bootmgr",
            "bcd",
            "ntldr",
            "sam",
            "security",
            "software",
            "system32\\config",
            "system32/config",
            "hkey_local_machine",
            "hklm",
            "hkey_classes_root",
            "hkcr",
            "%userprofile%",
            "$env:userprofile",
            "c:\\users\\",
            "c:/users/",
        ],
    ) || (!project_root_text.is_empty() && normalized_command.contains(&project_root_text))
}

pub(crate) fn command_targets_entire_drive(normalized_command: &str) -> bool {
    command_contains_any(
        normalized_command,
        &[
            " c:\\ -recurse",
            " c:/ -recurse",
            " c:\\* -recurse",
            " c:/* -recurse",
            " c:\\ -force",
            " c:/ -force",
            " c:\\* -force",
            " c:/* -force",
            " remove-item / -recurse",
            " rm / -rf",
            " rm -rf /",
        ],
    )
}

pub(crate) fn destructive_command_reason(command: &str, project_root: &Path) -> Option<String> {
    let normalized = format!(" {} ", command.trim().to_lowercase());

    if normalized.contains("powershell -enc") || normalized.contains("powershell.exe -enc") {
        return Some("encoded PowerShell can hide destructive actions".to_string());
    }

    if normalized.contains("curl") && normalized.contains("| powershell") {
        return Some("downloaded script is piped directly into PowerShell".to_string());
    }

    if normalized.contains("irm") && normalized.contains("| iex") {
        return Some("downloaded script is executed with Invoke-Expression".to_string());
    }

    if command_contains_any(
        &normalized,
        &[
            "format ",
            "format-volume",
            "clear-disk",
            "initialize-disk",
            "remove-partition",
            "diskpart",
            "bcdedit",
        ],
    ) {
        return Some("may wipe drives or modify boot configuration".to_string());
    }

    if command_contains_any(
        &normalized,
        &[
            "disable-windowsdefender",
            "disable-realtimemonitoring",
            "set-mppreference -disablerealtimemonitoring $true",
            "stop-service windefend",
            "sc stop windefend",
        ],
    ) {
        return Some("may disable security tools".to_string());
    }

    if command_uses_destructive_verb(&normalized) && command_targets_entire_drive(&normalized) {
        return Some("targets an entire drive with a destructive operation".to_string());
    }

    if command_uses_destructive_verb(&normalized)
        && command_targets_protected_path(&normalized, project_root)
    {
        return Some(
            "may damage system files, boot files, registry hives, or user profile data".to_string(),
        );
    }

    None
}

pub(crate) fn blocked_command_reason(command: &str) -> Option<String> {
    if command.trim().is_empty() {
        Some("empty command".to_string())
    } else {
        None
    }
}

pub(crate) fn command_approval_signature(command: &str) -> Option<String> {
    let lower = command.trim().to_lowercase();

    let first_line = lower.lines().find(|line| !line.trim().is_empty())?.trim();

    let separators = [' ', '\t', '|', ';', '&', '\r', '\n'];

    let first_token = first_line
        .split(separators)
        .find(|token| !token.trim().is_empty())?
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '(' | ')'));

    if first_token.is_empty() || first_token.starts_with('$') {
        return None;
    }

    Some(format!("signature:powershell:{}", first_token))
}

pub(crate) fn command_is_preapproved(command: &str, allowed_commands: &BTreeSet<String>) -> bool {
    let normalized = normalize_command(command);

    allowed_commands.contains(&normalized)
        || command_approval_signature(command)
            .as_ref()
            .is_some_and(|signature| allowed_commands.contains(signature))
}

pub(crate) fn classify_command_risk(
    command: &str,
    project_root: &Path,
) -> Result<CommandRisk, String> {
    if blocked_command_reason(command).is_some() {
        return Ok(CommandRisk::Blocked);
    }

    if destructive_command_reason(command, project_root).is_some() {
        return Ok(CommandRisk::Destructive);
    }

    let allowed_commands = load_allowed_commands()?;

    if command_is_preapproved(command, &allowed_commands) {
        Ok(CommandRisk::Safe)
    } else {
        Ok(CommandRisk::NeedsApproval)
    }
}

pub(crate) fn persist_allowed_command(command: &str, project_root: &Path) -> Result<(), String> {
    if destructive_command_reason(command, project_root).is_some() {
        return Err("Destructive commands can never be saved to allowed_commands.json".to_string());
    }

    let normalized = normalize_command(command);

    let mut allowed_commands = load_allowed_commands()?;

    allowed_commands.insert(normalized);

    if let Some(signature) = command_approval_signature(command) {
        allowed_commands.insert(signature);
    }

    save_allowed_commands(&allowed_commands)
}

pub(crate) fn execute_command_with_timeout(
    sandbox: &SandboxConfig,

    program: &str,

    args: &[&str],

    display_name: &str,
) -> String {
    let cargo_target_dir = sandbox.temp.join("target");

    if let Err(e) = fs::create_dir_all(&cargo_target_dir) {
        return format!(
            "Command failed: {}\n[stderr]\nFailed to prepare sandbox target directory {}: {}",
            display_name,
            display_path(&cargo_target_dir),
            e
        );
    }

    let mut child = match Command::new(program)
        .args(args)
        .current_dir(&sandbox.workspace)
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,

        Err(e) => return format!("Command failed: {}\n[stderr]\n{}", display_name, e),
    };

    let start = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return match child.wait_with_output() {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                        if output.status.success() {
                            if stdout.is_empty() {
                                format!("Executed command: {}\nStatus: success", display_name)
                            } else {
                                format!(
                                    "Executed command: {}\nStatus: success\n[stdout]\n{}",
                                    display_name, stdout
                                )
                            }
                        } else if stdout.is_empty() && stderr.is_empty() {
                            format!(
                                "Command failed: {}\nStatus: {}",
                                display_name, output.status
                            )
                        } else if stdout.is_empty() {
                            format!("Command failed: {}\n[stderr]\n{}", display_name, stderr)
                        } else if stderr.is_empty() {
                            format!("Command failed: {}\n[stdout]\n{}", display_name, stdout)
                        } else {
                            format!(
                                "Command failed: {}\n[stdout]\n{}\n\n[stderr]\n{}",
                                display_name, stdout, stderr
                            )
                        }
                    }

                    Err(e) => format!(
                        "Command failed: {}\n[stderr]\nFailed to collect output: {}",
                        display_name, e
                    ),
                };
            }

            Ok(None) => {
                if start.elapsed() >= Duration::from_millis(sandbox.command_timeout_ms) {
                    let _ = child.kill();

                    let _ = child.wait();

                    return format!(
                        "Command timed out: {}\nTimeout: {} ms",
                        display_name, sandbox.command_timeout_ms
                    );
                }

                thread::sleep(Duration::from_millis(100));
            }

            Err(e) => {
                return format!(
                    "Command failed: {}\n[stderr]\nFailed while waiting on command: {}",
                    display_name, e
                );
            }
        }
    }
}

pub(crate) fn execute_powershell_with_timeout(sandbox: &SandboxConfig, command: &str) -> String {
    let shell = CommandShell::PowerShell;

    let display_name = command.trim();

    let args = shell.args(command);

    let mut child = match Command::new(shell.program())
        .args(&args)
        .current_dir(&sandbox.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,

        Err(e) => return format!("Command failed: {}\n[stderr]\n{}", display_name, e),
    };

    let start = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return match child.wait_with_output() {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                        if output.status.success() {
                            if stdout.is_empty() {
                                format!(
                                    "Executed PowerShell command:\n{}\nStatus: success",
                                    display_name
                                )
                            } else {
                                format!(
                                    "Executed PowerShell command:\n{}\nStatus: success\n[stdout]\n{}",
                                    display_name, stdout
                                )
                            }
                        } else if stdout.is_empty() && stderr.is_empty() {
                            format!(
                                "PowerShell command failed:\n{}\nStatus: {}",
                                display_name, output.status
                            )
                        } else if stdout.is_empty() {
                            format!(
                                "PowerShell command failed:\n{}\n[stderr]\n{}",
                                display_name, stderr
                            )
                        } else if stderr.is_empty() {
                            format!(
                                "PowerShell command failed:\n{}\n[stdout]\n{}",
                                display_name, stdout
                            )
                        } else {
                            format!(
                                "PowerShell command failed:\n{}\n[stdout]\n{}\n\n[stderr]\n{}",
                                display_name, stdout, stderr
                            )
                        }
                    }

                    Err(e) => format!(
                        "PowerShell command failed:\n{}\n[stderr]\nFailed to collect output: {}",
                        display_name, e
                    ),
                };
            }

            Ok(None) => {
                if start.elapsed() >= Duration::from_millis(sandbox.command_timeout_ms) {
                    let _ = child.kill();

                    let _ = child.wait();

                    return format!(
                        "PowerShell command timed out:\n{}\nTimeout: {} ms",
                        display_name, sandbox.command_timeout_ms
                    );
                }

                thread::sleep(Duration::from_millis(100));
            }

            Err(e) => {
                return format!(
                    "PowerShell command failed:\n{}\n[stderr]\nFailed while waiting on command: {}",
                    display_name, e
                );
            }
        }
    }
}

pub(crate) async fn request_permission(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,

    title: &str,

    command: &str,

    risk_label: &str,

    allow_always: bool,
) -> PermissionChoice {
    let (response_tx, response_rx) = oneshot::channel();

    let pending = PendingApproval {
        title: title.to_string(),

        command: command.to_string(),

        risk_label: risk_label.to_string(),

        allow_always,

        responder: response_tx,
    };

    if event_tx
        .send(RuntimeEvent::ApprovalRequested(pending))
        .is_err()
    {
        return PermissionChoice::Decline;
    }

    response_rx.await.unwrap_or(PermissionChoice::Decline)
}

pub(crate) async fn run_command(
    sandbox: &SandboxConfig,

    command: &str,

    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> String {
    let shell = CommandShell::PowerShell;

    let normalized = normalize_command(command);

    if normalized.is_empty() {
        let result = "Blocked command: empty command".to_string();

        append_command_log(shell, &normalized, "blocked", &result, true);

        return result;
    }

    let project_root = match env::current_dir() {
        Ok(path) => path,

        Err(e) => {
            let result = format!("Blocked command: failed to resolve project root: {}", e);

            append_command_log(shell, &normalized, "blocked", &result, true);

            return result;
        }
    };

    match classify_command_risk(&normalized, &project_root) {
        Ok(CommandRisk::Blocked) => {
            let reason = blocked_command_reason(&normalized)
                .unwrap_or_else(|| "blocked by runtime policy".to_string());

            let result = format!("Blocked command: {} ({})", normalized, reason);

            append_command_log(shell, &normalized, "blocked", &result, true);

            result
        }

        Ok(CommandRisk::Safe) => {
            let result = execute_powershell_with_timeout(sandbox, &normalized);

            append_command_log(shell, &normalized, "preapproved", &result, false);

            result
        }

        Ok(CommandRisk::NeedsApproval) => {
            let choice = request_permission(
                event_tx,
                "PowerShell command requested",
                &normalized,
                "NeedsApproval",
                true,
            )
            .await;

            match choice {
                PermissionChoice::AllowOnce => {
                    let result = execute_powershell_with_timeout(sandbox, &normalized);

                    append_command_log(shell, &normalized, "allow_once", &result, false);

                    result
                }

                PermissionChoice::AllowAlways => {
                    let result = match persist_allowed_command(&normalized, &project_root) {
                        Ok(()) => execute_powershell_with_timeout(sandbox, &normalized),

                        Err(error) => {
                            format!("Blocked command: failed to persist approval ({})", error)
                        }
                    };

                    let blocked = result.starts_with("Blocked command:");

                    append_command_log(shell, &normalized, "allow_always", &result, blocked);

                    result
                }

                PermissionChoice::Decline => {
                    let result = format!("User declined command: {}", normalized);

                    append_command_log(shell, &normalized, "decline", &result, true);

                    result
                }
            }
        }

        Ok(CommandRisk::Destructive) => {
            let reason = destructive_command_reason(&normalized, &project_root)
                .unwrap_or_else(|| "matched destructive runtime policy".to_string());

            if !allow_destructive_actions() {
                let result = format!(
                    "This command was blocked because it may damage system files.\nReason: {}\nSafer alternative: narrow the command to a non-system path, preview affected items with Get-ChildItem, or ask for a read-only diagnostic command first.\nCommand:\n{}",
                    reason, normalized
                );

                append_command_log(shell, &normalized, "blocked", &result, true);

                return result;
            }

            let choice = request_permission(
                event_tx,
                "Destructive PowerShell command requested",
                &normalized,
                "Destructive",
                false,
            )
            .await;

            match choice {
                PermissionChoice::AllowOnce => {
                    let result = execute_powershell_with_timeout(sandbox, &normalized);

                    append_command_log(
                        shell,
                        &normalized,
                        "allow_once_destructive",
                        &result,
                        false,
                    );

                    result
                }

                PermissionChoice::AllowAlways | PermissionChoice::Decline => {
                    let result = format!("User declined command: {}", normalized);

                    append_command_log(shell, &normalized, "decline_destructive", &result, true);

                    result
                }
            }
        }

        Err(error) => {
            let result = format!(
                "Blocked command: failed to classify command risk ({})",
                error
            );

            append_command_log(shell, &normalized, "blocked", &result, true);

            result
        }
    }
}
