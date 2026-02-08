use anyhow::{Context as _, Result, anyhow};
use convert_case::{Case, Casing};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
use google_ai::{GoogleAuthMode, GoogleTransportOptions};
use gpui::{AnyView, App, AsyncApp, Context, Entity, SharedString, Task, Window};
use http_client::HttpClient;
use language_model::{
    ApiKeyState, AuthenticateError, EnvVar, IconOrSvg, LanguageModel, LanguageModelCompletionError,
    LanguageModelCompletionEvent, LanguageModelId, LanguageModelName, LanguageModelProvider,
    LanguageModelProviderId, LanguageModelProviderName, LanguageModelProviderState,
    LanguageModelRequest, LanguageModelToolChoice, LanguageModelToolSchemaFormat, RateLimiter,
};
use menu;
pub use settings::GoogleAvailableModel as AvailableModel;
use settings::{Settings, SettingsStore};
use std::sync::Arc;
use ui::{ButtonLink, ConfiguredApiCard, ElevationIndex, Tooltip, prelude::*};
use ui_input::InputField;
use util::ResultExt;

use crate::provider::google::{GoogleEventMapper, count_google_tokens, into_google};

#[derive(Default, Clone, Debug, PartialEq)]
pub struct GoogleCompatibleSettings {
    pub api_url: String,
    pub available_models: Vec<AvailableModel>,
    pub request_compat: GoogleRequestCompatSettings,
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum GoogleCompatibleAuthMode {
    #[default]
    Auto,
    Query,
    XGoogApiKey,
    Bearer,
}

impl From<settings::GoogleAuthMode> for GoogleCompatibleAuthMode {
    fn from(value: settings::GoogleAuthMode) -> Self {
        match value {
            settings::GoogleAuthMode::Auto => Self::Auto,
            settings::GoogleAuthMode::Query => Self::Query,
            settings::GoogleAuthMode::XGoogApiKey => Self::XGoogApiKey,
            settings::GoogleAuthMode::Bearer => Self::Bearer,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleRequestCompatSettings {
    pub auth_mode: GoogleCompatibleAuthMode,
    pub api_version: Option<String>,
    pub allow_stream_fallback: bool,
    pub allow_count_tokens_fallback: bool,
}

impl Default for GoogleRequestCompatSettings {
    fn default() -> Self {
        Self {
            auth_mode: GoogleCompatibleAuthMode::Auto,
            api_version: Some("v1beta".to_string()),
            allow_stream_fallback: true,
            allow_count_tokens_fallback: true,
        }
    }
}

impl From<settings::GoogleRequestCompatContent> for GoogleRequestCompatSettings {
    fn from(value: settings::GoogleRequestCompatContent) -> Self {
        Self {
            auth_mode: value.auth_mode.map(Into::into).unwrap_or_default(),
            api_version: value.api_version.or(Some("v1beta".to_string())),
            allow_stream_fallback: value.allow_stream_fallback.unwrap_or(true),
            allow_count_tokens_fallback: value.allow_count_tokens_fallback.unwrap_or(true),
        }
    }
}

pub struct GoogleCompatibleLanguageModelProvider {
    id: LanguageModelProviderId,
    name: LanguageModelProviderName,
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
}

pub struct State {
    id: Arc<str>,
    api_key_state: ApiKeyState,
    settings: GoogleCompatibleSettings,
}

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key_state.has_key()
    }

    fn set_api_key(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let api_url = SharedString::new(self.settings.api_url.as_str());
        self.api_key_state
            .store(api_url, api_key, |this| &mut this.api_key_state, cx)
    }

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let api_url = SharedString::new(self.settings.api_url.clone());
        self.api_key_state
            .load_if_needed(api_url, |this| &mut this.api_key_state, cx)
    }
}

impl GoogleCompatibleLanguageModelProvider {
    pub fn new(id: Arc<str>, http_client: Arc<dyn HttpClient>, cx: &mut App) -> Self {
        fn resolve_settings<'a>(id: &'a str, cx: &'a App) -> Option<&'a GoogleCompatibleSettings> {
            crate::AllLanguageModelSettings::get_global(cx)
                .google_compatible
                .get(id)
        }

        let api_key_env_var_name = format!("{}_API_KEY", id).to_case(Case::UpperSnake).into();
        let state = cx.new(|cx| {
            cx.observe_global::<SettingsStore>(|this: &mut State, cx| {
                let Some(settings) = resolve_settings(&this.id, cx).cloned() else {
                    return;
                };
                if this.settings != settings {
                    let api_url = SharedString::new(settings.api_url.as_str());
                    this.api_key_state
                        .handle_url_change(api_url, |this| &mut this.api_key_state, cx);
                    this.settings = settings;
                    cx.notify();
                }
            })
            .detach();

            let settings = resolve_settings(&id, cx).cloned().unwrap_or_default();
            State {
                id: id.clone(),
                api_key_state: ApiKeyState::new(
                    SharedString::new(settings.api_url.as_str()),
                    EnvVar::new(api_key_env_var_name),
                ),
                settings,
            }
        });

        Self {
            id: id.clone().into(),
            name: id.into(),
            http_client,
            state,
        }
    }

    fn create_language_model(&self, model: google_ai::Model) -> Arc<dyn LanguageModel> {
        Arc::new(GoogleCompatibleLanguageModel {
            id: LanguageModelId::from(model.id().to_string()),
            provider_id: self.id.clone(),
            provider_name: self.name.clone(),
            model,
            state: self.state.clone(),
            http_client: self.http_client.clone(),
            request_limiter: RateLimiter::new(4),
        })
    }
}

impl LanguageModelProviderState for GoogleCompatibleLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for GoogleCompatibleLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelProviderName {
        self.name.clone()
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::AiGoogle)
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        self.state
            .read(cx)
            .settings
            .available_models
            .first()
            .map(|model| {
                self.create_language_model(google_ai::Model::Custom {
                    name: model.name.clone(),
                    display_name: model.display_name.clone(),
                    max_tokens: model.max_tokens,
                    mode: model.mode.unwrap_or_default(),
                })
            })
    }

    fn default_fast_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        None
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        self.state
            .read(cx)
            .settings
            .available_models
            .iter()
            .map(|model| {
                self.create_language_model(google_ai::Model::Custom {
                    name: model.name.clone(),
                    display_name: model.display_name.clone(),
                    max_tokens: model.max_tokens,
                    mode: model.mode.unwrap_or_default(),
                })
            })
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn configuration_view(
        &self,
        _target_agent: language_model::ConfigurationViewTargetAgent,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        cx.new(|cx| ConfigurationView::new(self.state.clone(), window, cx))
            .into()
    }

    fn reset_credentials(&self, cx: &mut App) -> Task<Result<()>> {
        self.state
            .update(cx, |state, cx| state.set_api_key(None, cx))
    }
}

pub struct GoogleCompatibleLanguageModel {
    id: LanguageModelId,
    provider_id: LanguageModelProviderId,
    provider_name: LanguageModelProviderName,
    model: google_ai::Model,
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

fn into_transport_options(request_compat: &GoogleRequestCompatSettings) -> GoogleTransportOptions {
    let auth_mode = match request_compat.auth_mode {
        GoogleCompatibleAuthMode::Auto => GoogleAuthMode::Auto,
        GoogleCompatibleAuthMode::Query => GoogleAuthMode::Query,
        GoogleCompatibleAuthMode::XGoogApiKey => GoogleAuthMode::XGoogApiKey,
        GoogleCompatibleAuthMode::Bearer => GoogleAuthMode::Bearer,
    };
    GoogleTransportOptions {
        auth_mode,
        api_version: request_compat.api_version.clone(),
    }
}

impl LanguageModel for GoogleCompatibleLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name().to_string())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        self.provider_id.clone()
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        self.provider_name.clone()
    }

    fn supports_tools(&self) -> bool {
        self.model.supports_tools()
    }

    fn supports_images(&self) -> bool {
        self.model.supports_images()
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        matches!(
            choice,
            LanguageModelToolChoice::Auto
                | LanguageModelToolChoice::Any
                | LanguageModelToolChoice::None
        )
    }

    fn tool_input_format(&self) -> LanguageModelToolSchemaFormat {
        LanguageModelToolSchemaFormat::JsonSchemaSubset
    }

    fn telemetry_id(&self) -> String {
        format!(
            "gemini-compatible/{}/{}",
            self.provider_id.0,
            self.model.request_id()
        )
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_token_count()
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.model.max_output_tokens()
    }

    fn count_tokens(
        &self,
        request: LanguageModelRequest,
        cx: &App,
    ) -> BoxFuture<'static, Result<u64>> {
        let fallback_on_missing_key = count_google_tokens(request.clone(), cx);
        let fallback_on_error = count_google_tokens(request.clone(), cx);
        let request_for_api = into_google(
            request,
            self.model.request_id().to_string(),
            self.model.mode(),
        );

        let http_client = self.http_client.clone();
        let provider_name = self.provider_name.clone();
        let (api_key, api_url, request_compat) = self.state.read_with(cx, |state, _cx| {
            (
                state.api_key_state.key(&state.settings.api_url).map(|k| k.to_string()),
                state.settings.api_url.clone(),
                state.settings.request_compat.clone(),
            )
        });

        async move {
            let Some(api_key) = api_key else {
                return fallback_on_missing_key.await;
            };

            let response = google_ai::count_tokens_with_options(
                http_client.as_ref(),
                &api_url,
                &api_key,
                google_ai::CountTokensRequest {
                    generate_content_request: request_for_api,
                },
                &into_transport_options(&request_compat),
            )
            .await;

            match response {
                Ok(response) => Ok(response.total_tokens),
                Err(error) => {
                    if request_compat.allow_count_tokens_fallback {
                        log::warn!(
                            "gemini-compatible count_tokens failed, fallback to estimator: {error:#}"
                        );
                        fallback_on_error.await
                    } else {
                        Err(anyhow!(
                            "{provider_name}: failed to count tokens: {error:#}"
                        ))
                    }
                }
            }
        }
        .boxed()
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        let bypass_rate_limit = request.bypass_rate_limit;
        let stream_request = into_google(
            request.clone(),
            self.model.request_id().to_string(),
            self.model.mode(),
        );
        let fallback_request = into_google(
            request,
            self.model.request_id().to_string(),
            self.model.mode(),
        );

        let http_client = self.http_client.clone();
        let provider = self.provider_name.clone();
        let (api_key, api_url, request_compat) = self.state.read_with(cx, |state, _cx| {
            (
                state.api_key_state.key(&state.settings.api_url).map(|k| k.to_string()),
                state.settings.api_url.clone(),
                state.settings.request_compat.clone(),
            )
        });

        let future = self.request_limiter.stream_with_bypass(
            async move {
                let Some(api_key) = api_key else {
                    return Err(LanguageModelCompletionError::NoApiKey { provider });
                };
                let response = google_ai::stream_generate_content_with_options(
                    http_client.as_ref(),
                    &api_url,
                    &api_key,
                    stream_request,
                    &into_transport_options(&request_compat),
                )
                .await;

                match response {
                    Ok(response) => Ok(GoogleEventMapper::new().map_stream(response).boxed()),
                    Err(error) => {
                        if request_compat.allow_stream_fallback {
                            log::warn!(
                                "gemini-compatible streaming failed, fallback to generateContent: {error:#}"
                            );
                            let response = google_ai::generate_content_with_options(
                                http_client.as_ref(),
                                &api_url,
                                &api_key,
                                fallback_request,
                                &into_transport_options(&request_compat),
                            )
                            .await
                            .context("failed to fallback generateContent")
                            .map_err(LanguageModelCompletionError::from)?;
                            let stream = futures::stream::iter(vec![Ok(response)]).boxed();
                            Ok(GoogleEventMapper::new().map_stream(stream).boxed())
                        } else {
                            Err(LanguageModelCompletionError::from(anyhow!(
                                "failed to stream completion: {error:#}"
                            )))
                        }
                    }
                }
            },
            bypass_rate_limit,
        );

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

struct ConfigurationView {
    api_key_editor: Entity<InputField>,
    state: Entity<State>,
    load_credentials_task: Option<Task<()>>,
}

impl ConfigurationView {
    fn new(state: Entity<State>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let api_key_editor = cx.new(|cx| InputField::new(window, cx, "AIzaSy..."));

        cx.observe(&state, |_, _, cx| cx.notify()).detach();

        let load_credentials_task = Some(cx.spawn_in(window, {
            let state = state.clone();
            async move |this, cx| {
                if let Some(task) = Some(state.update(cx, |state, cx| state.authenticate(cx))) {
                    let _ = task.await;
                }

                this.update(cx, |this, cx| {
                    this.load_credentials_task = None;
                    cx.notify();
                })
                .log_err();
            }
        }));

        Self {
            api_key_editor,
            state,
            load_credentials_task,
        }
    }

    fn save_api_key(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor.read(cx).text(cx).trim().to_string();
        if api_key.is_empty() {
            return;
        }

        self.api_key_editor
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(Some(api_key), cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn reset_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(None, cx))
                .await
        })
        .detach_and_log_err(cx);
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let env_var_set = state.api_key_state.is_from_env_var();
        let env_var_name = state.api_key_state.env_var_name();

        let api_key_section = if !state.is_authenticated() {
            v_flex()
                .on_action(cx.listener(Self::save_api_key))
                .child(Label::new(
                    "To use Zed's agent with a Gemini-compatible provider, you need to add an API key.",
                ))
                .child(
                    div()
                        .pt(DynamicSpacing::Base04.rems(cx))
                        .child(self.api_key_editor.clone()),
                )
                .child(
                    Label::new(
                        format!("You can also set the {env_var_name} environment variable and restart Zed."),
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any()
        } else {
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(
                            div()
                                .w_full()
                                .overflow_x_hidden()
                                .text_ellipsis()
                                .child(Label::new(if env_var_set {
                                    format!("API key set in {env_var_name} environment variable")
                                } else {
                                    format!("API key configured for {}", &state.settings.api_url)
                                })),
                        ),
                )
                .child(
                    h_flex().flex_shrink_0().child(
                        Button::new("reset-api-key", "Reset API Key")
                            .label_size(LabelSize::Small)
                            .icon(IconName::Undo)
                            .icon_size(IconSize::Small)
                            .icon_position(IconPosition::Start)
                            .layer(ElevationIndex::ModalSurface)
                            .when(env_var_set, |this| {
                                this.tooltip(Tooltip::text(format!(
                                    "To reset your API key, unset the {env_var_name} environment variable."
                                )))
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_api_key(window, cx)
                            })),
                    ),
                )
                .into_any()
        };

        if self.load_credentials_task.is_some() {
            div().child(Label::new("Loading credentials…")).into_any()
        } else {
            v_flex()
                .size_full()
                .child(api_key_section)
                .child(
                    v_flex()
                        .mt_2()
                        .gap_1()
                        .child(ConfiguredApiCard::new("Gemini API compatible endpoint"))
                        .child(
                            Label::new("Uses Gemini GenerateContent API-compatible endpoints.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(h_flex().child(ButtonLink::new(
                            "Gemini API docs",
                            "https://ai.google.dev/gemini-api/docs",
                        ))),
                )
                .into_any()
        }
    }
}
