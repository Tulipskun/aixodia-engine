//! The SSE sink a run's chat executor writes into.
//!
//! Port of the Go `run/manager.go` `eventSink`. The gateway produces the same
//! `data: ...` events it sends to a direct client; this sink turns them into
//! numbered, persisted run events. Assistant text is coalesced on a short timer
//! so a fast model does not write hundreds of database rows a second.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::event::{KIND_ASSISTANT, KIND_ERROR, KIND_FINAL, KIND_REASONING, KIND_TOOL, KIND_TURN};
use crate::manager::ActiveRun;

struct SinkState {
    pending: Vec<u8>,
    raw: String,
    status: u16,
    assistant: String,
    reasoning: String,
    reason_round: i64,
    last_flush: Instant,
    closed: bool,
    err_text: String,
    has_timer: bool,
}

pub struct SseSink {
    run: Arc<ActiveRun>,
    state: Mutex<SinkState>,
    closed: AtomicBool,
}

impl SseSink {
    pub fn new(run: Arc<ActiveRun>) -> Arc<Self> {
        Arc::new(Self {
            run,
            state: Mutex::new(SinkState {
                pending: Vec::new(),
                raw: String::new(),
                status: 0,
                assistant: String::new(),
                reasoning: String::new(),
                reason_round: 0,
                last_flush: Instant::now(),
                closed: false,
                err_text: String::new(),
                has_timer: false,
            }),
            closed: AtomicBool::new(false),
        })
    }

    /// Records the status line the executor would have written. A non-200 is
    /// turned into a failure once the executor returns.
    pub fn write_header(&self, status: u16) {
        let mut state = self.state.lock().unwrap();
        if state.status == 0 {
            state.status = status;
        }
    }

    /// Feeds a chunk of SSE bytes. Incomplete events wait for the next chunk.
    pub fn write(self: &Arc<Self>, chunk: &[u8]) {
        let mut state = self.state.lock().unwrap();
        // `raw` is only used to describe a non-200 chat response. A long
        // successful turn must not pin its whole transcript here.
        if state.raw.len() < 8 * 1024 {
            let remaining = 8 * 1024 - state.raw.len();
            let take = chunk.len().min(remaining);
            state.raw.push_str(&String::from_utf8_lossy(&chunk[..take]));
        }
        state.pending.extend_from_slice(chunk);
        while let Some(index) = find(&state.pending, b"\n\n") {
            let event: Vec<u8> = state.pending[..index].to_vec();
            state.pending.drain(..index + 2);
            if let Some(data) = event.strip_prefix(b"data: ") {
                Self::handle_locked(self, &mut state, data);
            }
        }
    }

    fn handle_locked(sink: &Arc<Self>, state: &mut SinkState, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        if text.trim() == "[DONE]" {
            Self::flush_locked(sink, state);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(object) = value.as_object() else {
            return;
        };

        if let Some(message) = object
            .get("error")
            .and_then(Value::as_object)
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            Self::flush_locked(sink, state);
            state.err_text = message.to_string();
            sink.run.append(KIND_ERROR, json!({"message": message}));
            return;
        }

        if let Some(tool) = object.get("tool_event").and_then(Value::as_object) {
            let stage = tool
                .get("stage")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let round = tool.get("round").and_then(Value::as_i64).unwrap_or(0);
            let text = tool.get("text").and_then(Value::as_str).unwrap_or_default();
            if stage == "reasoning" && !text.is_empty() {
                state.reasoning.push_str(text);
                state.reason_round = round;
                Self::schedule_locked(sink, state);
                return;
            }
            Self::flush_locked(sink, state);
            // Keep the original payload: usage figures and the presentation
            // block are fields this sink does not need to understand, but the
            // phone does.
            if let Some(raw) = object.get("tool_event") {
                sink.run.append(KIND_TOOL, raw.clone());
            }
            return;
        }

        if let Some(turns) = object.get("turns").and_then(Value::as_array) {
            for turn in turns {
                Self::flush_locked(sink, state);
                sink.run.append(KIND_TURN, turn.clone());
            }
        }
        if let Some(turn) = object.get("turn") {
            Self::flush_locked(sink, state);
            let usage = object.get("usage").cloned().unwrap_or(Value::Null);
            sink.run
                .append(KIND_FINAL, json!({"turn": turn, "usage": usage}));
            return;
        }
        if let Some(choices) = object.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let content = choice
                    .get("delta")
                    .and_then(|delta| delta.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if content.is_empty() {
                    continue;
                }
                state.assistant.push_str(content);
                Self::schedule_locked(sink, state);
            }
        }
    }

    fn schedule_locked(sink: &Arc<Self>, state: &mut SinkState) {
        if state.closed {
            return;
        }
        let elapsed = state.last_flush.elapsed();
        let window = Duration::from_millis(120);
        if elapsed >= window {
            Self::flush_locked(sink, state);
            return;
        }
        if state.has_timer {
            return;
        }
        state.has_timer = true;
        let wait = window - elapsed;
        let sink = sink.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let mut state = sink.state.lock().unwrap();
            state.has_timer = false;
            if !state.closed {
                Self::flush_locked(&sink, &mut state);
            }
        });
    }

    fn flush_locked(sink: &Arc<Self>, state: &mut SinkState) {
        state.last_flush = Instant::now();
        if !state.assistant.is_empty() {
            let text = std::mem::take(&mut state.assistant);
            sink.run.append(KIND_ASSISTANT, json!({"text": text}));
        }
        if !state.reasoning.is_empty() {
            let text = std::mem::take(&mut state.reasoning);
            let round = state.reason_round;
            sink.run
                .append(KIND_REASONING, json!({"text": text, "round": round}));
        }
    }

    /// Flushes anything buffered and stops the timer. Safe to call twice.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        Self::flush_locked_plain(self, &mut state);
    }

    fn flush_locked_plain(sink: &Self, state: &mut SinkState) {
        state.last_flush = Instant::now();
        if !state.assistant.is_empty() {
            let text = std::mem::take(&mut state.assistant);
            sink.run.append(KIND_ASSISTANT, json!({"text": text}));
        }
        if !state.reasoning.is_empty() {
            let text = std::mem::take(&mut state.reasoning);
            let round = state.reason_round;
            sink.run
                .append(KIND_REASONING, json!({"text": text, "round": round}));
        }
    }

    pub fn status(&self) -> u16 {
        self.state.lock().unwrap().status
    }

    pub fn error_text(&self) -> String {
        self.state.lock().unwrap().err_text.clone()
    }

    pub fn raw(&self) -> String {
        self.state.lock().unwrap().raw.clone()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
