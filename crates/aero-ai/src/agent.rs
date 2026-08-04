//! Agentic tool-use loop (方向三 "能动AI" — from a passive Q&A helper to an agent
//! that *acts*).
//!
//! [`run_agent_loop`] drives a multi-step conversation: ask the model, and as long
//! as it responds with `tool_use` blocks, execute those tools and feed the results
//! back, until the model produces a plain-text answer (or the iteration cap trips).
//! This lets the model gather what it needs on demand — e.g. search the room a few
//! times with refined queries — instead of answering from one fixed retrieval.
//!
//! The model seam is the [`ToolChat`] trait (implemented by
//! [`AnthropicClient`](crate::anthropic::AnthropicClient)); tools implement
//! [`AgentTool`]. Both are trait objects, so the whole loop — message threading,
//! tool dispatch, iteration cap, usage accounting — is unit-tested with a scripted
//! mock chat + fake tools and **no API key**.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::anthropic::{AgentTurn, AnthropicClient, ToolDef, ToolUse, Usage};
use crate::error::Result;

/// Max tools actually executed per model turn. Bounds the per-answer I/O (DB/blob
/// reads) explicitly instead of relying on `max_tokens` to indirectly limit how
/// many `tool_use` blocks a turn can carry. Tool calls beyond this still get a
/// paired (synthetic) `tool_result` so the conversation stays API-valid.
const MAX_TOOLS_PER_TURN: usize = 8;

/// One tool-aware model turn. Implemented by the real client; mocked in tests.
#[async_trait]
pub trait ToolChat: Send + Sync {
    /// Send the conversation + advertised tools, return the model's turn.
    async fn next_turn(
        &self,
        system: &str,
        messages: &[Value],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<AgentTurn>;
}

#[async_trait]
impl ToolChat for AnthropicClient {
    async fn next_turn(
        &self,
        system: &str,
        messages: &[Value],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<AgentTurn> {
        self.complete_with_tools(system, messages, tools, max_tokens)
            .await
    }
}

/// A capability the agent may invoke. `definition()` advertises it to the model;
/// `run()` executes one call with the model-supplied `input` and returns the text
/// result that is fed back as a `tool_result`.
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDef;
    async fn run(&self, input: &Value) -> Result<String>;
}

/// The result of a completed (or capped) agent loop.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    /// The model's final text answer (the last turn's text if the cap was hit).
    pub answer: String,
    /// Model turns taken (each is one `next_turn` call).
    pub iterations: usize,
    /// Total tool invocations across all turns.
    pub tool_calls: usize,
    /// Summed token usage across every turn (for cost accounting).
    pub usage: Usage,
    /// True if the loop stopped at `max_iters` rather than the model finishing —
    /// the answer may be partial.
    pub hit_cap: bool,
}

/// Sum two usage records, saturating (token counts never wrap).
fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
    }
}

/// Rebuild the assistant turn as Anthropic content blocks so the next request sees
/// a valid conversation (text first, then each `tool_use` the model asked for).
fn assistant_message(turn: &AgentTurn) -> Value {
    let mut content = Vec::with_capacity(turn.tool_uses.len() + 1);
    if !turn.text.is_empty() {
        content.push(json!({ "type": "text", "text": turn.text }));
    }
    for tu in &turn.tool_uses {
        content.push(json!({
            "type": "tool_use",
            "id": tu.id,
            "name": tu.name,
            "input": tu.input,
        }));
    }
    json!({ "role": "assistant", "content": content })
}

/// Execute one requested tool, returning its `tool_result` block. An unknown tool
/// name yields an error result (fed back to the model) rather than aborting the
/// loop — the model can recover or apologize.
async fn run_one_tool(tools: &[Arc<dyn AgentTool>], tu: &ToolUse) -> Result<Value> {
    let output = match tools.iter().find(|t| t.definition().name == tu.name) {
        Some(tool) => tool.run(&tu.input).await?,
        None => format!("error: unknown tool '{}'", tu.name),
    };
    Ok(json!({ "type": "tool_result", "tool_use_id": tu.id, "content": output }))
}

/// Drive the tool-use loop to a final answer.
///
/// Threads the conversation: user question → (assistant `tool_use` → user
/// `tool_result`)* → assistant text. Stops when the model returns no tool calls or after `max_iters`
/// turns. Tool outputs are fed back verbatim; the model decides when it has enough.
///
/// # Errors
/// Propagates any [`ToolChat::next_turn`] error (transport / API). Tool failures are
/// surfaced to the model as result text, not errors.
pub async fn run_agent_loop(
    chat: &dyn ToolChat,
    tools: &[Arc<dyn AgentTool>],
    system: &str,
    question: &str,
    max_iters: usize,
    max_tokens: u32,
) -> Result<AgentOutcome> {
    let defs: Vec<ToolDef> = tools.iter().map(|t| t.definition()).collect();
    let mut messages: Vec<Value> = vec![json!({ "role": "user", "content": question })];
    let mut usage = Usage::default();
    let mut tool_calls = 0usize;
    let mut last_text = String::new();

    for iter in 1..=max_iters.max(1) {
        let turn = chat.next_turn(system, &messages, &defs, max_tokens).await?;
        usage = add_usage(usage, turn.usage);
        last_text = turn.text.clone();

        if turn.is_final() {
            return Ok(AgentOutcome {
                answer: turn.text,
                iterations: iter,
                tool_calls,
                usage,
                hit_cap: false,
            });
        }

        // Record the assistant's tool_use turn, then run each tool and feed the
        // results back as a single user turn. Real tool execution is bounded to
        // MAX_TOOLS_PER_TURN per turn so the per-answer I/O cost (DB/blob reads) is
        // explicit rather than relying on `max_tokens` to indirectly limit how many
        // tool_use blocks the model can emit. Every tool_use STILL gets a paired
        // tool_result (the Messages API requires 1:1) — calls past the cap get a
        // synthetic "not executed" result instead of doing I/O.
        messages.push(assistant_message(&turn));
        let mut result_blocks = Vec::with_capacity(turn.tool_uses.len());
        for (i, tu) in turn.tool_uses.iter().enumerate() {
            if i < MAX_TOOLS_PER_TURN {
                tool_calls += 1;
                result_blocks.push(run_one_tool(tools, tu).await?);
            } else {
                result_blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tu.id,
                    "content": "error: per-turn tool limit reached; not executed",
                }));
            }
        }
        messages.push(json!({ "role": "user", "content": result_blocks }));
    }

    // Cap reached without a final text turn — return the best text we have.
    Ok(AgentOutcome {
        answer: last_text,
        iterations: max_iters.max(1),
        tool_calls,
        usage,
        hit_cap: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A scripted [`ToolChat`]: returns the next canned turn each call and records
    /// the `messages` it was handed, so tests can assert the threading.
    struct MockChat {
        script: Mutex<std::collections::VecDeque<AgentTurn>>,
        seen: Mutex<Vec<Vec<Value>>>,
    }

    impl MockChat {
        fn new(turns: Vec<AgentTurn>) -> Self {
            Self {
                script: Mutex::new(turns.into_iter().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ToolChat for MockChat {
        async fn next_turn(
            &self,
            _system: &str,
            messages: &[Value],
            _tools: &[ToolDef],
            _max_tokens: u32,
        ) -> Result<AgentTurn> {
            self.seen.lock().unwrap().push(messages.to_vec());
            Ok(self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockChat script exhausted"))
        }
    }

    /// A tool that echoes a fixed reply and records every input it was called with.
    struct FakeTool {
        name: String,
        reply: String,
        calls: Mutex<Vec<Value>>,
    }

    impl FakeTool {
        fn new(name: &str, reply: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                reply: reply.into(),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AgentTool for FakeTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: self.name.clone(),
                description: "fake".into(),
                input_schema: json!({ "type": "object" }),
            }
        }
        async fn run(&self, input: &Value) -> Result<String> {
            self.calls.lock().unwrap().push(input.clone());
            Ok(self.reply.clone())
        }
    }

    fn text_turn(t: &str, usage: (u32, u32)) -> AgentTurn {
        AgentTurn {
            text: t.into(),
            tool_uses: vec![],
            usage: Usage {
                input_tokens: usage.0,
                output_tokens: usage.1,
            },
        }
    }

    fn tool_turn(id: &str, name: &str, input: Value, usage: (u32, u32)) -> AgentTurn {
        AgentTurn {
            text: String::new(),
            tool_uses: vec![ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }],
            usage: Usage {
                input_tokens: usage.0,
                output_tokens: usage.1,
            },
        }
    }

    #[tokio::test]
    async fn answers_immediately_when_no_tools_requested() {
        let chat = MockChat::new(vec![text_turn("42", (5, 2))]);
        let out = run_agent_loop(&chat, &[], "be helpful", "the answer?", 4, 256)
            .await
            .unwrap();
        assert_eq!(out.answer, "42");
        assert_eq!(out.iterations, 1);
        assert_eq!(out.tool_calls, 0);
        assert!(!out.hit_cap);
        assert_eq!(out.usage.input_tokens, 5);
    }

    #[tokio::test]
    async fn executes_a_tool_then_answers_with_its_result() {
        let tool = FakeTool::new("search", "found: ship-it");
        let chat = MockChat::new(vec![
            tool_turn("tu_1", "search", json!({ "q": "deploys" }), (10, 3)),
            text_turn("based on search: shipped", (8, 4)),
        ]);
        let tools: Vec<Arc<dyn AgentTool>> = vec![tool.clone()];
        let out = run_agent_loop(&chat, &tools, "be helpful", "did we ship?", 4, 256)
            .await
            .unwrap();

        assert_eq!(out.answer, "based on search: shipped");
        assert_eq!(out.iterations, 2);
        assert_eq!(out.tool_calls, 1);
        assert!(!out.hit_cap);
        // Usage summed across both turns.
        assert_eq!(out.usage.input_tokens, 18);
        assert_eq!(out.usage.output_tokens, 7);

        // The tool saw the model's input.
        assert_eq!(tool.calls.lock().unwrap()[0]["q"], "deploys");

        // The SECOND model call's messages include the assistant tool_use turn and
        // the user tool_result carrying our output.
        let seen = chat.seen.lock().unwrap();
        let second = &seen[1];
        assert_eq!(
            second.len(),
            3,
            "user q + assistant tool_use + user tool_result"
        );
        assert_eq!(second[1]["role"], "assistant");
        assert_eq!(second[1]["content"][0]["type"], "tool_use");
        assert_eq!(second[2]["role"], "user");
        assert_eq!(second[2]["content"][0]["type"], "tool_result");
        assert_eq!(second[2]["content"][0]["tool_use_id"], "tu_1");
        assert_eq!(second[2]["content"][0]["content"], "found: ship-it");
    }

    #[tokio::test]
    async fn unknown_tool_feeds_back_an_error_but_loop_continues() {
        let chat = MockChat::new(vec![
            tool_turn("tu_x", "nonexistent", json!({}), (1, 1)),
            text_turn("sorry, I could not look that up", (1, 1)),
        ]);
        let out = run_agent_loop(&chat, &[], "be helpful", "q", 4, 256)
            .await
            .unwrap();
        assert_eq!(out.answer, "sorry, I could not look that up");
        assert_eq!(out.tool_calls, 1);
        // The error result was threaded back to the model on the 2nd call.
        let seen = chat.seen.lock().unwrap();
        let result = &seen[1][2]["content"][0]["content"];
        assert!(result
            .as_str()
            .unwrap()
            .contains("unknown tool 'nonexistent'"));
    }

    #[tokio::test]
    async fn stops_and_flags_when_iteration_cap_is_hit() {
        // The model keeps calling a tool forever; cap at 2 turns.
        let tool = FakeTool::new("loop", "again");
        let chat = MockChat::new(vec![
            tool_turn("a", "loop", json!({}), (1, 1)),
            tool_turn("b", "loop", json!({}), (1, 1)),
            tool_turn("c", "loop", json!({}), (1, 1)),
        ]);
        let tools: Vec<Arc<dyn AgentTool>> = vec![tool];
        let out = run_agent_loop(&chat, &tools, "be helpful", "q", 2, 256)
            .await
            .unwrap();
        assert!(out.hit_cap, "should report hitting the cap");
        assert_eq!(out.iterations, 2);
        assert_eq!(out.tool_calls, 2);
    }

    #[tokio::test]
    async fn per_turn_tool_calls_are_capped_but_all_get_results() {
        // A single turn asks for MANY more tools than the per-turn cap allows.
        let n = MAX_TOOLS_PER_TURN + 5;
        let tool = FakeTool::new("t", "ok");
        let mut tool_uses = Vec::new();
        for i in 0..n {
            tool_uses.push(ToolUse {
                id: format!("tu_{i}"),
                name: "t".into(),
                input: json!({}),
            });
        }
        let chat = MockChat::new(vec![
            AgentTurn {
                text: String::new(),
                tool_uses,
                usage: Usage::default(),
            },
            text_turn("done", (1, 1)),
        ]);
        let tools: Vec<Arc<dyn AgentTool>> = vec![tool.clone()];
        let out = run_agent_loop(&chat, &tools, "be helpful", "q", 4, 256)
            .await
            .unwrap();

        // Only the cap's worth of tools actually executed (real I/O bounded)…
        assert_eq!(out.tool_calls, MAX_TOOLS_PER_TURN);
        assert_eq!(tool.calls.lock().unwrap().len(), MAX_TOOLS_PER_TURN);
        // …but EVERY tool_use got a paired tool_result (Messages API requires 1:1),
        // so the second model call saw n result blocks and the loop finished.
        let seen = chat.seen.lock().unwrap();
        let results = seen[1][2]["content"].as_array().expect("result blocks");
        assert_eq!(
            results.len(),
            n,
            "one tool_result per tool_use, capped or not"
        );
        // The over-cap results are synthetic "not executed" markers.
        let over = results[MAX_TOOLS_PER_TURN]["content"].as_str().unwrap();
        assert!(
            over.contains("per-turn tool limit"),
            "over-cap call is a synthetic skip: {over}"
        );
        assert_eq!(out.answer, "done");
    }
}
