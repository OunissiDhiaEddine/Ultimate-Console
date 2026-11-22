//! The messages between the Quick Access Menu script and the agent.
//!
//! Script → agent: the script calls `window[BINDING](json)` with
//! `{"id": n, "method": "...", "params": {...}}`, which reaches the agent as
//! a `Runtime.bindingCalled` event. `id` 0 means "no answer wanted" (logs).
//!
//! Agent → script: the agent evaluates `window.__uc.reply({"id": n,
//! "result": ...})` or `{"id": n, "error": "..."}`, and
//! `window.__uc.push("refresh", null)` to ask an open tab to re-read.

use serde::Deserialize;
use serde_json::{Value, json};

/// Name of the DevTools binding the script calls.
pub const BINDING: &str = "ucSteamMenuSend";

/// The script, before [`menu_script`] fills in the placeholders.
pub const MENU_JS: &str = include_str!("../js/menu.js");

const VERSION_PLACEHOLDER: &str = "__UC_VERSION__";
const BINDING_PLACEHOLDER: &str = "__UC_BINDING__";

/// The script to evaluate in Steam, for agent version `version`. Evaluating
/// it returns the same object as `window.__uc.health()`.
pub fn menu_script(version: &str) -> String {
    MENU_JS
        .replace(VERSION_PLACEHOLDER, &js_string_body(version))
        .replace(BINDING_PLACEHOLDER, BINDING)
}

/// A call from the script.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Call {
    /// 0 when the script wants no answer.
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Call {
    pub fn parse(payload: &str) -> serde_json::Result<Self> {
        serde_json::from_str(payload)
    }

    pub fn wants_reply(&self) -> bool {
        self.id != 0
    }
}

/// The expression that hands `result` back to the script for call `id`.
pub fn reply_expression(id: u64, result: &Result<Value, String>) -> String {
    let msg = match result {
        Ok(v) => json!({ "id": id, "result": v }),
        Err(e) => json!({ "id": id, "error": e }),
    };
    format!("window.__uc && window.__uc.reply({})", js_literal(&msg))
}

/// The expression that pushes an event to an open tab.
pub fn push_expression(event: &str, data: &Value) -> String {
    format!(
        "window.__uc && window.__uc.push({}, {})",
        js_literal(&Value::from(event)),
        js_literal(data)
    )
}

/// Opens the Quick Access Menu on the Ultimate Console tab; evaluates to
/// `true` if it could.
pub const OPEN_EXPRESSION: &str = "!!(window.__uc && window.__uc.open())";

/// Evaluates to the script's health report, or `null` if it is gone (Steam
/// reloaded its UI).
pub const HEALTH_EXPRESSION: &str =
    "window.__uc && window.__uc.version ? (window.__uc.ensure(), window.__uc.health()) : null";

/// JSON is valid JavaScript except that older engines reject raw U+2028 and
/// U+2029 in string literals; escape them to be safe.
fn js_literal(v: &Value) -> String {
    v.to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// `s` escaped for use between single quotes in the script.
fn js_string_body(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
        .collect()
}

/// What the script reports about itself (`window.__uc.health()`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Health {
    pub version: String,
    pub webpack: bool,
    pub react: bool,
    /// The Quick Access Menu component is hooked.
    pub hooked: bool,
    /// Which way it was hooked.
    pub how: Option<String>,
    /// The tab has been added to a rendered menu at least once.
    pub tab_shown: bool,
    /// Steam components found by the script (`Focusable`, `SliderField` …).
    pub found: std::collections::BTreeMap<String, bool>,
    pub problems: Vec<String>,
}

impl Health {
    /// Steam components the script could not find; the tab uses simpler
    /// stand-ins for them.
    pub fn missing(&self) -> Vec<&str> {
        self.found
            .iter()
            .filter(|(_, ok)| !**ok)
            .map(|(k, _)| k.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_gets_version_and_binding() {
        let s = menu_script("0.1.18");
        assert!(s.contains("const VERSION = '0.1.18';"));
        assert!(s.contains(&format!("const BINDING = '{BINDING}';")));
        assert!(!s.contains(VERSION_PLACEHOLDER));
        assert!(!s.contains(BINDING_PLACEHOLDER));
    }

    #[test]
    fn version_cannot_break_out_of_the_string() {
        let s = menu_script("1'; alert(1); '");
        assert!(s.contains("const VERSION = '1alert1';"));
    }

    #[test]
    fn calls_parse() {
        let c = Call::parse(r#"{"id":7,"method":"setVolume","params":{"level":0.5}}"#).unwrap();
        assert_eq!(c.id, 7);
        assert_eq!(c.method, "setVolume");
        assert_eq!(c.params["level"], 0.5);
        assert!(c.wants_reply());
        let log = Call::parse(r#"{"id":0,"method":"log"}"#).unwrap();
        assert!(!log.wants_reply());
        assert_eq!(log.params, Value::Null);
        assert!(Call::parse("nope").is_err());
    }

    #[test]
    fn replies_are_valid_js() {
        let ok = reply_expression(3, &Ok(json!({"a": "x\u{2028}y"})));
        assert!(ok.starts_with("window.__uc && window.__uc.reply({"), "{ok}");
        assert!(ok.contains("x\\u2028y"), "{ok}");
        assert!(!ok.contains('\u{2028}'), "{ok}");
        let back: Value = serde_json::from_str(
            ok.trim_start_matches("window.__uc && window.__uc.reply(")
                .trim_end_matches(')'),
        )
        .unwrap();
        assert_eq!(back, json!({"id": 3, "result": {"a": "x\u{2028}y"}}));
        let err = reply_expression(4, &Err("no \"audio\"".into()));
        assert_eq!(
            err,
            r#"window.__uc && window.__uc.reply({"error":"no \"audio\"","id":4})"#
        );
        assert_eq!(
            push_expression("refresh", &Value::Null),
            r#"window.__uc && window.__uc.push("refresh", null)"#
        );
    }

    #[test]
    fn health_parses_and_lists_missing_parts() {
        let h: Health = serde_json::from_value(json!({
            "version": "0.1.18",
            "webpack": true,
            "react": true,
            "hooked": true,
            "how": "QuickAccessMenu component",
            "tabShown": false,
            "lastTabRender": 0,
            "found": {"Focusable": true, "SliderField": false},
            "problems": []
        }))
        .unwrap();
        assert!(h.hooked && !h.tab_shown);
        assert_eq!(h.missing(), vec!["SliderField"]);
    }
}
