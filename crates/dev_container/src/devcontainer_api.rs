use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use futures::channel::mpsc::UnboundedSender;
use node_runtime::NodeRuntime;
use remote::{DockerConnectionOptions, DockerHost, RemoteConnectionOptions};
use serde::Deserialize;
use settings::{DevContainerConnection, DevContainerHost};
use smol::fs;
use util::command::Command;
use util::rel_path::RelPath;
use util::shell::ShellKind;
use workspace::Workspace;
use worktree::Snapshot;

use crate::{DevContainerContext, DevContainerFeature, DevContainerTemplate};

/// Represents a discovered devcontainer configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevContainerConfig {
    /// Display name for the configuration (subfolder name or "default")
    pub name: String,
    /// Relative path to the devcontainer.json file from the project root
    pub config_path: PathBuf,
}

impl DevContainerConfig {
    pub fn default_config() -> Self {
        Self {
            name: "default".to_string(),
            config_path: PathBuf::from(".devcontainer/devcontainer.json"),
        }
    }

    pub fn root_config() -> Self {
        Self {
            name: "root".to_string(),
            config_path: PathBuf::from(".devcontainer.json"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevContainerUp {
    _outcome: String,
    container_id: String,
    remote_user: String,
    remote_workspace_folder: String,
}

#[derive(Debug, Deserialize)]
struct DockerMount {
    #[serde(rename = "Destination")]
    destination: String,
    #[serde(rename = "Source")]
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevContainerApply {
    pub(crate) files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevContainerConfiguration {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DevContainerConfigurationOutput {
    configuration: DevContainerConfiguration,
}

pub(crate) struct DevContainerCli {
    pub path: PathBuf,
    node_runtime_path: Option<PathBuf>,
}

impl DevContainerCli {
    fn command(&self, use_podman: bool) -> Command {
        let mut command = if let Some(node_runtime_path) = &self.node_runtime_path {
            let mut command =
                util::command::new_command(node_runtime_path.as_os_str().display().to_string());
            command.arg(self.path.display().to_string());
            command
        } else {
            util::command::new_command(self.path.display().to_string())
        };

        if use_podman {
            command.arg("--docker-path");
            command.arg("podman");
        }
        command
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevContainerError {
    DockerNotAvailable,
    DevContainerCliNotAvailable,
    DevContainerTemplateApplyFailed(String),
    DevContainerUpFailed(String),
    DevContainerNotFound,
    DevContainerParseFailed,
    NodeRuntimeNotAvailable,
    NotInValidProject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevContainerBuildStep {
    CheckDocker,
    CheckDevcontainerCli,
    DevcontainerUp,
    ReadConfiguration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevContainerLogStream {
    Stdout,
    Stderr,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevContainerLogLine {
    pub stream: DevContainerLogStream,
    pub line: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevContainerProgressEvent {
    StepStarted(DevContainerBuildStep),
    StepCompleted(DevContainerBuildStep),
    StepFailed(DevContainerBuildStep, String),
    LogLine(DevContainerLogLine),
}

impl Display for DevContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                DevContainerError::DockerNotAvailable =>
                    "docker CLI not found on $PATH".to_string(),
                DevContainerError::DevContainerCliNotAvailable =>
                    "devcontainer CLI not found on path".to_string(),
                DevContainerError::DevContainerUpFailed(_) => {
                    "DevContainer creation failed".to_string()
                }
                DevContainerError::DevContainerTemplateApplyFailed(_) => {
                    "DevContainer template apply failed".to_string()
                }
                DevContainerError::DevContainerNotFound =>
                    "No valid dev container definition found in project".to_string(),
                DevContainerError::DevContainerParseFailed =>
                    "Failed to parse file .devcontainer/devcontainer.json".to_string(),
                DevContainerError::NodeRuntimeNotAvailable =>
                    "Cannot find a valid node runtime".to_string(),
                DevContainerError::NotInValidProject => "Not within a valid project".to_string(),
            }
        )
    }
}

/// Finds all available devcontainer configurations in the project.
///
/// See [`find_configs_in_snapshot`] for the locations that are scanned.
pub fn find_devcontainer_configs(workspace: &Workspace, cx: &gpui::App) -> Vec<DevContainerConfig> {
    let project = workspace.project().read(cx);

    let worktree = project
        .visible_worktrees(cx)
        .find_map(|tree| tree.read(cx).root_entry()?.is_dir().then_some(tree));

    let Some(worktree) = worktree else {
        log::debug!("find_devcontainer_configs: No worktree found");
        return Vec::new();
    };

    let worktree = worktree.read(cx);
    find_configs_in_snapshot(worktree)
}

/// Scans a worktree snapshot for devcontainer configurations.
///
/// Scans for configurations in these locations:
/// 1. `.devcontainer/devcontainer.json` (the default location)
/// 2. `.devcontainer.json` in the project root
/// 3. `.devcontainer/<subfolder>/devcontainer.json` (named configurations)
///
/// All found configurations are returned so the user can pick between them.
pub fn find_configs_in_snapshot(snapshot: &Snapshot) -> Vec<DevContainerConfig> {
    let mut configs = Vec::new();

    let devcontainer_dir_path = RelPath::unix(".devcontainer").expect("valid path");

    if let Some(devcontainer_entry) = snapshot.entry_for_path(devcontainer_dir_path) {
        if devcontainer_entry.is_dir() {
            log::debug!("find_configs_in_snapshot: Scanning .devcontainer directory");
            let devcontainer_json_path =
                RelPath::unix(".devcontainer/devcontainer.json").expect("valid path");
            for entry in snapshot.child_entries(devcontainer_dir_path) {
                log::debug!(
                    "find_configs_in_snapshot: Found entry: {:?}, is_file: {}, is_dir: {}",
                    entry.path.as_unix_str(),
                    entry.is_file(),
                    entry.is_dir()
                );

                if entry.is_file() && entry.path.as_ref() == devcontainer_json_path {
                    log::debug!("find_configs_in_snapshot: Found default devcontainer.json");
                    configs.push(DevContainerConfig::default_config());
                } else if entry.is_dir() {
                    let subfolder_name = entry
                        .path
                        .file_name()
                        .map(|n| n.to_string())
                        .unwrap_or_default();

                    let config_json_path =
                        format!("{}/devcontainer.json", entry.path.as_unix_str());
                    if let Ok(rel_config_path) = RelPath::unix(&config_json_path) {
                        if snapshot.entry_for_path(rel_config_path).is_some() {
                            log::debug!(
                                "find_configs_in_snapshot: Found config in subfolder: {}",
                                subfolder_name
                            );
                            configs.push(DevContainerConfig {
                                name: subfolder_name,
                                config_path: PathBuf::from(&config_json_path),
                            });
                        } else {
                            log::debug!(
                                "find_configs_in_snapshot: Subfolder {} has no devcontainer.json",
                                subfolder_name
                            );
                        }
                    }
                }
            }
        }
    }

    // Always include `.devcontainer.json` so the user can pick it from the UI
    // even when `.devcontainer/devcontainer.json` also exists.
    let root_config_path = RelPath::unix(".devcontainer.json").expect("valid path");
    if snapshot
        .entry_for_path(root_config_path)
        .is_some_and(|entry| entry.is_file())
    {
        log::debug!("find_configs_in_snapshot: Found .devcontainer.json in project root");
        configs.push(DevContainerConfig::root_config());
    }

    log::info!(
        "find_configs_in_snapshot: Found {} configurations",
        configs.len()
    );

    configs.sort_by(|a, b| {
        let a_is_primary = a.name == "default" || a.name == "root";
        let b_is_primary = b.name == "default" || b.name == "root";
        match (a_is_primary, b_is_primary) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        }
    });

    configs
}

pub async fn start_dev_container_with_config(
    context: DevContainerContext,
    config: Option<DevContainerConfig>,
) -> Result<(DevContainerConnection, String), DevContainerError> {
    start_dev_container_with_progress(context, config, None).await
}

pub async fn start_dev_container_with_progress(
    context: DevContainerContext,
    config: Option<DevContainerConfig>,
    progress_tx: Option<UnboundedSender<DevContainerProgressEvent>>,
) -> Result<(DevContainerConnection, String), DevContainerError> {
    let send_progress =
        |event: DevContainerProgressEvent,
         progress_tx: &Option<UnboundedSender<DevContainerProgressEvent>>| {
            if let Some(tx) = progress_tx {
                let _ = tx.unbounded_send(event);
            }
        };

    let log_info =
        |line: &str, progress_tx: &Option<UnboundedSender<DevContainerProgressEvent>>| {
            send_progress(
                DevContainerProgressEvent::LogLine(DevContainerLogLine {
                    stream: DevContainerLogStream::Info,
                    line: line.to_string(),
                }),
                progress_tx,
            );
        };

    send_progress(
        DevContainerProgressEvent::StepStarted(DevContainerBuildStep::CheckDocker),
        &progress_tx,
    );
    log_info("Checking Docker/Podman availability...", &progress_tx);
    if let Err(err) = check_for_docker(&context).await {
        let message = devcontainer_error_detail(&err);
        send_progress(
            DevContainerProgressEvent::StepFailed(DevContainerBuildStep::CheckDocker, message),
            &progress_tx,
        );
        return Err(err);
    }
    send_progress(
        DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::CheckDocker),
        &progress_tx,
    );

    send_progress(
        DevContainerProgressEvent::StepStarted(DevContainerBuildStep::CheckDevcontainerCli),
        &progress_tx,
    );
    log_info("Checking Dev Container CLI...", &progress_tx);
    let cli = match ensure_devcontainer_cli(&context).await {
        Ok(cli) => cli,
        Err(err) => {
            let message = devcontainer_error_detail(&err);
            send_progress(
                DevContainerProgressEvent::StepFailed(
                    DevContainerBuildStep::CheckDevcontainerCli,
                    message,
                ),
                &progress_tx,
            );
            return Err(err);
        }
    };
    send_progress(
        DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::CheckDevcontainerCli),
        &progress_tx,
    );

    send_progress(
        DevContainerProgressEvent::StepStarted(DevContainerBuildStep::DevcontainerUp),
        &progress_tx,
    );
    log_info("Running devcontainer up...", &progress_tx);
    let host_project_directory = match resolve_project_directory_on_host(&context).await {
        Ok(path) => path,
        Err(err) => {
            let message = devcontainer_error_detail(&err);
            send_progress(
                DevContainerProgressEvent::StepFailed(
                    DevContainerBuildStep::DevcontainerUp,
                    message,
                ),
                &progress_tx,
            );
            return Err(err);
        }
    };
    let config_path = config.map(|c| {
        join_config_path(
            host_project_directory.as_path(),
            c.config_path.as_path(),
            context.remote_connection.is_some(),
        )
    });
    let DevContainerUp {
        container_id,
        remote_workspace_folder,
        remote_user,
        ..
    } = match devcontainer_up(
        &context,
        cli.as_ref(),
        host_project_directory.as_path(),
        config_path.as_deref(),
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            let message = format!("Failed with nested error: {}", err);
            send_progress(
                DevContainerProgressEvent::StepFailed(
                    DevContainerBuildStep::DevcontainerUp,
                    message.clone(),
                ),
                &progress_tx,
            );
            return Err(DevContainerError::DevContainerUpFailed(message));
        }
    };
    send_progress(
        DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::DevcontainerUp),
        &progress_tx,
    );

    send_progress(
        DevContainerProgressEvent::StepStarted(DevContainerBuildStep::ReadConfiguration),
        &progress_tx,
    );
    log_info("Reading devcontainer configuration...", &progress_tx);
    let project_name =
        match read_devcontainer_configuration(&context, cli.as_ref(), config_path.as_deref()).await
        {
            Ok(DevContainerConfigurationOutput {
                configuration:
                    DevContainerConfiguration {
                        name: Some(project_name),
                    },
            }) => project_name,
            _ => get_backup_project_name(&remote_workspace_folder, &container_id),
        };
    send_progress(
        DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::ReadConfiguration),
        &progress_tx,
    );

    let connection = DevContainerConnection {
        name: project_name,
        container_id,
        use_podman: context.use_podman,
        remote_user,
        config_path: config_path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        projects: Default::default(),
        host_projects: Default::default(),
        host: context
            .remote_connection
            .as_ref()
            .and_then(devcontainer_host_from_remote_options),
    };

    Ok((connection, remote_workspace_folder))
}

fn devcontainer_host_from_remote_options(
    options: &RemoteConnectionOptions,
) -> Option<DevContainerHost> {
    match options {
        RemoteConnectionOptions::Ssh(options) => Some(DevContainerHost::Ssh {
            host: options.host.to_string(),
            username: options.username.clone(),
            port: options.port,
            args: options.args.clone().unwrap_or_default(),
        }),
        RemoteConnectionOptions::Wsl(options) => Some(DevContainerHost::Wsl {
            distro_name: options.distro_name.clone(),
            user: options.user.clone(),
        }),
        RemoteConnectionOptions::Docker(_) => None,
        RemoteConnectionOptions::Mock(_) => None,
    }
}

fn host_remote_options_for_docker_host(host: &DockerHost) -> Option<RemoteConnectionOptions> {
    match host {
        DockerHost::Local => None,
        DockerHost::Wsl(options) => Some(RemoteConnectionOptions::Wsl(options.clone())),
        DockerHost::Ssh(options) => Some(RemoteConnectionOptions::Ssh(options.clone())),
    }
}

async fn resolve_project_directory_on_host(
    context: &DevContainerContext,
) -> Result<PathBuf, DevContainerError> {
    let Some(docker_options) = context.docker_connection.as_ref() else {
        return Ok(context.project_directory.as_ref().to_path_buf());
    };

    let host_remote_options = host_remote_options_for_docker_host(&docker_options.host);
    resolve_host_directory_for_docker(
        docker_options,
        context.project_directory.as_ref(),
        host_remote_options.as_ref(),
    )
    .await
}

async fn resolve_host_directory_for_docker(
    docker_options: &DockerConnectionOptions,
    container_directory: &Path,
    host_remote_options: Option<&RemoteConnectionOptions>,
) -> Result<PathBuf, DevContainerError> {
    let mounts = docker_inspect_mounts(docker_options, host_remote_options).await?;
    let host_path = host_path_from_mounts(&mounts, &container_directory.display().to_string())?;
    Ok(PathBuf::from(host_path))
}

async fn docker_inspect_mounts(
    docker_options: &DockerConnectionOptions,
    host_remote_options: Option<&RemoteConnectionOptions>,
) -> Result<Vec<DockerMount>, DevContainerError> {
    let args = vec![
        "inspect".to_string(),
        "--format".to_string(),
        "{{json .Mounts}}".to_string(),
        docker_options.container_id.clone(),
    ];
    let mut command =
        build_host_docker_command(host_remote_options, docker_options.use_podman, &args)?;

    match command.output().await {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            parse_json_array_from_cli(&raw)
        }
        Ok(output) => {
            let message = format!(
                "Non-success status running docker inspect for container: out: {:?}, err: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
        Err(e) => {
            let message = format!("Error running docker inspect: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
    }
}

fn build_host_docker_command(
    host_remote_options: Option<&RemoteConnectionOptions>,
    use_podman: bool,
    args: &[String],
) -> Result<Command, DevContainerError> {
    let docker_cli = docker_cli_name(use_podman);
    if let Some(remote_options) = host_remote_options {
        build_remote_command(remote_options, docker_cli, args, false)
    } else {
        let mut command = util::command::new_command(docker_cli);
        command.args(args);
        Ok(command)
    }
}

fn host_path_from_mounts(
    mounts: &[DockerMount],
    container_path: &str,
) -> Result<String, DevContainerError> {
    let container_path = trim_trailing_slash(container_path);
    let mut best: Option<(&DockerMount, &str)> = None;

    for mount in mounts {
        let destination = trim_trailing_slash(&mount.destination);
        let rest = container_path.strip_prefix(destination);
        let is_match = match rest {
            Some(rest) => rest.is_empty() || rest.starts_with('/'),
            None => false,
        };
        if is_match {
            let replace = best
                .as_ref()
                .map_or(true, |(_, best_dest)| destination.len() > best_dest.len());
            if replace {
                best = Some((mount, destination));
            }
        }
    }

    let Some((mount, destination)) = best else {
        return Err(DevContainerError::DevContainerUpFailed(
            "Unable to resolve host workspace path for dev container".to_string(),
        ));
    };

    let suffix = container_path.strip_prefix(destination).unwrap_or("");
    Ok(join_host_path(&mount.source, suffix))
}

fn trim_trailing_slash(path: &str) -> &str {
    let trimmed = path.trim_end_matches(&['/', '\\'][..]);
    if trimmed.is_empty() { path } else { trimmed }
}

fn join_host_path(source: &str, suffix: &str) -> String {
    let source = trim_trailing_slash(source);
    let suffix = suffix.trim_start_matches(&['/', '\\'][..]);
    if suffix.is_empty() {
        return source.to_string();
    }

    let is_windows = source.contains('\\') || source.contains(':');
    let sep = if is_windows { '\\' } else { '/' };
    let mut base = source.to_string();
    if !base.ends_with(sep) && !base.ends_with('/') && !base.ends_with('\\') {
        base.push(sep);
    }
    let tail = if is_windows {
        suffix.replace('/', "\\")
    } else {
        suffix.to_string()
    };
    base.push_str(&tail);
    base
}

fn normalize_remote_path_arg(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn join_config_path(project_directory: &Path, config_path: &Path, remote_host: bool) -> PathBuf {
    if !remote_host {
        return project_directory.join(config_path);
    }

    let base = normalize_remote_path_arg(project_directory);
    let rel = normalize_remote_path_arg(config_path);
    let joined = if rel.is_empty() {
        base
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            rel.trim_start_matches('/')
        )
    };
    PathBuf::from(joined)
}

fn devcontainer_error_detail(error: &DevContainerError) -> String {
    match error {
        DevContainerError::DockerNotAvailable => "docker CLI not found on $PATH".to_string(),
        DevContainerError::DevContainerCliNotAvailable => {
            "devcontainer CLI not found on path".to_string()
        }
        DevContainerError::DevContainerTemplateApplyFailed(message) => {
            format!("DevContainer template apply failed: {message}")
        }
        DevContainerError::DevContainerUpFailed(message) => {
            format!("DevContainer creation failed: {message}")
        }
        DevContainerError::DevContainerNotFound => {
            "No valid dev container definition found in project".to_string()
        }
        DevContainerError::DevContainerParseFailed => {
            "Failed to parse file .devcontainer/devcontainer.json".to_string()
        }
        DevContainerError::NodeRuntimeNotAvailable => {
            "Cannot find a valid node runtime".to_string()
        }
        DevContainerError::NotInValidProject => "Not within a valid project".to_string(),
    }
}

#[cfg(not(target_os = "windows"))]
fn dev_container_cli() -> String {
    "devcontainer".to_string()
}

#[cfg(target_os = "windows")]
fn dev_container_cli() -> String {
    "devcontainer.cmd".to_string()
}

fn dev_container_script() -> String {
    "devcontainer.js".to_string()
}

fn docker_cli_name(use_podman: bool) -> &'static str {
    if use_podman { "podman" } else { "docker" }
}

fn profile_snippet() -> &'static str {
    "if [ -f ~/.bash_profile ]; then . ~/.bash_profile >/dev/null 2>&1; fi; \
if [ -f ~/.profile ]; then . ~/.profile >/dev/null 2>&1; fi; \
if [ -f ~/.bashrc ]; then . ~/.bashrc >/dev/null 2>&1; fi; \
if [ -f ~/.zprofile ]; then . ~/.zprofile >/dev/null 2>&1; fi;"
}

fn wrap_in_login_shell(exec: &str) -> Result<String, DevContainerError> {
    let shell_kind = ShellKind::Posix;
    let script = format!("{} {exec}", profile_snippet());
    let wrapped_for_bash = shell_kind.try_quote(&script).ok_or_else(|| {
        DevContainerError::DevContainerUpFailed(
            "Shell quoting failed for remote command".to_string(),
        )
    })?;
    let wrapped_for_sh = shell_kind.try_quote(&script).ok_or_else(|| {
        DevContainerError::DevContainerUpFailed(
            "Shell quoting failed for remote command".to_string(),
        )
    })?;
    Ok(format!(
        "if command -v bash >/dev/null 2>&1; then exec bash -lc {wrapped_for_bash}; else exec sh -lc {wrapped_for_sh}; fi"
    ))
}

fn wrap_in_sh_command(exec: &str) -> Result<String, DevContainerError> {
    let shell_kind = ShellKind::Posix;
    let script = format!("{} {exec}", profile_snippet());
    let wrapped_exec = shell_kind.try_quote(&script).ok_or_else(|| {
        DevContainerError::DevContainerUpFailed(
            "Shell quoting failed for remote command".to_string(),
        )
    })?;
    Ok(format!("sh -lc {wrapped_exec}"))
}

fn build_remote_command(
    options: &RemoteConnectionOptions,
    program: &str,
    args: &[String],
    interactive: bool,
) -> Result<Command, DevContainerError> {
    match options {
        RemoteConnectionOptions::Wsl(options) => {
            #[cfg(target_os = "windows")]
            {
                let shell_kind = ShellKind::Posix;
                let mut exec = String::new();
                use std::fmt::Write as _;

                let program = shell_kind.try_quote_prefix_aware(program).ok_or_else(|| {
                    DevContainerError::DevContainerUpFailed(
                        "Shell quoting failed for remote command".to_string(),
                    )
                })?;
                write!(exec, "exec {program}").map_err(|err| {
                    DevContainerError::DevContainerUpFailed(format!(
                        "Failed to build remote command: {err}"
                    ))
                })?;

                for arg in args {
                    let quoted = shell_kind.try_quote(arg).ok_or_else(|| {
                        DevContainerError::DevContainerUpFailed(
                            "Shell quoting failed for remote argument".to_string(),
                        )
                    })?;
                    write!(exec, " {quoted}").map_err(|err| {
                        DevContainerError::DevContainerUpFailed(format!(
                            "Failed to build remote command: {err}"
                        ))
                    })?;
                }

                let exec = wrap_in_login_shell(&exec)?;
                let mut command = util::command::new_command("wsl.exe");
                command.arg("--distribution");
                command.arg(&options.distro_name);
                if let Some(user) = &options.user {
                    command.arg("--user");
                    command.arg(user);
                }
                command.arg("--");
                command.arg("sh");
                command.arg("-lc");
                command.arg(exec);
                Ok(command)
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = options;
                Err(DevContainerError::DevContainerUpFailed(
                    "WSL host is only available on Windows".to_string(),
                ))
            }
        }
        RemoteConnectionOptions::Ssh(options) => {
            let shell_kind = ShellKind::Posix;
            let mut exec = String::new();
            use std::fmt::Write as _;

            let program = shell_kind.try_quote_prefix_aware(program).ok_or_else(|| {
                DevContainerError::DevContainerUpFailed(
                    "Shell quoting failed for remote command".to_string(),
                )
            })?;
            write!(exec, "exec {program}").map_err(|err| {
                DevContainerError::DevContainerUpFailed(format!(
                    "Failed to build remote command: {err}"
                ))
            })?;

            for arg in args {
                let quoted = shell_kind.try_quote(arg).ok_or_else(|| {
                    DevContainerError::DevContainerUpFailed(
                        "Shell quoting failed for remote argument".to_string(),
                    )
                })?;
                write!(exec, " {quoted}").map_err(|err| {
                    DevContainerError::DevContainerUpFailed(format!(
                        "Failed to build remote command: {err}"
                    ))
                })?;
            }

            let exec = wrap_in_sh_command(&exec)?;
            let mut ssh_args = options.additional_args();
            ssh_args.push("-q".to_string());
            ssh_args.push(if interactive { "-t" } else { "-T" }.to_string());
            ssh_args.push(options.ssh_destination());
            ssh_args.push(exec);

            let mut command = util::command::new_command("ssh");
            command.args(ssh_args);
            Ok(command)
        }
        RemoteConnectionOptions::Docker(_) => Err(DevContainerError::DevContainerUpFailed(
            "Unsupported remote connection for devcontainer command".to_string(),
        )),
        RemoteConnectionOptions::Mock(_) => Err(DevContainerError::DevContainerUpFailed(
            "Unsupported remote connection for devcontainer command".to_string(),
        )),
    }
}

async fn check_for_docker(context: &DevContainerContext) -> Result<(), DevContainerError> {
    if let Some(remote_options) = context.remote_connection.as_ref() {
        return check_for_docker_remote(remote_options, context.use_podman).await;
    }

    let mut command = if context.use_podman {
        util::command::new_command("podman")
    } else {
        util::command::new_command("docker")
    };
    command.arg("--version");

    match command.output().await {
        Ok(_) => Ok(()),
        Err(e) => {
            log::error!("Unable to find docker in $PATH: {:?}", e);
            Err(DevContainerError::DockerNotAvailable)
        }
    }
}

async fn check_for_docker_remote(
    options: &RemoteConnectionOptions,
    use_podman: bool,
) -> Result<(), DevContainerError> {
    let docker_cli = docker_cli_name(use_podman);
    let mut command = build_remote_command(options, docker_cli, &["--version".to_string()], false)?;
    match command.output().await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            log::error!(
                "Unable to find {} on remote host. out: {:?}, err: {:?}",
                docker_cli,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Err(DevContainerError::DockerNotAvailable)
        }
        Err(err) => {
            log::error!("Unable to execute {} on remote host: {:?}", docker_cli, err);
            Err(DevContainerError::DockerNotAvailable)
        }
    }
}

pub(crate) async fn ensure_devcontainer_cli(
    context: &DevContainerContext,
) -> Result<Option<DevContainerCli>, DevContainerError> {
    if let Some(remote_options) = context.remote_connection.as_ref() {
        ensure_devcontainer_cli_remote(remote_options).await?;
        return Ok(None);
    }

    ensure_devcontainer_cli_local(&context.node_runtime)
        .await
        .map(Some)
}

async fn ensure_devcontainer_cli_remote(
    options: &RemoteConnectionOptions,
) -> Result<(), DevContainerError> {
    let mut command =
        build_remote_command(options, "devcontainer", &["--version".to_string()], false)?;
    match command.output().await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!(
                "devcontainer CLI not found on remote host. out: {:?}, err: {:?}",
                stdout,
                stderr
            );
            Err(DevContainerError::DevContainerCliNotAvailable)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            log::error!("devcontainer command not found on remote host: {:?}", err);
            Err(DevContainerError::DevContainerCliNotAvailable)
        }
        Err(err) => {
            log::error!("Unable to execute remote devcontainer command: {:?}", err);
            Err(DevContainerError::DevContainerCliNotAvailable)
        }
    }
}

async fn ensure_devcontainer_cli_local(
    node_runtime: &NodeRuntime,
) -> Result<DevContainerCli, DevContainerError> {
    let mut command = util::command::new_command(&dev_container_cli());
    command.arg("--version");

    if let Err(e) = command.output().await {
        log::error!(
            "Unable to find devcontainer CLI in $PATH. Checking for a zed installed version. Error: {:?}",
            e
        );

        let Ok(node_runtime_path) = node_runtime.binary_path().await else {
            return Err(DevContainerError::NodeRuntimeNotAvailable);
        };

        let datadir_cli_path = paths::devcontainer_dir()
            .join("node_modules")
            .join("@devcontainers")
            .join("cli")
            .join(&dev_container_script());

        log::debug!(
            "devcontainer not found in path, using local location: ${}",
            datadir_cli_path.display()
        );

        let mut command =
            util::command::new_command(node_runtime_path.as_os_str().display().to_string());
        command.arg(datadir_cli_path.display().to_string());
        command.arg("--version");

        match command.output().await {
            Err(e) => log::error!(
                "Unable to find devcontainer CLI in Data dir. Will try to install. Error: {:?}",
                e
            ),
            Ok(output) => {
                if output.status.success() {
                    log::info!("Found devcontainer CLI in Data dir");
                    return Ok(DevContainerCli {
                        path: datadir_cli_path.clone(),
                        node_runtime_path: Some(node_runtime_path.clone()),
                    });
                } else {
                    log::error!(
                        "Could not run devcontainer CLI from data_dir. Will try once more to install. Output: {:?}",
                        output
                    );
                }
            }
        }

        if let Err(e) = fs::create_dir_all(paths::devcontainer_dir()).await {
            log::error!("Unable to create devcontainer directory. Error: {:?}", e);
            return Err(DevContainerError::DevContainerCliNotAvailable);
        }

        if let Err(e) = node_runtime
            .npm_install_packages(
                &paths::devcontainer_dir(),
                &[("@devcontainers/cli", "latest")],
            )
            .await
        {
            log::error!(
                "Unable to install devcontainer CLI to data directory. Error: {:?}",
                e
            );
            return Err(DevContainerError::DevContainerCliNotAvailable);
        };

        let mut command =
            util::command::new_command(node_runtime_path.as_os_str().display().to_string());
        command.arg(datadir_cli_path.display().to_string());
        command.arg("--version");
        if let Err(e) = command.output().await {
            log::error!(
                "Unable to find devcontainer cli after NPM install. Error: {:?}",
                e
            );
            Err(DevContainerError::DevContainerCliNotAvailable)
        } else {
            Ok(DevContainerCli {
                path: datadir_cli_path,
                node_runtime_path: Some(node_runtime_path),
            })
        }
    } else {
        log::info!("Found devcontainer cli on $PATH, using it");
        Ok(DevContainerCli {
            path: PathBuf::from(&dev_container_cli()),
            node_runtime_path: None,
        })
    }
}

async fn devcontainer_up(
    context: &DevContainerContext,
    cli: Option<&DevContainerCli>,
    project_directory: &Path,
    config_path: Option<&Path>,
) -> Result<DevContainerUp, DevContainerError> {
    if let Some(remote_options) = context.remote_connection.as_ref() {
        return devcontainer_up_remote(
            remote_options,
            project_directory,
            config_path,
            context.use_podman,
        )
        .await;
    }

    let Some(cli) = cli else {
        return Err(DevContainerError::DevContainerCliNotAvailable);
    };

    devcontainer_up_local(context, cli, project_directory, config_path).await
}

async fn devcontainer_up_local(
    context: &DevContainerContext,
    cli: &DevContainerCli,
    project_directory: &Path,
    config_path: Option<&Path>,
) -> Result<DevContainerUp, DevContainerError> {
    let mut command = cli.command(context.use_podman);
    command.arg("up");
    command.arg("--workspace-folder");
    command.arg(project_directory.display().to_string());

    if let Some(config) = config_path {
        command.arg("--config");
        command.arg(config.display().to_string());
    }

    log::info!("Running full devcontainer up command: {:?}", command);

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running devcontainer up for workspace: out: {}, err: {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );

                log::error!("{}", &message);
                Err(DevContainerError::DevContainerUpFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running devcontainer up: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
    }
}

async fn devcontainer_up_remote(
    remote_options: &RemoteConnectionOptions,
    project_directory: &Path,
    config_path: Option<&Path>,
    use_podman: bool,
) -> Result<DevContainerUp, DevContainerError> {
    let mut args = vec![
        "up".to_string(),
        "--workspace-folder".to_string(),
        normalize_remote_path_arg(project_directory),
    ];
    if let Some(config) = config_path {
        args.push("--config".to_string());
        args.push(normalize_remote_path_arg(config));
    }
    if use_podman {
        args.push("--docker-path".to_string());
        args.push("podman".to_string());
    }

    let mut command = build_remote_command(remote_options, "devcontainer", &args, false)?;
    log::info!("Running remote devcontainer up command: {:?}", command);

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running remote devcontainer up: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                log::error!("{}", &message);
                Err(DevContainerError::DevContainerUpFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running remote devcontainer up: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
    }
}

pub(crate) async fn read_devcontainer_configuration(
    context: &DevContainerContext,
    cli: Option<&DevContainerCli>,
    config_path: Option<&Path>,
) -> Result<DevContainerConfigurationOutput, DevContainerError> {
    let project_directory = resolve_project_directory_on_host(context).await?;

    if let Some(remote_options) = context.remote_connection.as_ref() {
        return read_devcontainer_configuration_remote(
            remote_options,
            project_directory.as_path(),
            config_path,
            context.use_podman,
        )
        .await;
    }

    let Some(cli) = cli else {
        return Err(DevContainerError::DevContainerCliNotAvailable);
    };

    read_devcontainer_configuration_local(context, cli, project_directory.as_path(), config_path)
        .await
}

async fn read_devcontainer_configuration_local(
    context: &DevContainerContext,
    cli: &DevContainerCli,
    project_directory: &Path,
    config_path: Option<&Path>,
) -> Result<DevContainerConfigurationOutput, DevContainerError> {
    let mut command = cli.command(context.use_podman);
    command.arg("read-configuration");
    command.arg("--workspace-folder");
    command.arg(project_directory.display().to_string());

    if let Some(config) = config_path {
        command.arg("--config");
        command.arg(config.display().to_string());
    }

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running devcontainer read-configuration for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                log::error!("{}", &message);
                Err(DevContainerError::DevContainerNotFound)
            }
        }
        Err(e) => {
            let message = format!("Error running devcontainer read-configuration: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerNotFound)
        }
    }
}

async fn read_devcontainer_configuration_remote(
    remote_options: &RemoteConnectionOptions,
    project_directory: &Path,
    config_path: Option<&Path>,
    use_podman: bool,
) -> Result<DevContainerConfigurationOutput, DevContainerError> {
    let mut args = vec![
        "read-configuration".to_string(),
        "--workspace-folder".to_string(),
        normalize_remote_path_arg(project_directory),
    ];
    if let Some(config) = config_path {
        args.push("--config".to_string());
        args.push(normalize_remote_path_arg(config));
    }
    if use_podman {
        args.push("--docker-path".to_string());
        args.push("podman".to_string());
    }

    let mut command = build_remote_command(remote_options, "devcontainer", &args, false)?;
    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running remote devcontainer read-configuration: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                log::error!("{}", &message);
                Err(DevContainerError::DevContainerNotFound)
            }
        }
        Err(e) => {
            let message = format!(
                "Error running remote devcontainer read-configuration: {:?}",
                e
            );
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerNotFound)
        }
    }
}

pub(crate) async fn apply_dev_container_template(
    template: &DevContainerTemplate,
    template_options: &HashMap<String, String>,
    features_selected: &HashSet<DevContainerFeature>,
    context: &DevContainerContext,
    cli: Option<&DevContainerCli>,
) -> Result<DevContainerApply, DevContainerError> {
    let project_directory = resolve_project_directory_on_host(context).await?;

    if let Some(remote_options) = context.remote_connection.as_ref() {
        return apply_dev_container_template_remote(
            template,
            template_options,
            features_selected,
            remote_options,
            project_directory.as_path(),
        )
        .await;
    }

    let Some(cli) = cli else {
        return Err(DevContainerError::DevContainerCliNotAvailable);
    };

    apply_dev_container_template_local(
        template,
        template_options,
        features_selected,
        context,
        cli,
        project_directory.as_path(),
    )
    .await
}

async fn apply_dev_container_template_local(
    template: &DevContainerTemplate,
    template_options: &HashMap<String, String>,
    features_selected: &HashSet<DevContainerFeature>,
    context: &DevContainerContext,
    cli: &DevContainerCli,
    project_directory: &Path,
) -> Result<DevContainerApply, DevContainerError> {
    let mut command = cli.command(context.use_podman);

    let Ok(serialized_options) = serde_json::to_string(template_options) else {
        log::error!("Unable to serialize options for {:?}", template_options);
        return Err(DevContainerError::DevContainerParseFailed);
    };

    command.arg("templates");
    command.arg("apply");
    command.arg("--workspace-folder");
    command.arg(project_directory.display().to_string());
    command.arg("--template-id");
    command.arg(format!(
        "{}/{}",
        template
            .source_repository
            .as_ref()
            .unwrap_or(&String::from("")),
        template.id
    ));
    command.arg("--template-args");
    command.arg(serialized_options);
    command.arg("--features");
    command.arg(template_features_to_json(features_selected));

    log::debug!("Running full devcontainer apply command: {:?}", command);

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running devcontainer templates apply for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );

                log::error!("{}", &message);
                Err(DevContainerError::DevContainerTemplateApplyFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running devcontainer templates apply: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerTemplateApplyFailed(message))
        }
    }
}

async fn apply_dev_container_template_remote(
    template: &DevContainerTemplate,
    template_options: &HashMap<String, String>,
    features_selected: &HashSet<DevContainerFeature>,
    remote_options: &RemoteConnectionOptions,
    project_directory: &Path,
) -> Result<DevContainerApply, DevContainerError> {
    let Ok(serialized_options) = serde_json::to_string(template_options) else {
        log::error!("Unable to serialize options for {:?}", template_options);
        return Err(DevContainerError::DevContainerParseFailed);
    };

    let args = vec![
        "templates".to_string(),
        "apply".to_string(),
        "--workspace-folder".to_string(),
        normalize_remote_path_arg(project_directory),
        "--template-id".to_string(),
        format!(
            "{}/{}",
            template
                .source_repository
                .as_ref()
                .unwrap_or(&String::from("")),
            template.id
        ),
        "--template-args".to_string(),
        serialized_options,
        "--features".to_string(),
        template_features_to_json(features_selected),
    ];

    let mut command = build_remote_command(remote_options, "devcontainer", &args, false)?;
    log::debug!("Running remote devcontainer apply command: {:?}", command);

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running remote devcontainer templates apply: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );

                log::error!("{}", &message);
                Err(DevContainerError::DevContainerTemplateApplyFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running remote devcontainer templates apply: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerTemplateApplyFailed(message))
        }
    }
}
// Try to parse directly first (newer versions output pure JSON)
// If that fails, look for JSON start (older versions have plaintext prefix)
fn parse_json_from_cli<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, DevContainerError> {
    serde_json::from_str::<T>(&raw)
        .or_else(|e| {
            log::error!("Error parsing json: {} - will try to find json object in larger plaintext", e);
            let json_start = raw
                .find(|c| c == '{')
                .ok_or_else(|| {
                    log::error!("No JSON found in devcontainer up output");
                    DevContainerError::DevContainerParseFailed
                })?;

            serde_json::from_str(&raw[json_start..]).map_err(|e| {
                log::error!(
                    "Unable to parse JSON from devcontainer up output (starting at position {}), error: {:?}",
                    json_start,
                    e
                );
                DevContainerError::DevContainerParseFailed
            })
        })
}

fn parse_json_array_from_cli<T: serde::de::DeserializeOwned>(
    raw: &str,
) -> Result<T, DevContainerError> {
    serde_json::from_str::<T>(raw).or_else(|e| {
        log::error!("Error parsing json: {} - will try to find json array in larger plaintext", e);
        let json_start = raw.find('[').ok_or_else(|| {
            log::error!("No JSON array found in docker inspect output");
            DevContainerError::DevContainerParseFailed
        })?;

        serde_json::from_str(&raw[json_start..]).map_err(|e| {
            log::error!(
                "Unable to parse JSON array from docker inspect output (starting at position {}), error: {:?}",
                json_start,
                e
            );
            DevContainerError::DevContainerParseFailed
        })
    })
}

fn get_backup_project_name(remote_workspace_folder: &str, container_id: &str) -> String {
    Path::new(remote_workspace_folder)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|string| string.to_string())
        .unwrap_or_else(|| container_id.to_string())
}

fn template_features_to_json(features_selected: &HashSet<DevContainerFeature>) -> String {
    let features_map = features_selected
        .iter()
        .map(|feature| {
            let mut map = HashMap::new();
            map.insert(
                "id",
                format!(
                    "{}/{}:{}",
                    feature
                        .source_repository
                        .as_ref()
                        .unwrap_or(&String::from("")),
                    feature.id,
                    feature.major_version()
                ),
            );
            map
        })
        .collect::<Vec<HashMap<&str, String>>>();
    serde_json::to_string(&features_map).unwrap()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::devcontainer_api::{
        DevContainerConfig, DevContainerUp, find_configs_in_snapshot, parse_json_from_cli,
    };
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[test]
    fn should_parse_from_devcontainer_json() {
        let json = r#"{"outcome":"success","containerId":"826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/zed"}"#;
        let up: DevContainerUp = parse_json_from_cli(json).unwrap();
        assert_eq!(up._outcome, "success");
        assert_eq!(
            up.container_id,
            "826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a"
        );
        assert_eq!(up.remote_user, "vscode");
        assert_eq!(up.remote_workspace_folder, "/workspaces/zed");

        let json_in_plaintext = r#"[2026-01-22T16:19:08.802Z] @devcontainers/cli 0.80.1. Node.js v22.21.1. darwin 24.6.0 arm64.
            {"outcome":"success","containerId":"826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/zed"}"#;
        let up: DevContainerUp = parse_json_from_cli(json_in_plaintext).unwrap();
        assert_eq!(up._outcome, "success");
        assert_eq!(
            up.container_id,
            "826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a"
        );
        assert_eq!(up.remote_user, "vscode");
        assert_eq!(up.remote_workspace_folder, "/workspaces/zed");
    }

    #[gpui::test]
    async fn test_find_configs_root_devcontainer_json(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".devcontainer.json": "{}"
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "root");
        assert_eq!(configs[0].config_path, PathBuf::from(".devcontainer.json"));
    }

    #[gpui::test]
    async fn test_find_configs_default_devcontainer_dir(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".devcontainer": {
                    "devcontainer.json": "{}"
                }
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0], DevContainerConfig::default_config());
    }

    #[gpui::test]
    async fn test_find_configs_dir_and_root_both_included(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".devcontainer.json": "{}",
                ".devcontainer": {
                    "devcontainer.json": "{}"
                }
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0], DevContainerConfig::default_config());
        assert_eq!(configs[1], DevContainerConfig::root_config());
    }

    #[gpui::test]
    async fn test_find_configs_subfolder_configs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".devcontainer": {
                    "rust": {
                        "devcontainer.json": "{}"
                    },
                    "python": {
                        "devcontainer.json": "{}"
                    }
                }
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert_eq!(configs.len(), 2);
        let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"python"));
        assert!(names.contains(&"rust"));
    }

    #[gpui::test]
    async fn test_find_configs_default_and_subfolder(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".devcontainer": {
                    "devcontainer.json": "{}",
                    "gpu": {
                        "devcontainer.json": "{}"
                    }
                }
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "default");
        assert_eq!(configs[1].name, "gpu");
    }

    #[gpui::test]
    async fn test_find_configs_no_devcontainer(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "src": {
                    "main.rs": "fn main() {}"
                }
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert!(configs.is_empty());
    }

    #[gpui::test]
    async fn test_find_configs_root_json_and_subfolder_configs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".devcontainer.json": "{}",
                ".devcontainer": {
                    "rust": {
                        "devcontainer.json": "{}"
                    }
                }
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "root");
        assert_eq!(configs[0].config_path, PathBuf::from(".devcontainer.json"));
        assert_eq!(configs[1].name, "rust");
        assert_eq!(
            configs[1].config_path,
            PathBuf::from(".devcontainer/rust/devcontainer.json")
        );
    }

    #[gpui::test]
    async fn test_find_configs_empty_devcontainer_dir_falls_back_to_root(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".devcontainer.json": "{}",
                ".devcontainer": {}
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let configs = project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("should have a worktree");
            find_configs_in_snapshot(worktree.read(cx))
        });

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0], DevContainerConfig::root_config());
    }
}
