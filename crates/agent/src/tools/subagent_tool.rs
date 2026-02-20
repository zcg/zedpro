use acp_thread::SUBAGENT_SESSION_ID_META_KEY;
use agent_client_protocol as acp;
use anyhow::{Result, anyhow};
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::{rc::Rc, time::Duration};

use crate::{AgentTool, Thread, ThreadEnvironment, ToolCallEventStream};

/// Spawns an agent to perform a delegated task.
///
/// Use this tool when you want to do any of the following:
/// - Perform an investigation where all you need to know is the outcome, not the research that led to that outcome.
/// - Complete a self-contained task where you need to know if it succeeded or failed (and how), but none of its intermediate output.
/// - Run multiple tasks in parallel that would take significantly longer to run sequentially.
///
/// You control what the agent does by providing a prompt describing what the agent should do. The agent has access to the same tools you do.
///
/// You will receive the agent's final message.
///
/// Note:
/// - Agents cannot use tools you don't have access to.
/// - If spawning multiple agents that might write to the filesystem, provide guidance on how to avoid conflicts (e.g. assign each to different directories)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubagentToolInput {
    /// Short label displayed in the UI while the agent runs (e.g., "Researching alternatives")
    pub label: String,
    /// The prompt that tells the agent what task to perform. Be specific about what you want the agent to accomplish.
    pub prompt: String,
    /// Optional: Maximum runtime in seconds. No timeout by default.
    #[serde(default)]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubagentToolOutput {
    pub session_id: acp::SessionId,
    pub output: String,
}

impl From<SubagentToolOutput> for LanguageModelToolResultContent {
    fn from(output: SubagentToolOutput) -> Self {
        serde_json::to_string(&output)
            .expect("Failed to serialize SubagentToolOutput")
            .into()
    }
}

/// Tool that spawns a subagent thread to work on a task.
pub struct SubagentTool {
    parent_thread: WeakEntity<Thread>,
    environment: Rc<dyn ThreadEnvironment>,
}

impl SubagentTool {
    pub fn new(parent_thread: WeakEntity<Thread>, environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self {
            parent_thread,
            environment,
        }
    }
}

impl AgentTool for SubagentTool {
    type Input = SubagentToolInput;
    type Output = SubagentToolOutput;

    const NAME: &'static str = "subagent";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        input
            .map(|i| i.label.into())
            .unwrap_or_else(|_| "Subagent".into())
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<SubagentToolOutput>> {
        let Some(parent_thread_entity) = self.parent_thread.upgrade() else {
            return Task::ready(Err(anyhow!("Parent thread no longer exists")));
        };

        let subagent = match self.environment.create_subagent(
            parent_thread_entity,
            input.label,
            input.prompt,
            input.timeout.map(|secs| Duration::from_secs(secs)),
            cx,
        ) {
            Ok(subagent) => subagent,
            Err(err) => return Task::ready(Err(err)),
        };

        let subagent_session_id = subagent.id();

        event_stream.subagent_spawned(subagent_session_id.clone());
        let meta = acp::Meta::from_iter([(
            SUBAGENT_SESSION_ID_META_KEY.into(),
            subagent_session_id.to_string().into(),
        )]);
        event_stream.update_fields_with_meta(acp::ToolCallUpdateFields::new(), Some(meta));

        cx.spawn(async move |cx| {
            let output = subagent.wait_for_output(cx).await?;
            Ok(SubagentToolOutput {
                session_id: subagent_session_id,
                output,
            })
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        event_stream.subagent_spawned(output.session_id.clone());
        let meta = acp::Meta::from_iter([(
            SUBAGENT_SESSION_ID_META_KEY.into(),
            output.session_id.to_string().into(),
        )]);
        event_stream.update_fields_with_meta(acp::ToolCallUpdateFields::new(), Some(meta));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContextServerRegistry, Templates, Thread};
    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext};
    use project::Project;
    use prompt_store::ProjectContext;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    async fn create_thread_with_tools(cx: &mut TestAppContext) -> Entity<Thread> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/test"), json!({})).await;
        let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
        let project_context = cx.new(|_cx| ProjectContext::default());
        let context_server_store =
            project.read_with(cx, |project, _| project.context_server_store());
        let context_server_registry =
            cx.new(|cx| ContextServerRegistry::new(context_server_store.clone(), cx));

        cx.new(|cx| {
            let mut thread = Thread::new(
                project,
                project_context,
                context_server_registry,
                Templates::new(),
                None,
                cx,
            );
            thread.add_tool(crate::NowTool, None);
            thread.add_tool(crate::WebSearchTool, None);
            thread
        })
    }

    #[gpui::test]
    async fn test_validate_allowed_tools_succeeds_for_valid_tools(cx: &mut TestAppContext) {
        let thread = create_thread_with_tools(cx).await;

        cx.update(|cx| {
            assert!(SubagentTool::validate_allowed_tools(&None, &thread, cx).is_ok());

            let valid_tools = Some(vec!["now".to_string()]);
            assert!(SubagentTool::validate_allowed_tools(&valid_tools, &thread, cx).is_ok());

            let both_tools = Some(vec!["now".to_string(), "web_search".to_string()]);
            assert!(SubagentTool::validate_allowed_tools(&both_tools, &thread, cx).is_ok());
        });
    }

    #[gpui::test]
    async fn test_validate_allowed_tools_fails_for_unknown_tools(cx: &mut TestAppContext) {
        let thread = create_thread_with_tools(cx).await;

        cx.update(|cx| {
            let unknown_tools = Some(vec!["nonexistent_tool".to_string()]);
            let result = SubagentTool::validate_allowed_tools(&unknown_tools, &thread, cx);
            assert!(result.is_err());
            let error_message = result.unwrap_err().to_string();
            assert!(
                error_message.contains("'nonexistent_tool'"),
                "Expected error to mention the invalid tool name, got: {error_message}"
            );

            let mixed_tools = Some(vec![
                "now".to_string(),
                "fake_tool_a".to_string(),
                "fake_tool_b".to_string(),
            ]);
            let result = SubagentTool::validate_allowed_tools(&mixed_tools, &thread, cx);
            assert!(result.is_err());
            let error_message = result.unwrap_err().to_string();
            assert!(
                error_message.contains("'fake_tool_a'") && error_message.contains("'fake_tool_b'"),
                "Expected error to mention both invalid tool names, got: {error_message}"
            );
            assert!(
                !error_message.contains("'now'"),
                "Expected error to not mention valid tool 'now', got: {error_message}"
            );
        });
    }

    #[test]
    fn test_subagent_tool_kind() {
        assert_eq!(SubagentTool::kind(), acp::ToolKind::Other);
    }
}
