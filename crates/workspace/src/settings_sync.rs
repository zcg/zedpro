use std::sync::{Arc, Weak};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use chrono::Local;
use credentials_provider::CredentialsProvider;
use fs::Fs;
use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, Global};
use http_client::{HttpClient, github};
use paths::{config_dir, keymap_file, settings_file};
use settings::{KeymapFile, RegisterSetting, Settings, SettingsStore, watch_config_file};

use crate::AppState;

const SETTINGS_SYNC_CREDENTIALS_URL: &str = "https://api.github.com/zedpro/settings-sync";
const DEFAULT_SYNC_REPO: &str = "zed_settings";

#[derive(Clone, Debug, RegisterSetting)]
pub struct SettingsSyncSettings {
    pub enabled: bool,
    pub repo_name: String,
    pub auto_sync_on_change: bool,
    pub sync_windows: bool,
    pub sync_macos: bool,
    pub sync_linux: bool,
    pub include_settings: bool,
    pub include_keymap: bool,
}

impl Settings for SettingsSyncSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let settings = content.settings_sync.clone().unwrap_or_default();
        Self {
            enabled: settings.enabled.unwrap_or(false),
            repo_name: settings
                .repo_name
                .filter(|repo| !repo.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_SYNC_REPO.to_string()),
            auto_sync_on_change: settings.auto_sync_on_change.unwrap_or(false),
            sync_windows: settings.sync_windows.unwrap_or(true),
            sync_macos: settings.sync_macos.unwrap_or(true),
            sync_linux: settings.sync_linux.unwrap_or(true),
            include_settings: settings.include_settings.unwrap_or(true),
            include_keymap: settings.include_keymap.unwrap_or(true),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SettingsSyncSnapshot {
    pub app_github_login: Option<String>,
    pub repo_name: String,
    pub sync_owner_login: Option<String>,
    pub token_available: bool,
    pub enabled: bool,
    pub auto_sync_on_change: bool,
    pub is_syncing: bool,
    pub last_action: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub last_message: Option<String>,
    pub synced_files: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
enum SyncDirection {
    Push,
    Pull,
}

impl SyncDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Push => "Push to GitHub",
            Self::Pull => "Pull from GitHub",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
enum CurrentPlatform {
    Windows,
    Mac,
    Linux,
}

impl CurrentPlatform {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Mac => "mac",
            Self::Linux => "linux",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Mac => "macOS",
            Self::Linux => "Linux",
        }
    }

    fn enabled(self, settings: &SettingsSyncSettings) -> bool {
        match self {
            Self::Windows => settings.sync_windows,
            Self::Mac => settings.sync_macos,
            Self::Linux => settings.sync_linux,
        }
    }
}

#[cfg(target_os = "windows")]
fn current_platform() -> CurrentPlatform {
    CurrentPlatform::Windows
}

#[cfg(target_os = "macos")]
fn current_platform() -> CurrentPlatform {
    CurrentPlatform::Mac
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn current_platform() -> CurrentPlatform {
    CurrentPlatform::Linux
}

struct GlobalSettingsSync(Entity<SettingsSyncState>);

impl Global for GlobalSettingsSync {}

pub struct SettingsSyncState {
    app_state: Weak<AppState>,
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    token_available: bool,
    is_syncing: bool,
    pending_auto_sync: bool,
    sync_owner_login: Option<String>,
    last_action: Option<String>,
    last_success_at: Option<String>,
    last_error: Option<String>,
    last_message: Option<String>,
    synced_files: Vec<String>,
}

impl SettingsSyncState {
    pub fn init_global(app_state: Weak<AppState>, cx: &mut App) -> Entity<Self> {
        let state = cx.new(|cx| Self::new(app_state, cx));
        cx.set_global(GlobalSettingsSync(state.clone()));
        state
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSettingsSync>()
            .map(|global| global.0.clone())
    }

    fn new(app_state: Weak<AppState>, cx: &mut Context<Self>) -> Self {
        let app_state_ref = app_state
            .upgrade()
            .expect("settings sync state requires an initialized AppState");
        let fs = app_state_ref.fs.clone();
        let http_client = app_state_ref.client.http_client();
        let credentials_provider = <dyn CredentialsProvider>::global(cx);

        let this = Self {
            app_state,
            fs: fs.clone(),
            http_client,
            credentials_provider,
            token_available: false,
            is_syncing: false,
            pending_auto_sync: false,
            sync_owner_login: None,
            last_action: None,
            last_success_at: None,
            last_error: None,
            last_message: None,
            synced_files: Vec::new(),
        };

        this.spawn_watchers(fs, cx);
        let provider = <dyn CredentialsProvider>::global(cx);
        cx.spawn(async move |this, cx| {
            let token_available = Self::load_token(&provider, cx)
                .await
                .ok()
                .flatten()
                .is_some();
            this.update(cx, |this, cx| {
                this.token_available = token_available;
                cx.notify();
            })
            .ok();
        })
        .detach();

        this
    }

    fn spawn_watchers(&self, fs: Arc<dyn Fs>, cx: &mut Context<Self>) {
        let (mut settings_rx, settings_task) = watch_config_file(
            cx.background_executor(),
            fs.clone(),
            settings_file().clone(),
        );
        settings_task.detach();
        cx.spawn(async move |this, cx| {
            let mut first = true;
            while settings_rx.next().await.is_some() {
                if first {
                    first = false;
                    continue;
                }
                this.update(cx, |this, cx| {
                    this.on_local_file_changed("settings.json", cx)
                })
                .ok();
            }
        })
        .detach();

        let (mut keymap_rx, keymap_task) =
            watch_config_file(cx.background_executor(), fs, keymap_file().clone());
        keymap_task.detach();
        cx.spawn(async move |this, cx| {
            let mut first = true;
            while keymap_rx.next().await.is_some() {
                if first {
                    first = false;
                    continue;
                }
                this.update(cx, |this, cx| this.on_local_file_changed("keymap.json", cx))
                    .ok();
            }
        })
        .detach();
    }

    pub fn snapshot(&self, cx: &App) -> SettingsSyncSnapshot {
        let settings = SettingsSyncSettings::get_global(cx).clone();
        let app_github_login = self.app_state.upgrade().and_then(|state| {
            state
                .user_store
                .read(cx)
                .current_user()
                .map(|user| user.github_login.to_string())
        });

        SettingsSyncSnapshot {
            app_github_login,
            repo_name: settings.repo_name.clone(),
            sync_owner_login: self.sync_owner_login.clone(),
            token_available: self.token_available,
            enabled: settings.enabled,
            auto_sync_on_change: settings.auto_sync_on_change,
            is_syncing: self.is_syncing,
            last_action: self.last_action.clone(),
            last_success_at: self.last_success_at.clone(),
            last_error: self.last_error.clone(),
            last_message: self.last_message.clone(),
            synced_files: self.synced_files.clone(),
        }
    }

    pub fn save_token(&mut self, token: Option<String>, cx: &mut Context<Self>) {
        let credentials_provider = self.credentials_provider.clone();
        let normalized = token.and_then(|token| {
            let trimmed = token.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        });
        self.last_error = None;
        self.last_message = Some(if normalized.is_some() {
            "Saving GitHub sync token…".to_string()
        } else {
            "Clearing GitHub sync token…".to_string()
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = if let Some(token) = normalized.clone() {
                credentials_provider
                    .write_credentials(
                        SETTINGS_SYNC_CREDENTIALS_URL,
                        "github",
                        token.as_bytes(),
                        cx,
                    )
                    .await
            } else {
                credentials_provider
                    .delete_credentials(SETTINGS_SYNC_CREDENTIALS_URL, cx)
                    .await
            };

            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.token_available = normalized.is_some();
                        this.last_message = Some(if normalized.is_some() {
                            "GitHub sync token was saved to the system credential store."
                                .to_string()
                        } else {
                            "GitHub sync token was cleared.".to_string()
                        });
                        this.last_error = None;
                    }
                    Err(err) => {
                        this.last_error = Some(err.to_string());
                        this.last_message =
                            Some("Failed to save the GitHub sync token.".to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.start_sync(SyncDirection::Push, false, cx);
    }

    pub fn pull_now(&mut self, cx: &mut Context<Self>) {
        self.start_sync(SyncDirection::Pull, false, cx);
    }

    fn on_local_file_changed(&mut self, source: &'static str, cx: &mut Context<Self>) {
        let settings = SettingsSyncSettings::get_global(cx).clone();
        if !settings.enabled
            || !settings.auto_sync_on_change
            || !current_platform().enabled(&settings)
        {
            return;
        }

        if self.is_syncing {
            self.pending_auto_sync = true;
            self.last_message = Some(format!(
                "Detected a change in {source}. Another sync will run after the current one finishes."
            ));
            cx.notify();
            return;
        }

        self.start_sync(SyncDirection::Push, true, cx);
    }

    fn start_sync(
        &mut self,
        direction: SyncDirection,
        is_auto_triggered: bool,
        cx: &mut Context<Self>,
    ) {
        if self.is_syncing {
            if is_auto_triggered {
                self.pending_auto_sync = true;
            } else {
                self.last_message = Some("A sync task is already running.".to_string());
                cx.notify();
            }
            return;
        }

        let settings = SettingsSyncSettings::get_global(cx).clone();
        if !settings.enabled {
            self.last_error = Some("Enable account sync in settings first.".to_string());
            self.last_message = Some("Account sync is disabled.".to_string());
            cx.notify();
            return;
        }

        if !current_platform().enabled(&settings) {
            self.last_error = Some(format!(
                "{} sync is disabled by the current settings.",
                current_platform().display_name()
            ));
            self.last_message = Some("Sync is disabled for the current platform.".to_string());
            cx.notify();
            return;
        }

        self.is_syncing = true;
        self.last_action = Some(direction.label().to_string());
        self.last_error = None;
        self.last_message = Some(if is_auto_triggered {
            format!(
                "Detected local config changes. Automatically running {}…",
                direction.label()
            )
        } else {
            format!("Running {}…", direction.label())
        });
        cx.notify();

        let credentials_provider = self.credentials_provider.clone();
        let fs = self.fs.clone();
        let http_client = self.http_client.clone();
        let repo_name = settings.repo_name.clone();
        let include_settings = settings.include_settings;
        let include_keymap = settings.include_keymap;
        let platform = current_platform();

        cx.spawn(async move |this, cx| {
            let result = perform_sync(
                direction,
                platform,
                repo_name,
                include_settings,
                include_keymap,
                fs,
                http_client,
                credentials_provider,
                cx,
            )
            .await;

            this.update(cx, |this, cx| {
                this.is_syncing = false;
                match result {
                    Ok(result) => {
                        this.token_available = true;
                        this.sync_owner_login = Some(result.owner_login);
                        this.last_success_at =
                            Some(Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
                        this.last_error = None;
                        this.last_message = Some(result.message);
                        this.synced_files = result.files;
                    }
                    Err(err) => {
                        if err.to_string().contains("missing GitHub sync token") {
                            this.token_available = false;
                        }
                        this.last_error = Some(err.to_string());
                        this.last_message = Some("Sync did not complete.".to_string());
                    }
                }

                let should_run_pending_auto_sync = this.pending_auto_sync;
                this.pending_auto_sync = false;
                cx.notify();

                if should_run_pending_auto_sync {
                    this.start_sync(SyncDirection::Push, true, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    async fn load_token(
        credentials_provider: &Arc<dyn CredentialsProvider>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<Option<String>> {
        let credentials = credentials_provider
            .read_credentials(SETTINGS_SYNC_CREDENTIALS_URL, cx)
            .await?;
        Ok(credentials.map(|(_, bytes)| String::from_utf8_lossy(&bytes).to_string()))
    }
}

struct SyncOutcome {
    owner_login: String,
    message: String,
    files: Vec<String>,
}

async fn perform_sync(
    direction: SyncDirection,
    platform: CurrentPlatform,
    repo_name: String,
    include_settings: bool,
    include_keymap: bool,
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &mut gpui::AsyncApp,
) -> Result<SyncOutcome> {
    let token = SettingsSyncState::load_token(&credentials_provider, cx)
        .await?
        .ok_or_else(|| anyhow!("missing GitHub sync token"))?;

    let user = github::current_user(&token, http_client.clone()).await?;
    let owner_login = user.login;
    let repo = match github::get_repo(&owner_login, &repo_name, &token, http_client.clone()).await?
    {
        Some(repo) => repo,
        None if matches!(direction, SyncDirection::Push) => {
            github::create_private_repo(&repo_name, &token, http_client.clone()).await?
        }
        None => bail!("GitHub private repository `{repo_name}` was not found. Run a push first."),
    };

    match direction {
        SyncDirection::Push => {
            let files = push_local_files(
                platform,
                include_settings,
                include_keymap,
                &owner_login,
                &repo.name,
                &token,
                fs,
                http_client,
            )
            .await?;
            Ok(SyncOutcome {
                owner_login,
                message: format!(
                    "Pushed {} file(s) to {}/{} under `{}`.",
                    files.len(),
                    repo.owner.login,
                    repo.name,
                    platform.dir_name()
                ),
                files,
            })
        }
        SyncDirection::Pull => {
            let files = pull_remote_files(
                platform,
                include_settings,
                include_keymap,
                &owner_login,
                &repo.name,
                &token,
                fs,
                http_client,
            )
            .await?;
            Ok(SyncOutcome {
                owner_login,
                message: format!(
                    "Pulled {} file(s) from {}/{} under `{}`.",
                    files.len(),
                    repo.owner.login,
                    repo.name,
                    platform.dir_name(),
                ),
                files,
            })
        }
    }
}

async fn push_local_files(
    platform: CurrentPlatform,
    include_settings: bool,
    include_keymap: bool,
    owner: &str,
    repo: &str,
    token: &str,
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
) -> Result<Vec<String>> {
    let mut uploaded_files = Vec::new();

    if include_settings {
        let contents = SettingsStore::load_settings(&fs).await?;
        let remote_path = format!("{}/settings.json", platform.dir_name());
        upload_file(
            owner,
            repo,
            token,
            &remote_path,
            format!("Sync settings for {}", platform.dir_name()),
            contents,
            http_client.clone(),
        )
        .await?;
        uploaded_files.push(remote_path);
    }

    if include_keymap {
        let contents = KeymapFile::load_keymap_file(&fs).await?;
        let remote_path = format!("{}/keymap.json", platform.dir_name());
        upload_file(
            owner,
            repo,
            token,
            &remote_path,
            format!("Sync keymap for {}", platform.dir_name()),
            contents,
            http_client,
        )
        .await?;
        uploaded_files.push(remote_path);
    }

    if uploaded_files.is_empty() {
        bail!("No files are enabled for sync.");
    }

    Ok(uploaded_files)
}

async fn pull_remote_files(
    platform: CurrentPlatform,
    include_settings: bool,
    include_keymap: bool,
    owner: &str,
    repo: &str,
    token: &str,
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
) -> Result<Vec<String>> {
    if fs.metadata(config_dir()).await?.is_none() {
        fs.create_dir(config_dir()).await?;
    }

    let mut restored_files = Vec::new();

    if include_settings {
        let remote_path = format!("{}/settings.json", platform.dir_name());
        if let Some(file) =
            github::get_repo_content(owner, repo, &remote_path, token, http_client.clone()).await?
        {
            let content = decode_repo_content(&file)?;
            fs.atomic_write(settings_file().clone(), content)
                .await
                .context("failed to write local settings.json")?;
            restored_files.push(remote_path);
        }
    }

    if include_keymap {
        let remote_path = format!("{}/keymap.json", platform.dir_name());
        if let Some(file) =
            github::get_repo_content(owner, repo, &remote_path, token, http_client.clone()).await?
        {
            let content = decode_repo_content(&file)?;
            fs.atomic_write(keymap_file().clone(), content)
                .await
                .context("failed to write local keymap.json")?;
            restored_files.push(remote_path);
        }
    }

    if restored_files.is_empty() {
        bail!(
            "No synced files were found under `{}`.",
            platform.dir_name()
        );
    }

    Ok(restored_files)
}

async fn upload_file(
    owner: &str,
    repo: &str,
    token: &str,
    remote_path: &str,
    message: String,
    contents: String,
    http_client: Arc<dyn HttpClient>,
) -> Result<()> {
    let sha = github::get_repo_content(owner, repo, remote_path, token, http_client.clone())
        .await?
        .map(|file| file.sha);

    github::put_repo_content(
        owner,
        repo,
        remote_path,
        &message,
        base64::engine::general_purpose::STANDARD.encode(contents.as_bytes()),
        sha,
        token,
        http_client,
    )
    .await
}

fn decode_repo_content(file: &github::GithubContentFile) -> Result<String> {
    let content = file
        .content
        .as_ref()
        .context("GitHub returned an empty file body")?
        .replace('\n', "");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(content)
        .context("failed to decode the GitHub file body")?;
    String::from_utf8(decoded).context("GitHub file content was not valid UTF-8")
}
