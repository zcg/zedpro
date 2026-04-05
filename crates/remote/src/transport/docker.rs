use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use collections::HashMap;
use parking_lot::Mutex;
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use semver::Version as SemanticVersion;
use std::collections::BTreeMap;
use std::time::Instant;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt;
use util::command::Stdio;
use util::shell::ShellKind;
use util::{
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
};

use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext, AsyncApp, Task};
use rpc::proto::Envelope;

use crate::{
    RemoteClientDelegate, RemoteConnection, RemoteConnectionOptions, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, Interactive},
    transport::parse_platform,
};

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct DockerConnectionOptions {
    pub name: String,
    pub container_id: String,
    pub remote_user: String,
    pub upload_binary_over_docker_exec: bool,
    pub use_podman: bool,
    pub host: DockerHost,
    pub remote_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DockerHost {
    Local,
    Ssh(crate::SshConnectionOptions),
    Wsl(crate::WslConnectionOptions),
}

impl Default for DockerHost {
    fn default() -> Self {
        Self::Local
    }
}

fn wsl_docker_command_prefix(
    docker_cli: &str,
    options: &crate::WslConnectionOptions,
) -> (String, Vec<String>) {
    let mut args = Vec::new();
    if let Some(user) = &options.user {
        args.push("--user".to_string());
        args.push(user.clone());
    }
    args.extend([
        "--distribution".to_string(),
        options.distro_name.clone(),
        "--cd".to_string(),
        "~".to_string(),
        "--exec".to_string(),
        docker_cli.to_string(),
    ]);
    ("wsl.exe".to_string(), args)
}

fn new_host_docker_command(docker_cli: &str, host: &DockerHost) -> util::command::Command {
    match host {
        DockerHost::Local => util::command::new_command(docker_cli),
        DockerHost::Ssh(options) => {
            let (program, args) = ssh_docker_command_prefix(docker_cli, options);
            let mut command = util::command::new_command(program);
            command.current_dir(std::env::temp_dir());
            command.args(args);
            command
        }
        DockerHost::Wsl(options) => {
            if should_bridge_wsl_commands() {
                let (program, args) = wsl_docker_command_prefix(docker_cli, options);
                let mut command = util::command::new_command(program);
                command.current_dir(std::env::temp_dir());
                command.args(args);
                command
            } else {
                util::command::new_command(docker_cli)
            }
        }
    }
}

fn ssh_docker_command_prefix(
    docker_cli: &str,
    options: &crate::SshConnectionOptions,
) -> (String, Vec<String>) {
    let mut args = options.additional_args();
    args.push(options.ssh_destination());
    args.push(docker_cli.to_string());
    ("ssh".to_string(), args)
}

fn format_local_path_for_docker_host(path: &str, host: &DockerHost) -> String {
    match host {
        DockerHost::Local | DockerHost::Ssh(_) => path.to_string(),
        DockerHost::Wsl(options) => format_local_path_for_wsl_host(path, options),
    }
}

fn format_local_path_for_wsl_host(path: &str, options: &crate::WslConnectionOptions) -> String {
    let rewritten = path
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"));
    let raw = rewritten.as_deref().unwrap_or(path);
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(raw);

    if raw.starts_with('/') {
        return raw.replace('\\', "/");
    }

    let raw = raw.replace('/', r"\");

    if let Some(path) = wsl_unc_path_to_posix(&raw, &options.distro_name) {
        return path;
    }

    windows_path_to_wsl_mount(&raw).unwrap_or_else(|| raw.replace('\\', "/"))
}

fn wsl_unc_path_to_posix(path: &str, distro_name: &str) -> Option<String> {
    let unc = path.strip_prefix(r"\\")?;
    let mut segments = unc.split('\\');
    let host = segments.next()?;
    let share = segments.next()?;

    if !(host.eq_ignore_ascii_case("wsl$") || host.eq_ignore_ascii_case("wsl.localhost")) {
        return None;
    }
    if !share.eq_ignore_ascii_case(distro_name) {
        return None;
    }

    let remainder = segments
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");

    if remainder.is_empty() {
        Some("/".to_string())
    } else {
        Some(format!("/{remainder}"))
    }
}

fn windows_path_to_wsl_mount(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }

    let drive = (bytes[0] as char).to_ascii_lowercase();
    let mut converted = format!("/mnt/{drive}");
    converted.push('/');
    converted.push_str(&path[3..].replace('\\', "/"));
    Some(converted)
}

fn host_docker_command_template(
    docker_cli: &str,
    host: &DockerHost,
    docker_args: Vec<String>,
) -> CommandTemplate {
    match host {
        DockerHost::Local => CommandTemplate {
            program: docker_cli.to_string(),
            args: docker_args,
            env: Default::default(),
        },
        DockerHost::Ssh(options) => {
            let (program, mut args) = ssh_docker_command_prefix(docker_cli, options);
            args.extend(docker_args);
            CommandTemplate {
                program,
                args,
                env: Default::default(),
            }
        }
        DockerHost::Wsl(options) => {
            if should_bridge_wsl_commands() {
                let (program, mut args) = wsl_docker_command_prefix(docker_cli, options);
                args.extend(docker_args);
                CommandTemplate {
                    program,
                    args,
                    env: Default::default(),
                }
            } else {
                CommandTemplate {
                    program: docker_cli.to_string(),
                    args: docker_args,
                    env: Default::default(),
                }
            }
        }
    }
}

#[inline]
fn should_bridge_wsl_commands() -> bool {
    cfg!(target_os = "windows")
}

fn should_fallback_to_container_default_user(err: &anyhow::Error) -> bool {
    should_fallback_to_container_default_user_message(&err.to_string())
}

fn should_fallback_to_container_default_user_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("unable to find user")
        || message.contains("no matching entries in passwd file")
        || message.contains("invalid user")
}

pub(crate) struct DockerExecConnection {
    proxy_process: Mutex<Option<u32>>,
    remote_dir_for_server: String,
    remote_binary_relpath: Option<Arc<RelPath>>,
    connection_options: DockerConnectionOptions,
    exec_user: Option<String>,
    remote_platform: Option<RemotePlatform>,
    path_style: Option<PathStyle>,
    shell: String,
}

impl DockerExecConnection {
    pub async fn new(
        connection_options: DockerConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        let mut this = Self {
            proxy_process: Mutex::new(None),
            remote_dir_for_server: "/".to_string(),
            remote_binary_relpath: None,
            connection_options,
            exec_user: None,
            remote_platform: None,
            path_style: None,
            shell: "sh".to_owned(),
        };
        let (release_channel, version, commit) = cx.update(|cx| {
            (
                ReleaseChannel::global(cx),
                AppVersion::global(cx),
                AppCommitSha::try_global(cx),
            )
        });
        this.ensure_container_running().await?;
        this.exec_user = this.resolve_exec_user().await?;
        let remote_platform = this.check_remote_platform().await?;

        this.path_style = match remote_platform.os {
            RemoteOs::Windows => Some(PathStyle::Windows),
            _ => Some(PathStyle::Posix),
        };

        this.remote_platform = Some(remote_platform);
        log::info!("Remote platform discovered: {:?}", this.remote_platform);

        this.shell = this.discover_shell().await;
        log::info!("Remote shell discovered: {}", this.shell);

        this.remote_dir_for_server = this.docker_user_home_dir().await?.trim().to_string();

        this.remote_binary_relpath = Some(
            this.ensure_server_binary(
                &delegate,
                release_channel,
                version,
                &this.remote_dir_for_server,
                commit,
                cx,
            )
            .await?,
        );

        Ok(this)
    }

    fn docker_cli(&self) -> &str {
        if self.connection_options.use_podman {
            "podman"
        } else {
            "docker"
        }
    }

    async fn resolve_exec_user(&self) -> Result<Option<String>> {
        let requested_user = self.connection_options.remote_user.trim();
        if requested_user.is_empty() {
            return Ok(None);
        }

        match self
            .run_docker_command(
                "exec",
                &[
                    "-u".to_string(),
                    requested_user.to_string(),
                    self.connection_options.container_id.clone(),
                    "id".to_string(),
                    "-un".to_string(),
                ],
            )
            .await
        {
            Ok(_) => Ok(Some(requested_user.to_string())),
            Err(err) if should_fallback_to_container_default_user(&err) => {
                log::warn!(
                    "Docker exec user '{}' is unavailable in container {}; falling back to the container default user",
                    requested_user,
                    self.connection_options.container_id
                );
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    async fn ensure_container_running(&self) -> Result<()> {
        let output = self
            .run_docker_command(
                "inspect",
                &[
                    "--format".to_string(),
                    "{{.State.Running}}".to_string(),
                    self.connection_options.container_id.clone(),
                ],
            )
            .await?;

        if output.trim().eq_ignore_ascii_case("true") {
            return Ok(());
        }

        log::info!(
            "Docker container {} is stopped; starting before connecting",
            self.connection_options.container_id
        );
        self.run_docker_command("start", &[self.connection_options.container_id.clone()])
            .await?;
        Ok(())
    }

    fn append_exec_user_arg(&self, args: &mut Vec<String>) {
        if let Some(exec_user) = self.exec_user.as_ref() {
            args.push("-u".to_string());
            args.push(exec_user.clone());
        }
    }

    async fn discover_shell(&self) -> String {
        let default_shell = "sh";
        match self
            .run_docker_exec("sh", None, &Default::default(), &["-c", "echo $SHELL"])
            .await
        {
            Ok(shell) => match shell.trim() {
                "" => {
                    log::info!("$SHELL is not set, checking passwd for user");
                }
                shell => {
                    return shell.to_owned();
                }
            },
            Err(e) => {
                log::error!("Failed to get $SHELL: {e}. Checking passwd for user");
            }
        }

        match self
            .run_docker_exec(
                "sh",
                None,
                &Default::default(),
                &["-c", "getent passwd \"$(id -un)\" | cut -d: -f7"],
            )
            .await
        {
            Ok(shell) => match shell.trim() {
                "" => {
                    log::info!("No shell found in passwd, falling back to {default_shell}");
                }
                shell => {
                    return shell.to_owned();
                }
            },
            Err(e) => {
                log::info!("Error getting shell from passwd: {e}. Falling back to {default_shell}");
            }
        }
        default_shell.to_owned()
    }

    async fn check_remote_platform(&self) -> Result<RemotePlatform> {
        let uname = self
            .run_docker_exec("uname", None, &Default::default(), &["-sm"])
            .await?;
        parse_platform(&uname)
    }

    async fn ensure_server_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: SemanticVersion,
        remote_dir_for_server: &str,
        commit: Option<AppCommitSha>,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        let remote_platform = self
            .remote_platform
            .context("No remote platform defined; cannot proceed.")?;

        let version_str = match release_channel {
            ReleaseChannel::Nightly => {
                let commit = commit.map(|s| s.full()).unwrap_or_default();
                format!("{}-{}", version, commit)
            }
            ReleaseChannel::Dev => "build".to_string(),
            _ => version.to_string(),
        };
        let binary_name = format!(
            "zed-remote-server-{}-{}",
            release_channel.dev_name(),
            version_str
        );
        let dst_path =
            paths::remote_server_dir_relative().join(RelPath::unix(&binary_name).unwrap());

        let binary_exists_on_server = self
            .run_docker_exec(
                &dst_path.display(self.path_style()),
                Some(&remote_dir_for_server),
                &Default::default(),
                &["version"],
            )
            .await
            .is_ok();
        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        if let Some(remote_server_path) = super::build_remote_server_from_source(
            &remote_platform,
            delegate.as_ref(),
            binary_exists_on_server,
            cx,
        )
        .await?
        {
            let tmp_path = paths::remote_server_dir_relative().join(
                RelPath::unix(&format!(
                    "download-{}-{}",
                    std::process::id(),
                    remote_server_path.file_name().unwrap().to_string_lossy()
                ))
                .unwrap(),
            );
            self.upload_local_server_binary(
                &remote_server_path,
                &tmp_path,
                &remote_dir_for_server,
                delegate,
                cx,
            )
            .await?;
            self.extract_server_binary(&dst_path, &tmp_path, &remote_dir_for_server, delegate, cx)
                .await?;
            return Ok(dst_path);
        }

        if binary_exists_on_server {
            return Ok(dst_path);
        }

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly => Ok(None),
            ReleaseChannel::Dev => {
                anyhow::bail!(
                    "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at ({:?})",
                    dst_path
                )
            }
            _ => Ok(Some(AppVersion::global(cx))),
        })?;

        let tmp_path_gz = paths::remote_server_dir_relative().join(
            RelPath::unix(&format!(
                "{}-download-{}.gz",
                binary_name,
                std::process::id()
            ))
            .unwrap(),
        );
        if !self.connection_options.upload_binary_over_docker_exec
            && let Some(url) = delegate
                .get_download_url(remote_platform, release_channel, wanted_version.clone(), cx)
                .await?
        {
            match self
                .download_binary_on_server(&url, &tmp_path_gz, &remote_dir_for_server, delegate, cx)
                .await
            {
                Ok(_) => {
                    self.extract_server_binary(
                        &dst_path,
                        &tmp_path_gz,
                        &remote_dir_for_server,
                        delegate,
                        cx,
                    )
                    .await
                    .context("extracting server binary")?;
                    return Ok(dst_path);
                }
                Err(e) => {
                    log::error!(
                        "Failed to download binary on server, attempting to download locally and then upload it the server: {e:#}",
                    )
                }
            }
        }

        let src_path = delegate
            .download_server_binary_locally(remote_platform, release_channel, wanted_version, cx)
            .await
            .context("downloading server binary locally")?;
        self.upload_local_server_binary(
            &src_path,
            &tmp_path_gz,
            &remote_dir_for_server,
            delegate,
            cx,
        )
        .await
        .context("uploading server binary")?;
        self.extract_server_binary(
            &dst_path,
            &tmp_path_gz,
            &remote_dir_for_server,
            delegate,
            cx,
        )
        .await
        .context("extracting server binary")?;
        Ok(dst_path)
    }

    async fn docker_user_home_dir(&self) -> Result<String> {
        let inner_program = self.shell();
        self.run_docker_exec(
            &inner_program,
            None,
            &Default::default(),
            &["-c", "echo $HOME"],
        )
        .await
    }

    async fn extract_server_binary(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some("Extracting remote development server"), cx);
        let server_mode = 0o755;

        let shell_kind = ShellKind::Posix;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let server_mode = format!("{:o}", server_mode);
        let server_mode = shell_kind
            .try_quote(&server_mode)
            .context("shell quoting")?;
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;
        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".gz") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(&tmp_path).context("shell quoting")?;
            format!(
                "gunzip -f {orig_tmp_path} && chmod {server_mode} {tmp_path} && mv {tmp_path} {dst_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("chmod {server_mode} {orig_tmp_path} && mv {orig_tmp_path} {dst_path}",)
        };
        let args = shell_kind.args_for_shell(false, script.to_string());
        self.run_docker_exec(
            "sh",
            Some(&remote_dir_for_server),
            &Default::default(),
            &args,
        )
        .await
        .log_err();
        Ok(())
    }

    async fn upload_local_server_binary(
        &self,
        src_path: &Path,
        tmp_path_gz: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path_gz.parent() {
            self.run_docker_exec(
                "mkdir",
                Some(remote_dir_for_server),
                &Default::default(),
                &["-p", parent.display(self.path_style()).as_ref()],
            )
            .await?;
        }

        let src_stat = smol::fs::metadata(&src_path).await?;
        let size = src_stat.len();

        let t0 = Instant::now();
        delegate.set_status(Some("Uploading remote development server"), cx);
        log::info!(
            "uploading remote development server to {:?} ({}kb)",
            tmp_path_gz,
            size / 1024
        );
        self.upload_file(src_path, tmp_path_gz, remote_dir_for_server)
            .await
            .context("failed to upload server binary")?;
        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn upload_and_chown(
        docker_cli: String,
        connection_options: DockerConnectionOptions,
        src_path: String,
        dst_path: String,
    ) -> Result<()> {
        let src_path = format_local_path_for_docker_host(&src_path, &connection_options.host);
        let mut command = new_host_docker_command(&docker_cli, &connection_options.host);
        command.arg("cp");
        command.arg("-a");
        command.arg(&src_path);
        command.arg(format!("{}:{}", connection_options.container_id, dst_path));

        let output = command.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!("failed to upload via docker cp {src_path} -> {dst_path}: {stderr}",);
            anyhow::bail!(
                "failed to upload via docker cp {} -> {}: {}",
                src_path,
                dst_path,
                stderr,
            );
        }

        let mut chown_command = new_host_docker_command(&docker_cli, &connection_options.host);
        chown_command.arg("exec");
        chown_command.arg(connection_options.container_id.clone());
        chown_command.arg("chown");
        chown_command.arg(format!(
            "{}:{}",
            connection_options.remote_user, connection_options.remote_user,
        ));
        chown_command.arg(&dst_path);

        let output = chown_command.output().await?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if should_fallback_to_container_default_user_message(stderr.as_ref()) {
            log::warn!(
                "Skipping docker chown for {}; remote user '{}' does not exist in the container",
                connection_options.container_id,
                connection_options.remote_user
            );
            return Ok(());
        }
        log::debug!("failed to change ownership for via chown: {stderr}",);
        anyhow::bail!(
            "failed to change ownership for zed_remote_server via chown: {}",
            stderr,
        );
    }

    async fn upload_file(
        &self,
        src_path: &Path,
        dest_path: &RelPath,
        remote_dir_for_server: &str,
    ) -> Result<()> {
        log::debug!("uploading file {:?} to {:?}", src_path, dest_path);

        let src_path_display = src_path.display().to_string();
        let dest_path_str = dest_path.display(self.path_style());
        let full_server_path = format!("{}/{}", remote_dir_for_server, dest_path_str);

        Self::upload_and_chown(
            self.docker_cli().to_string(),
            self.connection_options.clone(),
            src_path_display,
            full_server_path,
        )
        .await
    }

    async fn run_docker_command(
        &self,
        subcommand: &str,
        args: &[impl AsRef<str>],
    ) -> Result<String> {
        let mut command = new_host_docker_command(self.docker_cli(), &self.connection_options.host);
        command.arg(subcommand);
        for arg in args {
            command.arg(arg.as_ref());
        }
        let output = command.output().await?;
        log::debug!("{:?}: {:?}", command, output);
        anyhow::ensure!(
            output.status.success(),
            "failed to run command {command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn run_docker_exec(
        &self,
        inner_program: &str,
        working_directory: Option<&str>,
        env: &HashMap<String, String>,
        program_args: &[impl AsRef<str>],
    ) -> Result<String> {
        let mut args = match working_directory {
            Some(dir) => vec!["-w".to_string(), dir.to_string()],
            None => vec![],
        };

        self.append_exec_user_arg(&mut args);

        for (k, v) in self.connection_options.remote_env.iter() {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }

        for (k, v) in env.iter() {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }

        args.push(self.connection_options.container_id.clone());
        args.push(inner_program.to_string());

        for arg in program_args {
            args.push(arg.as_ref().to_owned());
        }
        self.run_docker_command("exec", args.as_ref()).await
    }

    async fn download_binary_on_server(
        &self,
        url: &str,
        tmp_path_gz: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path_gz.parent() {
            self.run_docker_exec(
                "mkdir",
                Some(remote_dir_for_server),
                &Default::default(),
                &["-p", parent.display(self.path_style()).as_ref()],
            )
            .await?;
        }

        delegate.set_status(Some("Downloading remote development server on host"), cx);

        match self
            .run_docker_exec(
                "curl",
                Some(remote_dir_for_server),
                &Default::default(),
                &[
                    "-f",
                    "-L",
                    url,
                    "-o",
                    &tmp_path_gz.display(self.path_style()),
                ],
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if self
                    .run_docker_exec("which", None, &Default::default(), &["curl"])
                    .await
                    .is_ok()
                {
                    return Err(e);
                }

                log::info!("curl is not available, trying wget");
                match self
                    .run_docker_exec(
                        "wget",
                        Some(remote_dir_for_server),
                        &Default::default(),
                        &[url, "-O", &tmp_path_gz.display(self.path_style())],
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        if self
                            .run_docker_exec("which", None, &Default::default(), &["wget"])
                            .await
                            .is_ok()
                        {
                            return Err(e);
                        } else {
                            anyhow::bail!("Neither curl nor wget is available");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn kill_inner(&self) -> Result<()> {
        if let Some(pid) = self.proxy_process.lock().take() {
            let output = if cfg!(windows) {
                std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .output()
            } else {
                std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .output()
            }
            .map_err(|error| anyhow!("Failed to kill process {pid}: {error}"))?;

            if output.status.success() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = format!("{stdout}\n{stderr}");
            let combined_lower = combined.to_ascii_lowercase();
            if combined_lower.contains("no such process")
                || combined_lower.contains("not found")
                || combined_lower.contains("no running instance")
                || combined_lower.contains("cannot find the process")
            {
                log::info!(
                    "proxy process {} was already gone while disconnecting docker transport",
                    pid
                );
                return Ok(());
            }

            Err(anyhow!("Failed to kill process {pid}: {}", combined.trim()))
        } else {
            Ok(())
        }
    }
}

#[async_trait(?Send)]
impl RemoteConnection for DockerExecConnection {
    fn has_wsl_interop(&self) -> bool {
        false
    }
    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        // We'll try connecting anew every time we open a devcontainer, so proactively try to kill any old connections.
        if !self.has_been_killed() {
            if let Err(e) = self.kill_inner() {
                return Task::ready(Err(e));
            };
        }

        delegate.set_status(Some("Starting proxy"), cx);

        let Some(remote_binary_relpath) = self.remote_binary_relpath.clone() else {
            return Task::ready(Err(anyhow!("Remote binary path not set")));
        };

        let mut docker_args = vec!["exec".to_string()];

        for (k, v) in self.connection_options.remote_env.iter() {
            docker_args.push("-e".to_string());
            docker_args.push(format!("{k}={v}"));
        }
        for env_var in ["RUST_LOG", "RUST_BACKTRACE", "ZED_GENERATE_MINIDUMPS"] {
            if let Some(value) = std::env::var(env_var).ok() {
                docker_args.push("-e".to_string());
                docker_args.push(format!("{env_var}={value}"));
            }
        }

        docker_args.extend([
            "-w".to_string(),
            self.remote_dir_for_server.clone(),
            "-i".to_string(),
        ]);
        self.append_exec_user_arg(&mut docker_args);
        docker_args.push(self.connection_options.container_id.to_string());

        let val = remote_binary_relpath
            .display(self.path_style())
            .into_owned();
        docker_args.push(val);
        docker_args.push("proxy".to_string());
        docker_args.push("--identifier".to_string());
        docker_args.push(unique_identifier);
        if reconnect {
            docker_args.push("--reconnect".to_string());
        }
        let mut command = new_host_docker_command(self.docker_cli(), &self.connection_options.host);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(docker_args);

        let Ok(child) = command.spawn() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Failed to start remote server process"
            )));
        };

        let mut proxy_process = self.proxy_process.lock();
        *proxy_process = Some(child.id());

        cx.spawn(async move |cx| {
            super::handle_rpc_messages_over_child_process_stdio(
                child,
                incoming_tx,
                outgoing_rx,
                connection_activity_tx,
                cx,
            )
            .await
            .and_then(|status| {
                if status != 0 {
                    anyhow::bail!("Remote server exited with status {status}");
                }
                Ok(0)
            })
        })
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let dest_path_str = dest_path.to_string();
        let src_path_display = src_path.display().to_string();

        let upload_task = Self::upload_and_chown(
            self.docker_cli().to_string(),
            self.connection_options.clone(),
            src_path_display,
            dest_path_str,
        );

        cx.background_spawn(upload_task)
    }

    async fn kill(&self) -> Result<()> {
        self.kill_inner()
    }

    fn has_been_killed(&self) -> bool {
        self.proxy_process.lock().is_none()
    }

    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        _port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        let mut parsed_working_dir = None;

        let path_style = self.path_style();

        if let Some(working_dir) = working_dir {
            let working_dir = RemotePathBuf::new(working_dir, path_style).to_string();

            const TILDE_PREFIX: &'static str = "~/";
            if working_dir.starts_with(TILDE_PREFIX) {
                let working_dir = working_dir.trim_start_matches("~").trim_start_matches("/");
                parsed_working_dir = Some(format!("$HOME/{working_dir}"));
            } else {
                parsed_working_dir = Some(working_dir);
            }
        }

        let mut inner_program = Vec::new();

        if let Some(program) = program {
            inner_program.push(program);
            for arg in args {
                inner_program.push(arg.clone());
            }
        } else {
            inner_program.push(self.shell());
            inner_program.push("-l".to_string());
        };

        let mut docker_args = vec!["exec".to_string()];
        self.append_exec_user_arg(&mut docker_args);

        if let Some(parsed_working_dir) = parsed_working_dir {
            docker_args.push("-w".to_string());
            docker_args.push(parsed_working_dir);
        }

        for (k, v) in self.connection_options.remote_env.iter() {
            docker_args.push("-e".to_string());
            docker_args.push(format!("{k}={v}"));
        }

        for (k, v) in env.iter() {
            docker_args.push("-e".to_string());
            docker_args.push(format!("{k}={v}"));
        }

        match interactive {
            Interactive::Yes => docker_args.push("-it".to_string()),
            Interactive::No => docker_args.push("-i".to_string()),
        }
        docker_args.push(self.connection_options.container_id.to_string());

        docker_args.append(&mut inner_program);

        Ok(host_docker_command_template(
            self.docker_cli(),
            &self.connection_options.host,
            docker_args,
        ))
    }

    fn build_forward_ports_command(
        &self,
        _forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        Err(anyhow::anyhow!("Not currently supported for docker_exec"))
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Docker(self.connection_options.clone())
    }

    fn path_style(&self) -> PathStyle {
        self.path_style.unwrap_or(PathStyle::Posix)
    }

    fn shell(&self) -> String {
        self.shell.clone()
    }

    fn default_system_shell(&self) -> String {
        String::from("/bin/sh")
    }
}

#[cfg(test)]
mod tests {
    use super::{host_docker_command_template, wsl_unc_path_to_posix};
    use crate::{DockerHost, SshConnectionOptions};

    #[test]
    fn should_convert_wsl_unc_path_case_insensitively() {
        let converted = wsl_unc_path_to_posix(
            r"\\wsl.localhost\arch\home\arch\project\.devcontainer",
            "Arch",
        );

        assert_eq!(
            converted.as_deref(),
            Some("/home/arch/project/.devcontainer")
        );
    }

    #[test]
    fn should_convert_wsl_dollar_unc_path_case_insensitively() {
        let converted = wsl_unc_path_to_posix(r"\\wsl$\ARCH\home\arch\project", "Arch");

        assert_eq!(converted.as_deref(), Some("/home/arch/project"));
    }

    #[test]
    fn should_build_ssh_docker_command_template() {
        let template = host_docker_command_template(
            "docker",
            &DockerHost::Ssh(SshConnectionOptions {
                host: "ssh.example.com".into(),
                username: Some("arch".to_string()),
                port: Some(2222),
                args: Some(vec!["-J".to_string(), "jumpbox".to_string()]),
                ..Default::default()
            }),
            vec!["exec".to_string(), "container-id".to_string()],
        );

        assert_eq!(template.program, "ssh");
        assert_eq!(
            template.args,
            vec![
                "-J".to_string(),
                "jumpbox".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "arch@ssh.example.com".to_string(),
                "docker".to_string(),
                "exec".to_string(),
                "container-id".to_string(),
            ]
        );
    }
}
