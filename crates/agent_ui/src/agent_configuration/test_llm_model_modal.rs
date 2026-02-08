use std::{sync::Arc, time::Duration};

use futures::StreamExt as _;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FutureExt as _, Task};
use language_model::{
    LanguageModel, LanguageModelCompletionError, LanguageModelProviderId, LanguageModelRegistry,
    LanguageModelRequest, LanguageModelRequestMessage, MessageContent, Role,
};
use ui::{
    Banner, KeyBinding, Modal, ModalFooter, ModalHeader, Section, Switch, TintColor, ToggleState,
    prelude::*,
};
use ui_input::InputField;
use workspace::{ModalView, Workspace};

use crate::agent_configuration::add_llm_provider_modal::LlmCompatibleProvider;

const DEFAULT_TEST_PROMPT: &str = "Who are you?";
const DEFAULT_TIMEOUT_SECONDS: u64 = 45;
const DEFAULT_DEGRADATION_THRESHOLD_MS: u64 = 6000;
const DEFAULT_MAX_RETRIES: u32 = 2;

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

#[derive(Clone)]
struct ModelTestConfig {
    model_name: String,
    prompt: String,
    timeout_seconds: u64,
    degradation_threshold_ms: u64,
    max_retries: u32,
}

enum ModelTestStatus {
    Idle,
    Testing,
    Available { latency_ms: u128 },
    Unavailable { reason: SharedString },
}

pub struct ModelAvailabilityTestModal {
    provider_id: Arc<str>,
    protocol: LlmCompatibleProvider,
    model_name: Entity<InputField>,
    prompt: Entity<InputField>,
    timeout_seconds: Entity<InputField>,
    degradation_threshold_ms: Entity<InputField>,
    max_retries: Entity<InputField>,
    use_custom_config: ToggleState,
    status: ModelTestStatus,
    last_error: Option<SharedString>,
    focus_handle: FocusHandle,
    _run_test_task: Task<()>,
}

impl ModelAvailabilityTestModal {
    pub fn toggle(
        provider_id: Arc<str>,
        model_name: SharedString,
        protocol: LlmCompatibleProvider,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(provider_id.clone(), model_name.clone(), protocol, window, cx)
        });
    }

    fn new(
        provider_id: Arc<str>,
        model_name: SharedString,
        protocol: LlmCompatibleProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            provider_id,
            protocol,
            model_name: single_line_input(
                "Test Model",
                "model-name",
                Some(model_name.as_ref()),
                1,
                window,
                cx,
            ),
            prompt: single_line_input(
                "Test Prompt",
                DEFAULT_TEST_PROMPT,
                Some(DEFAULT_TEST_PROMPT),
                2,
                window,
                cx,
            ),
            timeout_seconds: single_line_input(
                "Timeout (sec)",
                "45",
                Some(&DEFAULT_TIMEOUT_SECONDS.to_string()),
                3,
                window,
                cx,
            ),
            degradation_threshold_ms: single_line_input(
                "Degradation Threshold (ms)",
                "6000",
                Some(&DEFAULT_DEGRADATION_THRESHOLD_MS.to_string()),
                4,
                window,
                cx,
            ),
            max_retries: single_line_input(
                "Max Retries",
                "2",
                Some(&DEFAULT_MAX_RETRIES.to_string()),
                5,
                window,
                cx,
            ),
            use_custom_config: ToggleState::Selected,
            status: ModelTestStatus::Idle,
            last_error: None,
            focus_handle: cx.focus_handle(),
            _run_test_task: Task::ready(()),
        };

        this.run_test(&menu::Confirm, window, cx);
        this
    }

    fn parse_u64_field(
        field: &Entity<InputField>,
        field_name: &'static str,
        cx: &App,
    ) -> Result<u64, SharedString> {
        field
            .read(cx)
            .text(cx)
            .trim()
            .parse::<u64>()
            .map_err(|_| SharedString::from(format!("{field_name} must be a number")))
    }

    fn parse_config(&self, cx: &App) -> Result<ModelTestConfig, SharedString> {
        let model_name = self.model_name.read(cx).text(cx);
        if model_name.trim().is_empty() {
            return Err("Test Model cannot be empty".into());
        }

        let prompt = self.prompt.read(cx).text(cx);
        if prompt.trim().is_empty() {
            return Err("Test Prompt cannot be empty".into());
        }

        if matches!(self.use_custom_config, ToggleState::Selected) {
            let timeout_seconds =
                Self::parse_u64_field(&self.timeout_seconds, "Timeout (sec)", cx)?;
            let degradation_threshold_ms = Self::parse_u64_field(
                &self.degradation_threshold_ms,
                "Degradation Threshold (ms)",
                cx,
            )?;
            let max_retries = Self::parse_u64_field(&self.max_retries, "Max Retries", cx)? as u32;

            Ok(ModelTestConfig {
                model_name,
                prompt,
                timeout_seconds,
                degradation_threshold_ms,
                max_retries,
            })
        } else {
            Ok(ModelTestConfig {
                model_name,
                prompt,
                timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
                degradation_threshold_ms: DEFAULT_DEGRADATION_THRESHOLD_MS,
                max_retries: DEFAULT_MAX_RETRIES,
            })
        }
    }

    fn run_test(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.last_error = None;

        let config = match self.parse_config(cx) {
            Ok(config) => config,
            Err(error) => {
                self.last_error = Some(error);
                self.status = ModelTestStatus::Unavailable {
                    reason: "Invalid test configuration".into(),
                };
                cx.notify();
                return;
            }
        };

        let provider_id = LanguageModelProviderId::from(self.provider_id.as_ref().to_string());
        let provider = LanguageModelRegistry::read_global(cx).provider(&provider_id);
        let Some(provider) = provider else {
            self.status = ModelTestStatus::Unavailable {
                reason: "Provider is not available".into(),
            };
            cx.notify();
            return;
        };

        let model = provider
            .provided_models(cx)
            .into_iter()
            .find(|model| model.name().0.as_ref() == config.model_name);
        let Some(model) = model else {
            self.status = ModelTestStatus::Unavailable {
                reason: "Model is not registered in this provider".into(),
            };
            cx.notify();
            return;
        };

        self.status = ModelTestStatus::Testing;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = run_model_availability_test(model, config, cx).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(latency_ms) => {
                        this.status = ModelTestStatus::Available { latency_ms };
                        this.last_error = None;
                    }
                    Err(error) => {
                        this.status = ModelTestStatus::Unavailable {
                            reason: error.clone(),
                        };
                        this.last_error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        });

        self._run_test_task = task;
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

fn build_test_request(prompt: &str) -> LanguageModelRequest {
    LanguageModelRequest {
        thread_id: None,
        prompt_id: None,
        intent: None,
        messages: vec![LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(prompt.to_string())],
            cache: false,
            reasoning_details: None,
        }],
        tools: vec![],
        tool_choice: None,
        stop: vec![],
        temperature: None,
        thinking_allowed: true,
        bypass_rate_limit: false,
        thinking_effort: None,
    }
}

async fn run_model_availability_test(
    model: Arc<dyn LanguageModel>,
    config: ModelTestConfig,
    cx: &gpui::AsyncApp,
) -> Result<u128, SharedString> {
    let mut last_error = SharedString::from("Unknown error");
    let attempt_count = config.max_retries.saturating_add(1);

    for attempt in 1..=attempt_count {
        match run_single_attempt(&model, &config, cx).await {
            Ok(latency_ms) => return Ok(latency_ms),
            Err(error) => {
                last_error = SharedString::from(format!(
                    "Attempt {attempt}/{attempt_count} failed: {error}"
                ));
            }
        }
    }

    Err(last_error)
}

async fn run_single_attempt(
    model: &Arc<dyn LanguageModel>,
    config: &ModelTestConfig,
    cx: &gpui::AsyncApp,
) -> Result<u128, SharedString> {
    let timeout = Duration::from_secs(config.timeout_seconds.max(1));
    let start = std::time::Instant::now();

    let stream = model
        .stream_completion(build_test_request(&config.prompt), cx)
        .with_timeout(timeout, cx.background_executor())
        .await
        .map_err(|_| SharedString::from("Timed out while creating stream"))?
        .map_err(|error| SharedString::from(format_stream_error(&error)))?;

    let mut stream = stream;
    let first_event = stream
        .next()
        .with_timeout(timeout, cx.background_executor())
        .await
        .map_err(|_| SharedString::from("Timed out waiting for first response chunk"))?;

    let Some(first_event) = first_event else {
        return Err("Provider stream closed without a response".into());
    };

    first_event.map_err(|error| SharedString::from(format_stream_error(&error)))?;

    let latency_ms = start.elapsed().as_millis();
    if latency_ms > u128::from(config.degradation_threshold_ms) {
        return Err(SharedString::from(format!(
            "Latency {latency_ms}ms exceeded degradation threshold {}ms",
            config.degradation_threshold_ms
        )));
    }

    Ok(latency_ms)
}

fn format_stream_error(error: &LanguageModelCompletionError) -> String {
    format!("{error:#}")
}

impl EventEmitter<DismissEvent> for ModelAvailabilityTestModal {}

impl Focusable for ModelAvailabilityTestModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ModelAvailabilityTestModal {}

impl Render for ModelAvailabilityTestModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status_row = match &self.status {
            ModelTestStatus::Idle => Label::new("Not tested").color(Color::Muted),
            ModelTestStatus::Testing => Label::new("Testing...").color(Color::Accent),
            ModelTestStatus::Available { latency_ms } => {
                Label::new(format!("Available ({latency_ms} ms)")).color(Color::Created)
            }
            ModelTestStatus::Unavailable { reason } => {
                Label::new(format!("Unavailable · {}", reason.as_ref())).color(Color::Error)
            }
        };

        let protocol_label = match self.protocol {
            LlmCompatibleProvider::OpenAi => "OpenAI",
            LlmCompatibleProvider::Anthropic => "Anthropic",
            LlmCompatibleProvider::Gemini => "Gemini",
        };

        v_flex()
            .id("model-availability-test-modal")
            .key_context("ModelAvailabilityTestModal")
            .w(rems(56.))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::run_test))
            .child(
                Modal::new("test-llm-model-modal", None)
                    .header(
                        ModalHeader::new()
                            .headline("Model Test Configuration")
                            .description(format!(
                                "Provider: {} · Protocol: {}",
                                self.provider_id, protocol_label
                            )),
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
                    .section(
                        Section::new().child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(Label::new("Use Custom Configuration"))
                                .child(
                                    Switch::new(
                                        "use-custom-test-config",
                                        self.use_custom_config,
                                    )
                                    .on_click(cx.listener(|this, state, _window, cx| {
                                        this.use_custom_config = *state;
                                        cx.notify();
                                    })),
                                ),
                        ),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .child(self.model_name.clone())
                                .child(self.prompt.clone())
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(self.timeout_seconds.clone())
                                        .child(self.degradation_threshold_ms.clone()),
                                )
                                .child(self.max_retries.clone())
                                .when(
                                    matches!(self.use_custom_config, ToggleState::Unselected),
                                    |this| {
                                        this.child(
                                            Label::new(
                                                "Custom configuration is disabled. Defaults will be used.",
                                            )
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                        )
                                    },
                                ),
                        ),
                    )
                    .section(
                        Section::new().child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(Label::new("Availability Status").size(LabelSize::Small))
                                .child(status_row),
                        ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("cancel", "Cancel").key_binding(
                                        KeyBinding::for_action(&menu::Cancel, cx)
                                            .map(|kb| kb.size(rems_from_px(12.))),
                                    ),
                                )
                                .child(
                                    Button::new("test-availability", "Run Availability Test")
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .key_binding(
                                            KeyBinding::for_action(&menu::Confirm, cx)
                                                .map(|kb| kb.size(rems_from_px(12.))),
                                        )
                                        .disabled(matches!(self.status, ModelTestStatus::Testing))
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.run_test(&menu::Confirm, window, cx);
                                        })),
                                ),
                        ),
                    ),
            )
    }
}
