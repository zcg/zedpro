use anthropic::{
    AnthropicAuthMode, AnthropicError, AnthropicTransportOptions, ApiErrorCode, Response,
    ResponseContent,
};
use anyhow::Result;
use convert_case::{Case, Casing};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
use gpui::{AnyView, App, AsyncApp, Context, Entity, SharedString, Task, Window};
use http_client::HttpClient;
use language_model::{
    ApiKeyState, AuthenticateError, EnvVar, IconOrSvg, LanguageModel, LanguageModelCompletionError,
    LanguageModelCompletionEvent, LanguageModelId, LanguageModelName, LanguageModelProvider,
    LanguageModelProviderId, LanguageModelProviderName, LanguageModelProviderState,
    LanguageModelRequest, LanguageModelToolChoice, LanguageModelToolUse, RateLimiter, StopReason,
    TokenUsage,
};
use menu;
pub use settings::AnthropicAvailableModel as AvailableModel;
use settings::{Settings, SettingsStore};
use std::sync::Arc;
use ui::{ButtonLink, ConfiguredApiCard, ElevationIndex, Tooltip, prelude::*};
use ui_input::InputField;
use util::ResultExt;

use crate::provider::anthropic::{
    AnthropicEventMapper, count_anthropic_tokens_with_tiktoken, into_anthropic,
    into_anthropic_count_tokens_request,
};

#[derive(Default, Clone, Debug, PartialEq)]
pub struct AnthropicCompatibleSettings {
    pub api_url: String,
    pub available_models: Vec<AvailableModel>,
    pub request_compat: AnthropicRequestCompatSettings,
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum AnthropicCompatibleAuthMode {
    #[default]
    Auto,
    XApiKey,
    Bearer,
}

impl From<settings::AnthropicAuthMode> for AnthropicCompatibleAuthMode {
    fn from(value: settings::AnthropicAuthMode) -> Self {
        match value {
            settings::AnthropicAuthMode::Auto => Self::Auto,
            settings::AnthropicAuthMode::XApiKey => Self::XApiKey,
            settings::AnthropicAuthMode::Bearer => Self::Bearer,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnthropicRequestCompatSettings {
    pub auth_mode: AnthropicCompatibleAuthMode,
    pub anthropic_version: Option<String>,
    pub allow_stream_fallback: bool,
    pub allow_count_tokens_fallback: bool,
}

impl Default for AnthropicRequestCompatSettings {
    fn default() -> Self {
        Self {
            auth_mode: AnthropicCompatibleAuthMode::Auto,
            anthropic_version: Some("2023-06-01".to_string()),
            allow_stream_fallback: true,
            allow_count_tokens_fallback: true,
        }
    }
}

impl From<settings::AnthropicRequestCompatContent> for AnthropicRequestCompatSettings {
    fn from(value: settings::AnthropicRequestCompatContent) -> Self {
        Self {
            auth_mode: value.auth_mode.map(Into::into).unwrap_or_default(),
            anthropic_version: value
                .anthropic_version
                .or(Some("2023-06-01".to_string())),
            allow_stream_fallback: value.allow_stream_fallback.unwrap_or(true),
            allow_count_tokens_fallback: value.allow_count_tokens_fallback.unwrap_or(true),
        }
    }
}

pub struct AnthropicCompatibleLanguageModelProvider {
    id: LanguageModelProviderId,
    name: LanguageModelProviderName,
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
}

pub struct State {
    id: Arc<str>,
    api_key_state: ApiKeyState,
    settings: AnthropicCompatibleSettings,
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

impl AnthropicCompatibleLanguageModelProvider {
    pub fn new(id: Arc<str>, http_client: Arc<dyn HttpClient>, cx: &mut App) -> Self {
        fn resolve_settings<'a>(
            id: &'a str,
            cx: &'a App,
        ) -> Option<&'a AnthropicCompatibleSettings> {
            crate::AllLanguageModelSettings::get_global(cx)
                .anthropic_compatible
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

    fn create_language_model(&self, model: anthropic::Model) -> Arc<dyn LanguageModel> {
        Arc::new(AnthropicCompatibleLanguageModel {
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

impl LanguageModelProviderState for AnthropicCompatibleLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for AnthropicCompatibleLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelProviderName {
        self.name.clone()
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::AiAnthropic)
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        self.state
            .read(cx)
            .settings
            .available_models
            .first()
            .map(|model| {
                self.create_language_model(anthropic::Model::Custom {
                    name: model.name.clone(),
                    display_name: model.display_name.clone(),
                    max_tokens: model.max_tokens,
                    tool_override: model.tool_override.clone(),
                    cache_configuration: model.cache_configuration.as_ref().map(|config| {
                        anthropic::AnthropicModelCacheConfiguration {
                            max_cache_anchors: config.max_cache_anchors,
                            should_speculate: config.should_speculate,
                            min_total_token: config.min_total_token,
                        }
                    }),
                    max_output_tokens: model.max_output_tokens,
                    default_temperature: model.default_temperature,
                    extra_beta_headers: model.extra_beta_headers.clone(),
                    mode: model.mode.unwrap_or_default().into(),
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
                self.create_language_model(anthropic::Model::Custom {
                    name: model.name.clone(),
                    display_name: model.display_name.clone(),
                    max_tokens: model.max_tokens,
                    tool_override: model.tool_override.clone(),
                    cache_configuration: model.cache_configuration.as_ref().map(|config| {
                        anthropic::AnthropicModelCacheConfiguration {
                            max_cache_anchors: config.max_cache_anchors,
                            should_speculate: config.should_speculate,
                            min_total_token: config.min_total_token,
                        }
                    }),
                    max_output_tokens: model.max_output_tokens,
                    default_temperature: model.default_temperature,
                    extra_beta_headers: model.extra_beta_headers.clone(),
                    mode: model.mode.unwrap_or_default().into(),
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

pub struct AnthropicCompatibleLanguageModel {
    id: LanguageModelId,
    provider_id: LanguageModelProviderId,
    provider_name: LanguageModelProviderName,
    model: anthropic::Model,
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

fn map_anthropic_api_error(
    provider: LanguageModelProviderName,
    error: anthropic::ApiError,
) -> LanguageModelCompletionError {
    match error.code() {
        Some(code) => match code {
            ApiErrorCode::InvalidRequestError => LanguageModelCompletionError::BadRequestFormat {
                provider,
                message: error.message,
            },
            ApiErrorCode::AuthenticationError => {
                LanguageModelCompletionError::AuthenticationError {
                    provider,
                    message: error.message,
                }
            }
            ApiErrorCode::PermissionError => LanguageModelCompletionError::PermissionError {
                provider,
                message: error.message,
            },
            _ => LanguageModelCompletionError::UpstreamProviderError {
                message: error.message,
                status: http_client::StatusCode::BAD_REQUEST,
                retry_after: None,
            },
        },
        None => LanguageModelCompletionError::UpstreamProviderError {
            message: error.message,
            status: http_client::StatusCode::BAD_REQUEST,
            retry_after: None,
        },
    }
}

fn map_anthropic_error(
    provider: LanguageModelProviderName,
    error: AnthropicError,
) -> LanguageModelCompletionError {
    match error {
        AnthropicError::SerializeRequest(error) => {
            LanguageModelCompletionError::SerializeRequest { provider, error }
        }
        AnthropicError::BuildRequestBody(error) => {
            LanguageModelCompletionError::BuildRequestBody { provider, error }
        }
        AnthropicError::HttpSend(error) => LanguageModelCompletionError::HttpSend { provider, error },
        AnthropicError::DeserializeResponse(error) => {
            LanguageModelCompletionError::DeserializeResponse { provider, error }
        }
        AnthropicError::ReadResponse(error) => {
            LanguageModelCompletionError::ApiReadResponseError { provider, error }
        }
        AnthropicError::HttpResponseError {
            status_code,
            message,
        } => LanguageModelCompletionError::HttpResponseError {
            provider,
            status_code,
            message,
        },
        AnthropicError::RateLimit { retry_after } => LanguageModelCompletionError::RateLimitExceeded {
            provider,
            retry_after: Some(retry_after),
        },
        AnthropicError::ServerOverloaded { retry_after } => {
            LanguageModelCompletionError::ServerOverloaded {
                provider,
                retry_after,
            }
        }
        AnthropicError::ApiError(api_error) => map_anthropic_api_error(provider, api_error),
    }
}

fn convert_usage(usage: &anthropic::Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens.unwrap_or(0),
        output_tokens: usage.output_tokens.unwrap_or(0),
        cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
        cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
    }
}

fn parse_stop_reason(stop_reason: Option<&str>) -> StopReason {
    match stop_reason {
        Some("end_turn") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("tool_use") => StopReason::ToolUse,
        Some("refusal") => StopReason::Refusal,
        Some(other) => {
            log::warn!("anthropic-compatible stop_reason unsupported: {other}");
            StopReason::EndTurn
        }
        None => StopReason::EndTurn,
    }
}

fn into_transport_options(
    request_compat: &AnthropicRequestCompatSettings,
) -> AnthropicTransportOptions {
    let auth_mode = match request_compat.auth_mode {
        AnthropicCompatibleAuthMode::Auto => AnthropicAuthMode::Auto,
        AnthropicCompatibleAuthMode::XApiKey => AnthropicAuthMode::XApiKey,
        AnthropicCompatibleAuthMode::Bearer => AnthropicAuthMode::Bearer,
    };

    AnthropicTransportOptions {
        auth_mode,
        anthropic_version: request_compat.anthropic_version.clone(),
        allow_stream_fallback: request_compat.allow_stream_fallback,
    }
}

fn map_non_streaming_response(
    response: Response,
) -> Vec<Result<LanguageModelCompletionEvent, LanguageModelCompletionError>> {
    let mut events = Vec::new();
    events.push(Ok(LanguageModelCompletionEvent::StartMessage {
        message_id: response.id,
    }));

    for content in response.content {
        match content {
            ResponseContent::Text { text } => {
                events.push(Ok(LanguageModelCompletionEvent::Text(text)));
            }
            ResponseContent::Thinking { thinking } => {
                events.push(Ok(LanguageModelCompletionEvent::Thinking {
                    text: thinking,
                    signature: None,
                }));
            }
            ResponseContent::RedactedThinking { data } => {
                events.push(Ok(LanguageModelCompletionEvent::RedactedThinking { data }));
            }
            ResponseContent::ToolUse { id, name, input } => {
                events.push(Ok(LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
                    id: id.into(),
                    name: name.into(),
                    raw_input: input.to_string(),
                    input,
                    is_input_complete: true,
                    thought_signature: None,
                })));
            }
        }
    }

    events.push(Ok(LanguageModelCompletionEvent::UsageUpdate(convert_usage(
        &response.usage,
    ))));
    events.push(Ok(LanguageModelCompletionEvent::Stop(parse_stop_reason(
        response.stop_reason.as_deref(),
    ))));
    events
}

impl LanguageModel for AnthropicCompatibleLanguageModel {
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
        true
    }

    fn supports_images(&self) -> bool {
        true
    }

    fn supports_streaming_tools(&self) -> bool {
        true
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        matches!(
            choice,
            LanguageModelToolChoice::Auto
                | LanguageModelToolChoice::Any
                | LanguageModelToolChoice::None
        )
    }

    fn telemetry_id(&self) -> String {
        format!("anthropic-compatible/{}/{}", self.provider_id.0, self.model.id())
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_token_count()
    }

    fn max_output_tokens(&self) -> Option<u64> {
        Some(self.model.max_output_tokens())
    }

    fn supports_split_token_display(&self) -> bool {
        true
    }

    fn count_tokens(
        &self,
        request: LanguageModelRequest,
        cx: &App,
    ) -> BoxFuture<'static, Result<u64>> {
        let http_client = self.http_client.clone();
        let model_id = self.model.request_id().to_string();
        let mode = self.model.mode();
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
                return count_anthropic_tokens_with_tiktoken(request);
            };

            let count_request =
                into_anthropic_count_tokens_request(request.clone(), model_id, mode);
            let options = into_transport_options(&request_compat);

            match anthropic::count_tokens_with_options(
                http_client.as_ref(),
                &api_url,
                &api_key,
                count_request,
                &options,
            )
            .await
            {
                Ok(response) => Ok(response.input_tokens),
                Err(err) => {
                    if request_compat.allow_count_tokens_fallback {
                        log::warn!(
                            "anthropic-compatible count_tokens failed, fallback to tiktoken: {err:?}"
                        );
                        count_anthropic_tokens_with_tiktoken(request)
                    } else {
                        Err(map_anthropic_error(provider_name, err).into())
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
        let provider_name = self.provider_name.clone();
        let request_compat = self
            .state
            .read_with(cx, |state, _cx| state.settings.request_compat.clone());
        let options = into_transport_options(&request_compat);
        let beta_headers = self.model.beta_headers();

        let stream_request = into_anthropic(
            request.clone(),
            self.model.request_id().into(),
            self.model.default_temperature(),
            self.model.max_output_tokens(),
            self.model.mode(),
        );
        let fallback_request = into_anthropic(
            request,
            self.model.request_id().into(),
            self.model.default_temperature(),
            self.model.max_output_tokens(),
            self.model.mode(),
        );

        let http_client = self.http_client.clone();
        let (api_key, api_url) = self.state.read_with(cx, |state, _cx| {
            (
                state.api_key_state.key(&state.settings.api_url).map(|k| k.to_string()),
                state.settings.api_url.clone(),
            )
        });

        let future = self.request_limiter.stream_with_bypass(
            async move {
                let Some(api_key) = api_key else {
                    return Err(LanguageModelCompletionError::NoApiKey {
                        provider: provider_name.clone(),
                    });
                };

                match anthropic::stream_completion_with_options(
                    http_client.as_ref(),
                    &api_url,
                    &api_key,
                    stream_request,
                    beta_headers.clone(),
                    &options,
                )
                .await
                {
                    Ok(stream) => Ok(AnthropicEventMapper::new().map_stream(stream).boxed()),
                    Err(err) => {
                        if request_compat.allow_stream_fallback {
                            log::warn!(
                                "anthropic-compatible streaming failed, fallback to non-streaming: {err:?}"
                            );
                            let response = anthropic::non_streaming_completion_with_options(
                                http_client.as_ref(),
                                &api_url,
                                &api_key,
                                fallback_request,
                                beta_headers,
                                &options,
                            )
                            .await
                            .map_err(|e| map_anthropic_error(provider_name.clone(), e))?;

                            Ok(futures::stream::iter(map_non_streaming_response(response)).boxed())
                        } else {
                            Err(map_anthropic_error(provider_name.clone(), err))
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
        let api_key_editor = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                "000000000000000000000000000000000000000000000000000",
            )
        });

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

    fn should_render_editor(&self, cx: &Context<Self>) -> bool {
        !self.state.read(cx).is_authenticated()
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let env_var_set = state.api_key_state.is_from_env_var();
        let env_var_name = state.api_key_state.env_var_name();

        let api_key_section = if self.should_render_editor(cx) {
            v_flex()
                .on_action(cx.listener(Self::save_api_key))
                .child(Label::new("To use Zed's agent with an Anthropic-compatible provider, you need to add an API key."))
                .child(
                    div()
                        .pt(DynamicSpacing::Base04.rems(cx))
                        .child(self.api_key_editor.clone()),
                )
                .child(
                    Label::new(
                        format!(
                            "You can also set the {env_var_name} environment variable and restart Zed."
                        ),
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
                    h_flex()
                        .flex_shrink_0()
                        .child(
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
                        .child(ConfiguredApiCard::new("Anthropic API compatible endpoint"))
                        .child(
                            Label::new("Uses Anthropic Messages API-compatible endpoints.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(h_flex().child(ButtonLink::new(
                            "Messages API docs",
                            "https://docs.anthropic.com/en/api/messages",
                        ))),
                )
                .into_any()
        }
    }
}
