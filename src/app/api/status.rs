use std::time::{Duration, Instant};

use crate::api::schema::{ResponseResult, StatusClearParams, StatusSetParams};
use crate::app::App;

use super::responses::{encode_error, encode_success};

const STATUS_SOURCE_MAX_CHARS: usize = 80;
const STATUS_TTL_MIN_MS: u64 = 1;
const STATUS_TTL_MAX_MS: u64 = 86_400_000;

impl App {
    pub(super) fn handle_status_set(&mut self, id: String, params: StatusSetParams) -> String {
        let source = match normalize_status_source(params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_status_source", message),
        };
        let ttl = match normalize_status_ttl(params.ttl_ms) {
            Ok(ttl) => ttl,
            Err(message) => return encode_error(id, "invalid_status_ttl", message),
        };
        // Sanitize to plain text on ingest, with the same rules the endpoint
        // applies to `tab_bar_right` command output: control and escape bytes
        // are dropped so a pushed value cannot inject styling. Style comes only
        // from the trusted `#[…]` directives in the format string. An empty
        // result is kept as an empty slot, which blanks the token and drops its
        // separator rather than leaving the previous value stuck.
        let text =
            crate::app::tab_bar_status::sanitize_status_text(&params.text).unwrap_or_default();
        let now = Instant::now();
        if self.state.status_slots.at_capacity_for(&source, now) {
            return encode_error(
                id,
                "status_slot_limit",
                "the status strip holds at most 32 pushed sources",
            );
        }
        let changed = self
            .state
            .status_slots
            .set(source.clone(), text, params.seq, ttl, now);
        if changed && self.state.status_strip.references_slot(&source) {
            self.request_status_strip_repaint();
        }

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_status_clear(&mut self, id: String, params: StatusClearParams) -> String {
        let source = match normalize_status_source(params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_status_source", message),
        };
        if self.state.status_slots.clear(&source, Instant::now())
            && self.state.status_strip.references_slot(&source)
        {
            self.request_status_strip_repaint();
        }

        encode_success(id, ResponseResult::Ok {})
    }

    /// Reuse the state-change to repaint path: a rendered pushed value changed
    /// the
    /// strip, so flag a redraw. No new polling is introduced.
    fn request_status_strip_repaint(&self) {
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }
}

/// Validate a push-lane source key. Mirrors `normalize_metadata_source`: ASCII
/// letters/digits plus `:._-`, non-empty, at most 80 characters.
fn normalize_status_source(value: String) -> Result<String, &'static str> {
    let value = value.trim();
    if value.is_empty() {
        return Err("status source must not be empty");
    }
    if value.chars().count() > STATUS_SOURCE_MAX_CHARS {
        return Err("status source must be 80 characters or fewer");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '.' | '_' | '-'))
    {
        return Err(
            "status source may contain only ASCII letters, digits, colon, dot, underscore, and hyphen",
        );
    }
    Ok(value.to_string())
}

/// Validate an optional TTL in milliseconds. Mirrors `normalize_metadata_ttl`.
fn normalize_status_ttl(ttl_ms: Option<u64>) -> Result<Option<Duration>, &'static str> {
    let Some(ttl_ms) = ttl_ms else {
        return Ok(None);
    };
    if ttl_ms < STATUS_TTL_MIN_MS {
        return Err("status ttl_ms must be at least 1");
    }
    if ttl_ms > STATUS_TTL_MAX_MS {
        return Err("status ttl_ms must be 86400000 or less");
    }
    Ok(Some(Duration::from_millis(ttl_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, Request};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn response_json(response: &str) -> serde_json::Value {
        serde_json::from_str(response).expect("valid response json")
    }

    #[test]
    fn status_set_writes_store_and_sanitizes() {
        let mut app = test_app();
        let response = app.handle_api_request(Request {
            id: "s1".into(),
            method: Method::StatusSet(StatusSetParams {
                // Control bytes (ESC, BEL) are stripped by the shared sanitizer
                // and the newline collapses, so only printable text survives.
                source: "git".into(),
                text: "\u{1b}main\u{7}".into(),
                seq: None,
                ttl_ms: None,
            }),
        });
        assert_eq!(response_json(&response)["result"]["type"], "ok");
        assert_eq!(
            app.state.status_slots.get_for_test("git"),
            Some("main".to_string())
        );
    }

    #[test]
    fn status_set_honors_seq_and_clear_removes() {
        let mut app = test_app();
        app.handle_api_request(Request {
            id: "s1".into(),
            method: Method::StatusSet(StatusSetParams {
                source: "git".into(),
                text: "main".into(),
                seq: Some(5),
                ttl_ms: None,
            }),
        });
        // Older seq is ignored.
        app.handle_api_request(Request {
            id: "s2".into(),
            method: Method::StatusSet(StatusSetParams {
                source: "git".into(),
                text: "old".into(),
                seq: Some(4),
                ttl_ms: None,
            }),
        });
        assert_eq!(
            app.state.status_slots.get_for_test("git"),
            Some("main".to_string())
        );

        let response = app.handle_api_request(Request {
            id: "c1".into(),
            method: Method::StatusClear(StatusClearParams {
                source: "git".into(),
            }),
        });
        assert_eq!(response_json(&response)["result"]["type"], "ok");
        assert_eq!(app.state.status_slots.get_for_test("git"), None);
    }

    #[test]
    fn status_set_rejects_invalid_source() {
        let mut app = test_app();
        let response = app.handle_api_request(Request {
            id: "s1".into(),
            method: Method::StatusSet(StatusSetParams {
                source: "bad source!".into(),
                text: "x".into(),
                seq: None,
                ttl_ms: None,
            }),
        });
        assert_eq!(
            response_json(&response)["error"]["code"],
            "invalid_status_source"
        );
    }

    #[test]
    fn status_set_rejects_out_of_range_ttl() {
        let mut app = test_app();
        for ttl_ms in [0, STATUS_TTL_MAX_MS + 1] {
            let response = app.handle_api_request(Request {
                id: "s1".into(),
                method: Method::StatusSet(StatusSetParams {
                    source: "git".into(),
                    text: "x".into(),
                    seq: None,
                    ttl_ms: Some(ttl_ms),
                }),
            });
            assert_eq!(
                response_json(&response)["error"]["code"],
                "invalid_status_ttl"
            );
        }
    }
}
