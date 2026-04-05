use std::{io, process::Output};

use async_trait::async_trait;
use futures::{AsyncBufReadExt as _, io::BufReader};
use serde::Deserialize;
use util::command::{Command, Stdio};

use crate::devcontainer_api::{
    DevContainerError, DevContainerLogStream, DevContainerProgressCallback, emit_command_output,
    emit_log_line,
};

pub(crate) struct DefaultCommandRunner;

impl DefaultCommandRunner {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn with_progress(
        progress: Option<DevContainerProgressCallback>,
    ) -> ReportingCommandRunner {
        ReportingCommandRunner { progress }
    }
}

#[async_trait]
impl CommandRunner for DefaultCommandRunner {
    async fn run_command(&self, command: &mut Command) -> Result<Output, std::io::Error> {
        command.output().await
    }
}

#[async_trait]
pub(crate) trait CommandRunner: Send + Sync {
    async fn run_command(&self, command: &mut Command) -> Result<Output, std::io::Error>;
}

pub(crate) struct ReportingCommandRunner {
    progress: Option<DevContainerProgressCallback>,
}

#[async_trait]
impl CommandRunner for ReportingCommandRunner {
    async fn run_command(&self, command: &mut Command) -> Result<Output, std::io::Error> {
        emit_log_line(
            self.progress.as_ref(),
            DevContainerLogStream::Info,
            format_command(command),
        );
        let output = run_command_with_progress(command, self.progress.as_ref()).await;
        if let Ok(output) = &output {
            emit_command_output(None, output);
        }
        output
    }
}

pub(crate) async fn run_command_with_progress(
    command: &mut Command,
    progress: Option<&DevContainerProgressCallback>,
) -> Result<Output, io::Error> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("failed to capture stderr"))?;

    let (stdout, stderr, status) = futures::try_join!(
        collect_stream_output(stdout, DevContainerLogStream::Stdout, progress),
        collect_stream_output(stderr, DevContainerLogStream::Stderr, progress),
        child.status(),
    )?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn collect_stream_output<R>(
    reader: R,
    stream: DevContainerLogStream,
    progress: Option<&DevContainerProgressCallback>,
) -> Result<Vec<u8>, io::Error>
where
    R: futures::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut output = Vec::new();
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line).await?;
        if bytes_read == 0 {
            break;
        }

        output.extend_from_slice(&line);
        emit_log_bytes(progress, stream, &line);
    }

    Ok(output)
}

fn emit_log_bytes(
    progress: Option<&DevContainerProgressCallback>,
    stream: DevContainerLogStream,
    bytes: &[u8],
) {
    let line = String::from_utf8_lossy(bytes);
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return;
    }

    emit_log_line(progress, stream, line.to_string());
}

pub(crate) async fn evaluate_json_command_with_progress<T>(
    mut command: Command,
    progress: Option<&DevContainerProgressCallback>,
) -> Result<Option<T>, DevContainerError>
where
    T: for<'de> Deserialize<'de>,
{
    emit_log_line(
        progress,
        DevContainerLogStream::Info,
        format_command(&command),
    );
    let output = run_command_with_progress(&mut command, progress)
        .await
        .map_err(|e| {
            log::error!("Error running command {:?}: {e}", command);
            DevContainerError::CommandFailed(command.get_program().display().to_string())
        })?;
    emit_command_output(None, &output);

    deserialize_json_output(output).map_err(|e| {
        log::error!("Error running command {:?}: {e}", command);
        DevContainerError::CommandFailed(command.get_program().display().to_string())
    })
}

fn format_command(command: &Command) -> String {
    let args = command
        .get_args()
        .map(|arg| arg.display().to_string())
        .collect::<Vec<_>>();
    if args.is_empty() {
        format!("$ {}", command.get_program().display())
    } else {
        format!("$ {} {}", command.get_program().display(), args.join(" "))
    }
}

pub(crate) fn deserialize_json_output<T>(output: Output) -> Result<Option<T>, String>
where
    T: for<'de> Deserialize<'de>,
{
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout);
        if raw.is_empty() || raw.trim() == "[]" || raw.trim() == "{}" {
            return Ok(None);
        }
        let value = serde_json_lenient::from_str(&raw)
            .map_err(|e| format!("Error deserializing from raw json: {e}"));
        value
    } else {
        let std_err = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Sent non-successful output; cannot deserialize. StdErr: {std_err}"
        ))
    }
}
