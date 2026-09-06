//! The todo hand: the model's live plan, mirrored to surfaces as
//! [`ka_protocol::Event::Todos`]. Each call REPLACES the whole list — the
//! model sends every item with its current state, so there is no merge
//! logic to drift. The normalized list lands in a shared slot; the voice
//! forwards it to surfaces after execution (the hand itself has no event
//! channel by design).

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// Maximum items kept in one todo list.
pub const MAX_ITEMS: usize = 32;
/// Maximum characters per item text.
pub const MAX_TEXT: usize = 200;

/// Shared slot the hand writes and the voice reads to emit the
/// [`ka_protocol::Event::Todos`] surface event.
pub type TodoSlot = std::sync::Arc<parking_lot::Mutex<Vec<ka_protocol::TodoItem>>>;

/// A fresh, empty slot.
pub fn slot() -> TodoSlot {
    std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()))
}

/// The todo tool.
pub struct TodoHand {
    slot: TodoSlot,
}

impl TodoHand {
    /// Hand over a shared slot (usually one per voice, cloned into it).
    pub fn new(slot: TodoSlot) -> Self {
        Self { slot }
    }
}

/// Normalize model arguments into the canonical list. Unknown states fall
/// back to pending, texts truncate at [`MAX_TEXT`] chars, and the list
/// caps at [`MAX_ITEMS`] items. An empty array clears the list. Malformed
/// payloads (missing `items`, non-array `items`, item without string
/// `text`) error so the model can self-correct.
pub fn normalize(args: &Value) -> Result<Vec<ka_protocol::TodoItem>, String> {
    let Some(items) = args.get("items") else {
        return Err("todo requires an `items` array".to_string());
    };
    let Some(items) = items.as_array() else {
        return Err("todo `items` must be an array".to_string());
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().take(MAX_ITEMS).enumerate() {
        let Some(text) = item.get("text").and_then(Value::as_str) else {
            return Err(format!("items[{i}] needs a string `text`"));
        };
        let text: String = text.chars().take(MAX_TEXT).collect();
        let state = match item.get("state").and_then(Value::as_str) {
            Some("done") => ka_protocol::TodoState::Done,
            _ => ka_protocol::TodoState::Pending,
        };
        out.push(ka_protocol::TodoItem { text, state });
    }
    Ok(out)
}

impl Hand for TodoHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "todo".into(),
            description: "Maintain your live todo list for the session sidebar. Each call \
replaces the WHOLE list: send every item with its state (`pending` or `done`), in display \
order; an empty list clears it. Keep it current on multi-step tasks."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "maxItems": MAX_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string", "maxLength": MAX_TEXT },
                                "state": { "type": "string", "enum": ["pending", "done"] }
                            },
                            "required": ["text", "state"]
                        }
                    }
                },
                "required": ["items"]
            }),
            clearance: Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let sent = args
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, |a| a.len());
            match normalize(args) {
                Ok(items) => {
                    let kept = items.len();
                    let done = items
                        .iter()
                        .filter(|i| i.state == ka_protocol::TodoState::Done)
                        .count();
                    *self.slot.lock() = items;
                    let mut note = format!("todo list updated: {kept} items ({done} done)");
                    if sent > kept {
                        note.push_str(&format!("; capped at {MAX_ITEMS} items"));
                    }
                    ToolOutput::ok(note)
                }
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_keeps_full_shape() {
        let items = normalize(&json!({
            "items": [
                {"text": "first", "state": "pending"},
                {"text": "second", "state": "done"},
            ]
        }))
        .unwrap();
        assert_eq!(
            items,
            vec![
                ka_protocol::TodoItem {
                    text: "first".into(),
                    state: ka_protocol::TodoState::Pending
                },
                ka_protocol::TodoItem {
                    text: "second".into(),
                    state: ka_protocol::TodoState::Done
                },
            ]
        );
    }

    #[test]
    fn normalize_caps_items_and_text() {
        let items: Vec<_> = (0..40)
            .map(|i| json!({"text": "x".repeat(300), "state": "pending", "n": i}))
            .collect();
        let out = normalize(&json!({ "items": items })).unwrap();
        assert_eq!(out.len(), MAX_ITEMS, "list caps at {MAX_ITEMS}");
        assert!(out.iter().all(|i| i.text.chars().count() == MAX_TEXT));
    }

    #[test]
    fn normalize_empty_array_clears_and_unknown_state_falls_back() {
        assert!(normalize(&json!({ "items": [] })).unwrap().is_empty());
        let items = normalize(&json!({ "items": [{"text": "a", "state": "bananas"}] })).unwrap();
        assert_eq!(items[0].state, ka_protocol::TodoState::Pending);
        let items = normalize(&json!({ "items": [{"text": "a"}] })).unwrap();
        assert_eq!(items[0].state, ka_protocol::TodoState::Pending);
    }

    #[test]
    fn normalize_rejects_malformed_payloads() {
        assert!(normalize(&json!({})).is_err(), "missing items");
        assert!(
            normalize(&json!({ "items": "hurry" })).is_err(),
            "non-array"
        );
        assert!(
            normalize(&json!({ "items": [{"state": "done"}] })).is_err(),
            "item without text"
        );
    }

    #[test]
    fn execute_replaces_slot_and_reports() {
        let hand = TodoHand::new(slot());
        let ctx = test_ctx();
        let out = tokio_test_block(hand.execute(
            &json!({"items": [
                {"text": "dig", "state": "done"},
                {"text": "fill", "state": "pending"},
            ]}),
            &ctx,
        ));
        assert!(!out.is_error);
        assert!(out.content.contains("2 items (1 done)"), "{}", out.content);
        let held = hand.slot.lock();
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].state, ka_protocol::TodoState::Done);
    }

    #[test]
    fn execute_error_leaves_slot_untouched() {
        let hand = TodoHand::new(slot());
        let ctx = test_ctx();
        let ok = tokio_test_block(hand.execute(
            &json!({"items": [{"text": "kept", "state": "pending"}]}),
            &ctx,
        ));
        assert!(!ok.is_error);
        let bad = tokio_test_block(hand.execute(&json!({}), &ctx));
        assert!(bad.is_error);
        assert_eq!(
            hand.slot.lock().len(),
            1,
            "failed call must not touch the list"
        );
    }

    // — helpers —

    fn test_ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: std::sync::Arc::new(super::super::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                super::super::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
        }
    }

    fn tokio_test_block(fut: impl Future<Output = ToolOutput>) -> ToolOutput {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
