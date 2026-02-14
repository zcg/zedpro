use std::sync::Arc;

use anyhow::Result;
use collections::HashSet;
use fs::Fs;
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle, Task,
};
use language_model::{LanguageModelProviderId, LanguageModelRegistry};
use language_models::AllLanguageModelSettings;
use language_models::provider::{
    anthropic_compatible::AvailableModel as AnthropicCompatibleAvailableModel,
    google_compatible::AvailableModel as GoogleCompatibleAvailableModel,
    open_ai_compatible::{AvailableModel as OpenAiCompatibleAvailableModel, ModelCapabilities},
};
use settings::{
    AnthropicCompatibleSettingsContent, GoogleCompatibleSettingsContent,
    OpenAiCompatibleSettingsContent, Settings, update_settings_file,
};
use ui::{
    Banner, Checkbox, KeyBinding, Modal, ModalFooter, ModalHeader, Section, ToggleButtonGroup,
    ToggleButtonGroupStyle, ToggleButtonWithIcon, ToggleState, WithScrollbar, prelude::*,
};
use ui_input::InputField;
use workspace::{ModalView, Workspace};

fn single_line_input(
    label: impl Into<SharedString>,
    placeholder: &str,
    text: Option<&str>,
    tab_index: isize,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputField> {
    cx.new(|cx| {
        let input = InputField::new(window, cx, placeholder)
            .label(label)
            .tab_index(tab_index)
            .tab_stop(true);

        if let Some(text) = text {
            input.set_text(text, window, cx);
        }
        input
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LlmCompatibleProvider {
    OpenAi,
    Anthropic,
    Gemini,
}

impl LlmCompatibleProvider {
    fn name(&self) -> &'static str {
        match self {
            LlmCompatibleProvider::OpenAi => "OpenAI",
            LlmCompatibleProvider::Anthropic => "Anthropic",
            LlmCompatibleProvider::Gemini => "Gemini",
        }
    }

    fn api_url(&self) -> &'static str {
        match self {
            LlmCompatibleProvider::OpenAi => "https://api.openai.com/v1",
            LlmCompatibleProvider::Anthropic => "https://api.anthropic.com",
            LlmCompatibleProvider::Gemini => "https://generativelanguage.googleapis.com",
        }
    }

    fn id_suffix(&self) -> &'static str {
        match self {
            LlmCompatibleProvider::OpenAi => "openai",
            LlmCompatibleProvider::Anthropic => "anthropic",
            LlmCompatibleProvider::Gemini => "gemini",
        }
    }
}

struct AddLlmProviderInput {
    default_provider: LlmCompatibleProvider,
    provider_name: Entity<InputField>,
    api_url: Entity<InputField>,
    api_key: Entity<InputField>,
    models: Vec<ModelInput>,
}

impl AddLlmProviderInput {
    fn new(provider: LlmCompatibleProvider, window: &mut Window, cx: &mut App) -> Self {
        let provider_name =
            single_line_input("Provider Name", provider.name(), None, 1, window, cx);
        let api_url = single_line_input("API URL", provider.api_url(), None, 2, window, cx);
        let api_key = single_line_input(
            "API Key",
            "000000000000000000000000000000000000000000000000",
            None,
            3,
            window,
            cx,
        );

        Self {
            default_provider: provider,
            provider_name,
            api_url,
            api_key,
            models: vec![ModelInput::new(provider, 0, window, cx)],
        }
    }

    fn add_model(&mut self, window: &mut Window, cx: &mut App) {
        let model_index = self.models.len();
        self.models.push(ModelInput::new(
            self.default_provider,
            model_index,
            window,
            cx,
        ));
    }

    fn remove_model(&mut self, index: usize) {
        self.models.remove(index);
    }
}

struct ModelCapabilityToggles {
    pub supports_tools: ToggleState,
    pub supports_images: ToggleState,
    pub supports_parallel_tool_calls: ToggleState,
    pub supports_prompt_cache_key: ToggleState,
    pub supports_chat_completions: ToggleState,
}

struct ModelProtocolToggles {
    pub openai: ToggleState,
    pub anthropic: ToggleState,
    pub gemini: ToggleState,
}

struct ModelInput {
    name: Entity<InputField>,
    max_completion_tokens: Entity<InputField>,
    max_output_tokens: Entity<InputField>,
    max_tokens: Entity<InputField>,
    capabilities: ModelCapabilityToggles,
    protocols: ModelProtocolToggles,
}

impl ModelInput {
    fn new(
        provider: LlmCompatibleProvider,
        model_index: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let base_tab_index = (3 + (model_index * 4)) as isize;

        let model_name = single_line_input(
            "Model Name",
            "e.g. gpt-5, claude-opus-4, gemini-2.5-pro",
            None,
            base_tab_index + 1,
            window,
            cx,
        );
        let max_completion_tokens = single_line_input(
            "Max Completion Tokens",
            "200000",
            Some("200000"),
            base_tab_index + 2,
            window,
            cx,
        );
        let max_output_tokens = single_line_input(
            "Max Output Tokens",
            "Max Output Tokens",
            Some("32000"),
            base_tab_index + 3,
            window,
            cx,
        );
        let max_tokens = single_line_input(
            "Max Tokens",
            "Max Tokens",
            Some("200000"),
            base_tab_index + 4,
            window,
            cx,
        );

        let ModelCapabilities {
            tools,
            images,
            parallel_tool_calls,
            prompt_cache_key,
            chat_completions,
        } = ModelCapabilities::default();

        Self {
            name: model_name,
            max_completion_tokens,
            max_output_tokens,
            max_tokens,
            capabilities: ModelCapabilityToggles {
                supports_tools: tools.into(),
                supports_images: images.into(),
                supports_parallel_tool_calls: parallel_tool_calls.into(),
                supports_prompt_cache_key: prompt_cache_key.into(),
                supports_chat_completions: chat_completions.into(),
            },
            protocols: ModelProtocolToggles {
                openai: matches!(provider, LlmCompatibleProvider::OpenAi).into(),
                anthropic: matches!(provider, LlmCompatibleProvider::Anthropic).into(),
                gemini: matches!(provider, LlmCompatibleProvider::Gemini).into(),
            },
        }
    }

    fn parse_openai(&self, cx: &App) -> Result<OpenAiCompatibleAvailableModel, SharedString> {
        let name = self.name.read(cx).text(cx);
        if name.is_empty() {
            return Err(SharedString::from("Model Name cannot be empty"));
        }
        Ok(OpenAiCompatibleAvailableModel {
            name,
            display_name: None,
            max_completion_tokens: Some(
                self.max_completion_tokens
                    .read(cx)
                    .text(cx)
                    .parse::<u64>()
                    .map_err(|_| SharedString::from("Max Completion Tokens must be a number"))?,
            ),
            max_output_tokens: Some(
                self.max_output_tokens
                    .read(cx)
                    .text(cx)
                    .parse::<u64>()
                    .map_err(|_| SharedString::from("Max Output Tokens must be a number"))?,
            ),
            max_tokens: self
                .max_tokens
                .read(cx)
                .text(cx)
                .parse::<u64>()
                .map_err(|_| SharedString::from("Max Tokens must be a number"))?,
            capabilities: ModelCapabilities {
                tools: self.capabilities.supports_tools.selected(),
                images: self.capabilities.supports_images.selected(),
                parallel_tool_calls: self.capabilities.supports_parallel_tool_calls.selected(),
                prompt_cache_key: self.capabilities.supports_prompt_cache_key.selected(),
                chat_completions: self.capabilities.supports_chat_completions.selected(),
            },
        })
    }

    fn parse_anthropic(&self, cx: &App) -> Result<AnthropicCompatibleAvailableModel, SharedString> {
        let name = self.name.read(cx).text(cx);
        if name.is_empty() {
            return Err(SharedString::from("Model Name cannot be empty"));
        }

        Ok(AnthropicCompatibleAvailableModel {
            name,
            display_name: None,
            max_tokens: self
                .max_tokens
                .read(cx)
                .text(cx)
                .parse::<u64>()
                .map_err(|_| SharedString::from("Max Tokens must be a number"))?,
            tool_override: None,
            cache_configuration: None,
            max_output_tokens: Some(
                self.max_output_tokens
                    .read(cx)
                    .text(cx)
                    .parse::<u64>()
                    .map_err(|_| SharedString::from("Max Output Tokens must be a number"))?,
            ),
            default_temperature: None,
            extra_beta_headers: Vec::new(),
            mode: None,
        })
    }

    fn parse_gemini(&self, cx: &App) -> Result<GoogleCompatibleAvailableModel, SharedString> {
        let name = self.name.read(cx).text(cx);
        if name.is_empty() {
            return Err(SharedString::from("Model Name cannot be empty"));
        }

        Ok(GoogleCompatibleAvailableModel {
            name,
            display_name: None,
            max_tokens: self
                .max_tokens
                .read(cx)
                .text(cx)
                .parse::<u64>()
                .map_err(|_| SharedString::from("Max Tokens must be a number"))?,
            mode: None,
        })
    }

    fn supports_any_protocol(&self) -> bool {
        self.protocols.openai.selected()
            || self.protocols.anthropic.selected()
            || self.protocols.gemini.selected()
    }
}

fn select_protocol(model: &mut ModelInput, provider: LlmCompatibleProvider) {
    model.protocols.openai = matches!(provider, LlmCompatibleProvider::OpenAi).into();
    model.protocols.anthropic = matches!(provider, LlmCompatibleProvider::Anthropic).into();
    model.protocols.gemini = matches!(provider, LlmCompatibleProvider::Gemini).into();
}

#[derive(Clone)]
enum ProviderModalMode {
    Create,
    Edit {
        provider_id: Arc<str>,
        protocol: LlmCompatibleProvider,
    },
}

fn load_provider_input_for_edit(
    provider_id: Arc<str>,
    protocol: LlmCompatibleProvider,
    window: &mut Window,
    cx: &mut App,
) -> Result<AddLlmProviderInput, SharedString> {
    let mut input = AddLlmProviderInput::new(protocol, window, cx);

    let model_count = match protocol {
        LlmCompatibleProvider::OpenAi => {
            let provider = {
                let settings = AllLanguageModelSettings::get_global(cx);
                settings
                    .openai_compatible
                    .get(provider_id.as_ref())
                    .cloned()
            }
            .ok_or_else(|| SharedString::from("Provider settings not found"))?;

            input.provider_name.update(cx, |field, cx| {
                field.set_text(provider_id.as_ref(), window, cx)
            });
            input.api_url.update(cx, |field, cx| {
                field.set_text(&provider.api_url, window, cx)
            });

            input.models.clear();
            for (index, model) in provider.available_models.iter().enumerate() {
                input
                    .models
                    .push(model_input_from_openai_model(model, index, window, cx));
            }
            provider.available_models.len()
        }
        LlmCompatibleProvider::Anthropic => {
            let provider = {
                let settings = AllLanguageModelSettings::get_global(cx);
                settings
                    .anthropic_compatible
                    .get(provider_id.as_ref())
                    .cloned()
            }
            .ok_or_else(|| SharedString::from("Provider settings not found"))?;

            input.provider_name.update(cx, |field, cx| {
                field.set_text(provider_id.as_ref(), window, cx)
            });
            input.api_url.update(cx, |field, cx| {
                field.set_text(&provider.api_url, window, cx)
            });

            input.models.clear();
            for (index, model) in provider.available_models.iter().enumerate() {
                input
                    .models
                    .push(model_input_from_anthropic_model(model, index, window, cx));
            }
            provider.available_models.len()
        }
        LlmCompatibleProvider::Gemini => {
            let provider = {
                let settings = AllLanguageModelSettings::get_global(cx);
                settings
                    .google_compatible
                    .get(provider_id.as_ref())
                    .cloned()
            }
            .ok_or_else(|| SharedString::from("Provider settings not found"))?;

            input.provider_name.update(cx, |field, cx| {
                field.set_text(provider_id.as_ref(), window, cx)
            });
            input.api_url.update(cx, |field, cx| {
                field.set_text(&provider.api_url, window, cx)
            });

            input.models.clear();
            for (index, model) in provider.available_models.iter().enumerate() {
                input
                    .models
                    .push(model_input_from_gemini_model(model, index, window, cx));
            }
            provider.available_models.len()
        }
    };

    if model_count == 0 {
        input.models.push(ModelInput::new(protocol, 0, window, cx));
    }

    input
        .api_key
        .update(cx, |field, cx| field.set_text("", window, cx));

    Ok(input)
}

fn model_input_from_openai_model(
    model: &OpenAiCompatibleAvailableModel,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) -> ModelInput {
    let mut input = ModelInput::new(LlmCompatibleProvider::OpenAi, index, window, cx);
    input
        .name
        .update(cx, |field, cx| field.set_text(&model.name, window, cx));
    input.max_tokens.update(cx, |field, cx| {
        field.set_text(&model.max_tokens.to_string(), window, cx)
    });
    input.max_output_tokens.update(cx, |field, cx| {
        field.set_text(
            &model.max_output_tokens.unwrap_or(32000).to_string(),
            window,
            cx,
        )
    });
    input.max_completion_tokens.update(cx, |field, cx| {
        field.set_text(
            &model.max_completion_tokens.unwrap_or(200000).to_string(),
            window,
            cx,
        )
    });
    input.capabilities.supports_tools = model.capabilities.tools.into();
    input.capabilities.supports_images = model.capabilities.images.into();
    input.capabilities.supports_parallel_tool_calls = model.capabilities.parallel_tool_calls.into();
    input.capabilities.supports_prompt_cache_key = model.capabilities.prompt_cache_key.into();
    input.capabilities.supports_chat_completions = model.capabilities.chat_completions.into();
    select_protocol(&mut input, LlmCompatibleProvider::OpenAi);
    input
}

fn model_input_from_anthropic_model(
    model: &AnthropicCompatibleAvailableModel,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) -> ModelInput {
    let mut input = ModelInput::new(LlmCompatibleProvider::Anthropic, index, window, cx);
    input
        .name
        .update(cx, |field, cx| field.set_text(&model.name, window, cx));
    input.max_tokens.update(cx, |field, cx| {
        field.set_text(&model.max_tokens.to_string(), window, cx)
    });
    input.max_output_tokens.update(cx, |field, cx| {
        field.set_text(
            &model.max_output_tokens.unwrap_or(32000).to_string(),
            window,
            cx,
        )
    });
    select_protocol(&mut input, LlmCompatibleProvider::Anthropic);
    input
}

fn model_input_from_gemini_model(
    model: &GoogleCompatibleAvailableModel,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) -> ModelInput {
    let mut input = ModelInput::new(LlmCompatibleProvider::Gemini, index, window, cx);
    input
        .name
        .update(cx, |field, cx| field.set_text(&model.name, window, cx));
    input.max_tokens.update(cx, |field, cx| {
        field.set_text(&model.max_tokens.to_string(), window, cx)
    });
    select_protocol(&mut input, LlmCompatibleProvider::Gemini);
    input
}

fn save_provider_to_settings(
    input: &AddLlmProviderInput,
    mode: ProviderModalMode,
    cx: &mut App,
) -> Task<Result<(), SharedString>> {
    let provider_base_name = input.provider_name.read(cx).text(cx);
    if provider_base_name.is_empty() {
        return Task::ready(Err("Provider Name cannot be empty".into()));
    }
    let provider_base_name: Arc<str> = provider_base_name.into();

    let api_url = input.api_url.read(cx).text(cx);
    if api_url.is_empty() {
        return Task::ready(Err("API URL cannot be empty".into()));
    }

    let api_key = input.api_key.read(cx).text(cx);
    let should_update_api_key = !api_key.is_empty();
    if matches!(mode, ProviderModalMode::Create) && api_key.is_empty() {
        return Task::ready(Err("API Key cannot be empty".into()));
    }

    let mut openai_model_names: HashSet<String> = HashSet::default();
    let mut anthropic_model_names: HashSet<String> = HashSet::default();
    let mut gemini_model_names: HashSet<String> = HashSet::default();
    let mut openai_models = Vec::new();
    let mut anthropic_models = Vec::new();
    let mut gemini_models = Vec::new();

    for model in &input.models {
        if !model.supports_any_protocol() {
            return Task::ready(Err(
                "Each model must select at least one compatibility type".into(),
            ));
        }

        if model.protocols.openai.selected() {
            match model.parse_openai(cx) {
                Ok(model) => {
                    if !openai_model_names.insert(model.name.clone()) {
                        return Task::ready(Err("OpenAI Model Names must be unique".into()));
                    }
                    openai_models.push(model);
                }
                Err(err) => return Task::ready(Err(err)),
            }
        }

        if model.protocols.anthropic.selected() {
            match model.parse_anthropic(cx) {
                Ok(model) => {
                    if !anthropic_model_names.insert(model.name.clone()) {
                        return Task::ready(Err("Anthropic Model Names must be unique".into()));
                    }
                    anthropic_models.push(model);
                }
                Err(err) => return Task::ready(Err(err)),
            }
        }

        if model.protocols.gemini.selected() {
            match model.parse_gemini(cx) {
                Ok(model) => {
                    if !gemini_model_names.insert(model.name.clone()) {
                        return Task::ready(Err("Gemini Model Names must be unique".into()));
                    }
                    gemini_models.push(model);
                }
                Err(err) => return Task::ready(Err(err)),
            }
        }
    }

    if openai_models.is_empty() && anthropic_models.is_empty() && gemini_models.is_empty() {
        return Task::ready(Err("At least one model is required".into()));
    }

    let (openai_provider_id, anthropic_provider_id, gemini_provider_id) = match &mode {
        ProviderModalMode::Create => {
            let primary_provider = if !openai_models.is_empty()
                && anthropic_models.is_empty()
                && gemini_models.is_empty()
            {
                LlmCompatibleProvider::OpenAi
            } else if openai_models.is_empty()
                && !anthropic_models.is_empty()
                && gemini_models.is_empty()
            {
                LlmCompatibleProvider::Anthropic
            } else if openai_models.is_empty()
                && anthropic_models.is_empty()
                && !gemini_models.is_empty()
            {
                LlmCompatibleProvider::Gemini
            } else if !openai_models.is_empty() {
                LlmCompatibleProvider::OpenAi
            } else if !anthropic_models.is_empty() {
                LlmCompatibleProvider::Anthropic
            } else {
                LlmCompatibleProvider::Gemini
            };

            let openai_provider_id = if openai_models.is_empty() {
                None
            } else {
                Some(provider_id_for_protocol(
                    &provider_base_name,
                    primary_provider,
                    LlmCompatibleProvider::OpenAi,
                ))
            };
            let anthropic_provider_id = if anthropic_models.is_empty() {
                None
            } else {
                Some(provider_id_for_protocol(
                    &provider_base_name,
                    primary_provider,
                    LlmCompatibleProvider::Anthropic,
                ))
            };
            let gemini_provider_id = if gemini_models.is_empty() {
                None
            } else {
                Some(provider_id_for_protocol(
                    &provider_base_name,
                    primary_provider,
                    LlmCompatibleProvider::Gemini,
                ))
            };
            (
                openai_provider_id,
                anthropic_provider_id,
                gemini_provider_id,
            )
        }
        ProviderModalMode::Edit {
            provider_id,
            protocol,
        } => {
            let openai_provider_id = match protocol {
                LlmCompatibleProvider::OpenAi => Some(provider_id.clone()),
                _ => None,
            };
            let anthropic_provider_id = match protocol {
                LlmCompatibleProvider::Anthropic => Some(provider_id.clone()),
                _ => None,
            };
            let gemini_provider_id = match protocol {
                LlmCompatibleProvider::Gemini => Some(provider_id.clone()),
                _ => None,
            };
            (
                openai_provider_id,
                anthropic_provider_id,
                gemini_provider_id,
            )
        }
    };

    let provider_ids = [
        openai_provider_id.as_ref(),
        anthropic_provider_id.as_ref(),
        gemini_provider_id.as_ref(),
    ];

    if matches!(mode, ProviderModalMode::Create)
        && LanguageModelRegistry::read_global(cx)
            .providers()
            .iter()
            .any(|provider| {
                provider_ids.iter().flatten().any(|provider_id| {
                    provider.id().0.as_ref() == provider_id.as_ref()
                        || provider.name().0.as_ref() == provider_id.as_ref()
                })
            })
    {
        return Task::ready(Err(
            "Provider Name (or one of its compatibility aliases) is already taken".into(),
        ));
    }

    let fs = <dyn Fs>::global(cx);
    let maybe_task = if should_update_api_key {
        Some(cx.write_credentials(&api_url, "Bearer", api_key.as_bytes()))
    } else {
        None
    };
    cx.spawn(async move |cx| {
        if let Some(task) = maybe_task {
            task.await
                .map_err(|_| SharedString::from("Failed to write API key to keychain"))?;
        }
        cx.update(|cx| {
            update_settings_file(fs, cx, move |settings, _cx| {
                let language_models = settings.language_models.get_or_insert_default();
                if let Some(provider_id) = &openai_provider_id {
                    language_models
                        .openai_compatible
                        .get_or_insert_default()
                        .insert(
                            provider_id.clone(),
                            OpenAiCompatibleSettingsContent {
                                api_url: api_url.clone(),
                                available_models: openai_models.clone(),
                            },
                        );
                }

                if let Some(provider_id) = &anthropic_provider_id {
                    let request_compat = if matches!(mode, ProviderModalMode::Edit { .. }) {
                        language_models
                            .anthropic_compatible
                            .as_ref()
                            .and_then(|providers| providers.get(provider_id))
                            .and_then(|provider| provider.request_compat.clone())
                    } else {
                        None
                    };
                    language_models
                        .anthropic_compatible
                        .get_or_insert_default()
                        .insert(
                            provider_id.clone(),
                            AnthropicCompatibleSettingsContent {
                                api_url: api_url.clone(),
                                available_models: anthropic_models.clone(),
                                request_compat,
                            },
                        );
                }

                if let Some(provider_id) = &gemini_provider_id {
                    let request_compat = if matches!(mode, ProviderModalMode::Edit { .. }) {
                        language_models
                            .google_compatible
                            .as_ref()
                            .and_then(|providers| providers.get(provider_id))
                            .and_then(|provider| provider.request_compat.clone())
                    } else {
                        None
                    };
                    language_models
                        .google_compatible
                        .get_or_insert_default()
                        .insert(
                            provider_id.clone(),
                            GoogleCompatibleSettingsContent {
                                api_url: api_url.clone(),
                                available_models: gemini_models.clone(),
                                request_compat,
                            },
                        );
                }
            });
        });
        Ok(())
    })
}

fn provider_id_for_protocol(
    base_name: &Arc<str>,
    primary_provider: LlmCompatibleProvider,
    protocol: LlmCompatibleProvider,
) -> Arc<str> {
    if primary_provider == protocol {
        base_name.clone()
    } else {
        format!("{}-{}", base_name.as_ref(), protocol.id_suffix()).into()
    }
}

pub struct AddLlmProviderModal {
    input: AddLlmProviderInput,
    mode: ProviderModalMode,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
    last_error: Option<SharedString>,
    show_api_key_input: bool,
}

impl AddLlmProviderModal {
    fn provider_has_api_key(provider_id: &Arc<str>, cx: &App) -> bool {
        let provider_id = LanguageModelProviderId::from(provider_id.as_ref().to_string());
        LanguageModelRegistry::read_global(cx)
            .provider(&provider_id)
            .map(|provider| provider.is_authenticated(cx))
            .unwrap_or(false)
    }

    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new_create(LlmCompatibleProvider::OpenAi, window, cx)
        });
    }

    pub fn toggle_edit(
        provider_id: Arc<str>,
        protocol: LlmCompatibleProvider,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new_edit(provider_id.clone(), protocol, window, cx)
        });
    }

    fn new_create(
        provider: LlmCompatibleProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            input: AddLlmProviderInput::new(provider, window, cx),
            mode: ProviderModalMode::Create,
            last_error: None,
            show_api_key_input: true,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn new_edit(
        provider_id: Arc<str>,
        protocol: LlmCompatibleProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let show_api_key_input = !Self::provider_has_api_key(&provider_id, cx);
        match load_provider_input_for_edit(provider_id.clone(), protocol, window, cx) {
            Ok(input) => Self {
                input,
                mode: ProviderModalMode::Edit {
                    provider_id,
                    protocol,
                },
                last_error: None,
                show_api_key_input,
                focus_handle: cx.focus_handle(),
                scroll_handle: ScrollHandle::new(),
            },
            Err(error) => Self {
                input: AddLlmProviderInput::new(protocol, window, cx),
                mode: ProviderModalMode::Edit {
                    provider_id,
                    protocol,
                },
                last_error: Some(error),
                show_api_key_input,
                focus_handle: cx.focus_handle(),
                scroll_handle: ScrollHandle::new(),
            },
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let task = save_provider_to_settings(&self.input, self.mode.clone(), cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.last_error = Some(error);
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_model_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .mt_1()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("Models").size(LabelSize::Small))
                    .child(
                        Button::new("add-model", "Add Model")
                            .icon(IconName::Plus)
                            .icon_position(IconPosition::Start)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.input.add_model(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .children(
                self.input
                    .models
                    .iter()
                    .enumerate()
                    .map(|(ix, _)| self.render_model(ix, cx)),
            )
    }

    fn render_model(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let has_more_than_one_model = self.input.models.len() > 1;
        let model = &self.input.models[ix];
        let is_edit_mode = matches!(self.mode, ProviderModalMode::Edit { .. });
        let uses_openai = model.protocols.openai.selected();
        let uses_anthropic = model.protocols.anthropic.selected();
        let uses_gemini = model.protocols.gemini.selected();
        let protocol_label = if uses_openai {
            "OpenAI"
        } else if uses_anthropic {
            "Anthropic"
        } else {
            "Gemini"
        };

        v_flex()
            .p_2()
            .gap_2()
            .rounded_sm()
            .border_1()
            .border_dashed()
            .border_color(cx.theme().colors().border.opacity(0.6))
            .bg(cx.theme().colors().element_active.opacity(0.15))
            .child(model.name.clone())
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new("Model Compatibility").size(LabelSize::Small))
                    .when(is_edit_mode, |this| {
                        this.child(
                            Label::new(protocol_label)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(!is_edit_mode, |this| {
                        this.child(
                            ToggleButtonGroup::single_row(
                                format!("model-protocol-{ix}"),
                                [
                                    ToggleButtonWithIcon::new(
                                        "OpenAI",
                                        IconName::AiOpenAi,
                                        cx.listener(move |this, _event, _window, cx| {
                                            select_protocol(
                                                &mut this.input.models[ix],
                                                LlmCompatibleProvider::OpenAi,
                                            );
                                            cx.notify();
                                        }),
                                    ),
                                    ToggleButtonWithIcon::new(
                                        "Anthropic",
                                        IconName::AiAnthropic,
                                        cx.listener(move |this, _event, _window, cx| {
                                            select_protocol(
                                                &mut this.input.models[ix],
                                                LlmCompatibleProvider::Anthropic,
                                            );
                                            cx.notify();
                                        }),
                                    ),
                                    ToggleButtonWithIcon::new(
                                        "Gemini",
                                        IconName::AiGoogle,
                                        cx.listener(move |this, _event, _window, cx| {
                                            select_protocol(
                                                &mut this.input.models[ix],
                                                LlmCompatibleProvider::Gemini,
                                            );
                                            cx.notify();
                                        }),
                                    ),
                                ],
                            )
                            .style(ToggleButtonGroupStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .auto_width()
                            .selected_index(if uses_openai {
                                0
                            } else if uses_anthropic {
                                1
                            } else {
                                2
                            }),
                        )
                    }),
            )
            .when(!uses_openai && !uses_anthropic && !uses_gemini, |this| {
                this.child(
                    Label::new("Select at least one compatibility type for this model.")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when(uses_openai, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(model.max_completion_tokens.clone())
                        .child(model.max_output_tokens.clone()),
                )
                .child(model.max_tokens.clone())
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new(
                                ("supports-tools", ix),
                                model.capabilities.supports_tools,
                            )
                            .label("Supports tools")
                            .on_click(cx.listener(
                                move |this, checked, _window, cx| {
                                    this.input.models[ix].capabilities.supports_tools = *checked;
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            Checkbox::new(
                                ("supports-images", ix),
                                model.capabilities.supports_images,
                            )
                            .label("Supports images")
                            .on_click(cx.listener(
                                move |this, checked, _window, cx| {
                                    this.input.models[ix].capabilities.supports_images = *checked;
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            Checkbox::new(
                                ("supports-parallel-tool-calls", ix),
                                model.capabilities.supports_parallel_tool_calls,
                            )
                            .label("Supports parallel_tool_calls")
                            .on_click(cx.listener(
                                move |this, checked, _window, cx| {
                                    this.input.models[ix]
                                        .capabilities
                                        .supports_parallel_tool_calls = *checked;
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            Checkbox::new(
                                ("supports-prompt-cache-key", ix),
                                model.capabilities.supports_prompt_cache_key,
                            )
                            .label("Supports prompt_cache_key")
                            .on_click(cx.listener(
                                move |this, checked, _window, cx| {
                                    this.input.models[ix].capabilities.supports_prompt_cache_key =
                                        *checked;
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            Checkbox::new(
                                ("supports-chat-completions", ix),
                                model.capabilities.supports_chat_completions,
                            )
                            .label("Supports /chat/completions")
                            .on_click(cx.listener(
                                move |this, checked, _window, cx| {
                                    this.input.models[ix].capabilities.supports_chat_completions =
                                        *checked;
                                    cx.notify();
                                },
                            )),
                        ),
                )
            })
            .when(!uses_openai && uses_anthropic, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(model.max_output_tokens.clone())
                        .child(model.max_tokens.clone()),
                )
            })
            .when(!uses_openai && !uses_anthropic && uses_gemini, |this| {
                this.child(model.max_tokens.clone())
            })
            .when(has_more_than_one_model, |this| {
                this.child(
                    Button::new(("remove-model", ix), "Remove Model")
                        .icon(IconName::Trash)
                        .icon_position(IconPosition::Start)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Outlined)
                        .full_width()
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.input.remove_model(ix);
                            cx.notify();
                        })),
                )
            })
    }

    fn on_tab(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn on_tab_prev(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }
}

impl EventEmitter<DismissEvent> for AddLlmProviderModal {}

impl Focusable for AddLlmProviderModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for AddLlmProviderModal {}

impl Render for AddLlmProviderModal {
    fn render(&mut self, window: &mut ui::Window, cx: &mut ui::Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);

        let window_size = window.viewport_size();
        let rem_size = window.rem_size();
        let is_large_window = window_size.height / rem_size > rems_from_px(600.).0;

        let modal_max_height = if is_large_window {
            rems_from_px(620.)
        } else {
            rems_from_px(360.)
        };

        v_flex()
            .id("add-llm-provider-modal")
            .key_context("AddLlmProviderModal")
            .w(rems(44.))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_tab_prev))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .child(
                Modal::new("configure-context-server", None)
                    .header(
                        ModalHeader::new()
                            .headline(match self.mode {
                                ProviderModalMode::Create => "Add LLM Provider",
                                ProviderModalMode::Edit { .. } => "Edit LLM Provider",
                            })
                            .description(match self.mode {
                                ProviderModalMode::Create => {
                                    "Create one provider and choose compatibility type per model (OpenAI / Anthropic / Gemini)."
                                }
                                ProviderModalMode::Edit { .. } => {
                                    if self.show_api_key_input {
                                        "Edit models and parameters for this provider."
                                    } else {
                                        "Edit models and parameters for this provider. API key is already configured."
                                    }
                                }
                            }),
                    )
                    .when_some(self.last_error.clone(), |this, error| {
                        this.section(
                            Section::new().child(
                                Banner::new()
                                    .severity(Severity::Warning)
                                    .child(div().text_xs().child(error)),
                            ),
                        )
                    })
                    .child(
                        div()
                            .size_full()
                            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                            .child(
                                v_flex()
                                    .id("modal_content")
                                    .size_full()
                                    .tab_group()
                                    .max_h(modal_max_height)
                                    .pl_3()
                                    .pr_4()
                                    .pb_2()
                                    .gap_2()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll_handle)
                                    .child(self.input.provider_name.clone())
                                    .child(self.input.api_url.clone())
                                    .when(self.show_api_key_input, |this| {
                                        this.child(self.input.api_key.clone())
                                    })
                                    .when(!self.show_api_key_input, |this| {
                                        this.child(
                                            Label::new(
                                                "API Key already configured. Use 'Reset API Key' to re-enter credentials.",
                                            )
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                        )
                                    })
                                    .child(self.render_model_section(cx)),
                            ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("cancel", "Cancel")
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Cancel,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.))),
                                        )
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("save-server", "Save Provider")
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Confirm,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.))),
                                        )
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.confirm(&menu::Confirm, window, cx)
                                        })),
                                ),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use language_model::{
        LanguageModelProviderId, LanguageModelProviderName,
        fake_provider::FakeLanguageModelProvider,
    };
    use project::Project;
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn test_save_provider_invalid_inputs(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        assert_eq!(
            save_provider_validation_errors("", "someurl", "somekey", vec![], cx,).await,
            Some("Provider Name cannot be empty".into())
        );

        assert_eq!(
            save_provider_validation_errors("someprovider", "", "somekey", vec![], cx,).await,
            Some("API URL cannot be empty".into())
        );

        assert_eq!(
            save_provider_validation_errors("someprovider", "someurl", "", vec![], cx,).await,
            Some("API Key cannot be empty".into())
        );

        assert_eq!(
            save_provider_validation_errors(
                "someprovider",
                "someurl",
                "somekey",
                vec![("", "200000", "200000", "32000")],
                cx,
            )
            .await,
            Some("Model Name cannot be empty".into())
        );

        assert_eq!(
            save_provider_validation_errors(
                "someprovider",
                "someurl",
                "somekey",
                vec![("somemodel", "abc", "200000", "32000")],
                cx,
            )
            .await,
            Some("Max Tokens must be a number".into())
        );

        assert_eq!(
            save_provider_validation_errors(
                "someprovider",
                "someurl",
                "somekey",
                vec![("somemodel", "200000", "abc", "32000")],
                cx,
            )
            .await,
            Some("Max Completion Tokens must be a number".into())
        );

        assert_eq!(
            save_provider_validation_errors(
                "someprovider",
                "someurl",
                "somekey",
                vec![("somemodel", "200000", "200000", "abc")],
                cx,
            )
            .await,
            Some("Max Output Tokens must be a number".into())
        );

        assert_eq!(
            save_provider_validation_errors(
                "someprovider",
                "someurl",
                "somekey",
                vec![
                    ("somemodel", "200000", "200000", "32000"),
                    ("somemodel", "200000", "200000", "32000"),
                ],
                cx,
            )
            .await,
            Some("Model Names must be unique".into())
        );
    }

    #[gpui::test]
    async fn test_save_provider_name_conflict(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        cx.update(|_window, cx| {
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(
                    Arc::new(FakeLanguageModelProvider::new(
                        LanguageModelProviderId::new("someprovider"),
                        LanguageModelProviderName::new("Some Provider"),
                    )),
                    cx,
                );
            });
        });

        assert_eq!(
            save_provider_validation_errors(
                "someprovider",
                "someurl",
                "someapikey",
                vec![("somemodel", "200000", "200000", "32000")],
                cx,
            )
            .await,
            Some("Provider Name (or one of its compatibility aliases) is already taken".into())
        );
    }

    #[gpui::test]
    async fn test_model_input_default_capabilities(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        cx.update(|window, cx| {
            let model_input = ModelInput::new(LlmCompatibleProvider::OpenAi, 0, window, cx);
            model_input.name.update(cx, |input, cx| {
                input.set_text("somemodel", window, cx);
            });
            assert_eq!(
                model_input.capabilities.supports_tools,
                ToggleState::Selected
            );
            assert_eq!(
                model_input.capabilities.supports_images,
                ToggleState::Unselected
            );
            assert_eq!(
                model_input.capabilities.supports_parallel_tool_calls,
                ToggleState::Unselected
            );
            assert_eq!(
                model_input.capabilities.supports_prompt_cache_key,
                ToggleState::Unselected
            );
            assert_eq!(
                model_input.capabilities.supports_chat_completions,
                ToggleState::Selected
            );

            let parsed_model = model_input.parse_openai(cx).unwrap();
            assert!(parsed_model.capabilities.tools);
            assert!(!parsed_model.capabilities.images);
            assert!(!parsed_model.capabilities.parallel_tool_calls);
            assert!(!parsed_model.capabilities.prompt_cache_key);
            assert!(parsed_model.capabilities.chat_completions);
        });
    }

    #[gpui::test]
    async fn test_model_input_deselected_capabilities(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        cx.update(|window, cx| {
            let mut model_input = ModelInput::new(LlmCompatibleProvider::OpenAi, 0, window, cx);
            model_input.name.update(cx, |input, cx| {
                input.set_text("somemodel", window, cx);
            });

            model_input.capabilities.supports_tools = ToggleState::Unselected;
            model_input.capabilities.supports_images = ToggleState::Unselected;
            model_input.capabilities.supports_parallel_tool_calls = ToggleState::Unselected;
            model_input.capabilities.supports_prompt_cache_key = ToggleState::Unselected;
            model_input.capabilities.supports_chat_completions = ToggleState::Unselected;

            let parsed_model = model_input.parse_openai(cx).unwrap();
            assert!(!parsed_model.capabilities.tools);
            assert!(!parsed_model.capabilities.images);
            assert!(!parsed_model.capabilities.parallel_tool_calls);
            assert!(!parsed_model.capabilities.prompt_cache_key);
            assert!(!parsed_model.capabilities.chat_completions);
        });
    }

    #[gpui::test]
    async fn test_model_input_with_name_and_capabilities(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        cx.update(|window, cx| {
            let mut model_input = ModelInput::new(LlmCompatibleProvider::OpenAi, 0, window, cx);
            model_input.name.update(cx, |input, cx| {
                input.set_text("somemodel", window, cx);
            });

            model_input.capabilities.supports_tools = ToggleState::Selected;
            model_input.capabilities.supports_images = ToggleState::Unselected;
            model_input.capabilities.supports_parallel_tool_calls = ToggleState::Selected;
            model_input.capabilities.supports_prompt_cache_key = ToggleState::Unselected;
            model_input.capabilities.supports_chat_completions = ToggleState::Selected;

            let parsed_model = model_input.parse_openai(cx).unwrap();
            assert_eq!(parsed_model.name, "somemodel");
            assert!(parsed_model.capabilities.tools);
            assert!(!parsed_model.capabilities.images);
            assert!(parsed_model.capabilities.parallel_tool_calls);
            assert!(!parsed_model.capabilities.prompt_cache_key);
            assert!(parsed_model.capabilities.chat_completions);
        });
    }

    #[gpui::test]
    async fn test_model_input_parse_anthropic(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        cx.update(|window, cx| {
            let model_input = ModelInput::new(LlmCompatibleProvider::Anthropic, 0, window, cx);
            model_input.name.update(cx, |input, cx| {
                input.set_text("claude-opus-4-5-20251101", window, cx);
            });
            model_input.max_tokens.update(cx, |input, cx| {
                input.set_text("200000", window, cx);
            });
            model_input.max_output_tokens.update(cx, |input, cx| {
                input.set_text("32000", window, cx);
            });

            let parsed_model = model_input.parse_anthropic(cx).unwrap();
            assert_eq!(parsed_model.name, "claude-opus-4-5-20251101");
            assert_eq!(parsed_model.max_tokens, 200000);
            assert_eq!(parsed_model.max_output_tokens, Some(32000));
            assert!(parsed_model.tool_override.is_none());
            assert!(parsed_model.cache_configuration.is_none());
        });
    }

    #[gpui::test]
    async fn test_save_anthropic_provider_ignores_openai_only_fields(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        assert_eq!(
            save_provider_validation_errors_for(
                LlmCompatibleProvider::Anthropic,
                "someprovider",
                "https://api.anthropic.com",
                "somekey",
                vec![("somemodel", "200000", "not-a-number", "32000")],
                cx,
            )
            .await,
            None
        );
    }

    #[gpui::test]
    async fn test_model_input_parse_gemini(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        cx.update(|window, cx| {
            let model_input = ModelInput::new(LlmCompatibleProvider::Gemini, 0, window, cx);
            model_input.name.update(cx, |input, cx| {
                input.set_text("gemini-2.5-pro", window, cx);
            });
            model_input.max_tokens.update(cx, |input, cx| {
                input.set_text("1048576", window, cx);
            });

            let parsed_model = model_input.parse_gemini(cx).unwrap();
            assert_eq!(parsed_model.name, "gemini-2.5-pro");
            assert_eq!(parsed_model.max_tokens, 1_048_576);
            assert!(parsed_model.mode.is_none());
        });
    }

    #[gpui::test]
    async fn test_save_gemini_provider_ignores_openai_only_fields(cx: &mut TestAppContext) {
        let cx = setup_test(cx).await;

        assert_eq!(
            save_provider_validation_errors_for(
                LlmCompatibleProvider::Gemini,
                "gemini_provider",
                "https://example.com",
                "somekey",
                vec![("gemini-2.5-pro", "1048576", "not-a-number", "32000")],
                cx,
            )
            .await,
            None
        );
    }

    async fn setup_test(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme::init(theme::LoadThemes::JustBase, cx);

            language_model::init_settings(cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let _workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx
    }

    async fn save_provider_validation_errors(
        provider_name: &str,
        api_url: &str,
        api_key: &str,
        models: Vec<(&str, &str, &str, &str)>,
        cx: &mut VisualTestContext,
    ) -> Option<SharedString> {
        save_provider_validation_errors_for(
            LlmCompatibleProvider::OpenAi,
            provider_name,
            api_url,
            api_key,
            models,
            cx,
        )
        .await
    }

    async fn save_provider_validation_errors_for(
        provider: LlmCompatibleProvider,
        provider_name: &str,
        api_url: &str,
        api_key: &str,
        models: Vec<(&str, &str, &str, &str)>,
        cx: &mut VisualTestContext,
    ) -> Option<SharedString> {
        fn set_text(input: &Entity<InputField>, text: &str, window: &mut Window, cx: &mut App) {
            input.update(cx, |input, cx| {
                input.set_text(text, window, cx);
            });
        }

        let task = cx.update(|window, cx| {
            let mut input = AddLlmProviderInput::new(provider, window, cx);
            set_text(&input.provider_name, provider_name, window, cx);
            set_text(&input.api_url, api_url, window, cx);
            set_text(&input.api_key, api_key, window, cx);

            for (i, (name, max_tokens, max_completion_tokens, max_output_tokens)) in
                models.iter().enumerate()
            {
                if i >= input.models.len() {
                    input.models.push(ModelInput::new(provider, i, window, cx));
                }
                let model = &mut input.models[i];
                set_text(&model.name, name, window, cx);
                set_text(&model.max_tokens, max_tokens, window, cx);
                set_text(
                    &model.max_completion_tokens,
                    max_completion_tokens,
                    window,
                    cx,
                );
                set_text(&model.max_output_tokens, max_output_tokens, window, cx);
            }
            save_provider_to_settings(&input, ProviderModalMode::Create, cx)
        });

        task.await.err()
    }
}
