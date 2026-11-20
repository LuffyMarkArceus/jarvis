//! `/mode`, `/approval`, `/sandbox` and `jarvis mode` arguments, shared by
//! the CLI and the TUI (Milestone 6.6).

use serde_json::json;

/// `jarvis mode <what> <value>` as a `mode.set` change. A value alone
/// (`jarvis mode autopilot`) names its own setting.
pub fn mode_change(what: &str, value: &str) -> Result<serde_json::Value, String> {
    let value = value.trim().to_lowercase().replace('-', "_");
    let what = what.trim().to_lowercase().replace('-', "_");
    let (what, value) = match what.as_str() {
        "plan" | "interactive" | "autopilot" if value.is_empty() => ("autonomy".to_string(), what),
        "manual" | "assisted" | "allow_all" if value.is_empty() => ("approval".to_string(), what),
        _ => (what, value),
    };
    match (what.as_str(), value.as_str()) {
        ("autonomy", v @ ("plan" | "interactive" | "autopilot")) => Ok(json!({ "autonomy": v })),
        ("approval", v @ ("manual" | "assisted" | "allow_all")) => Ok(json!({ "approval": v })),
        ("sandbox", "on" | "true") => Ok(json!({ "sandbox": true })),
        ("sandbox", "off" | "false") => Ok(json!({ "sandbox": false })),
        ("autonomy", v) => Err(format!(
            "{v:?} is not an autonomy: plan, interactive or autopilot"
        )),
        ("approval", v) => Err(format!(
            "{v:?} is not an approval: manual, assisted or allow-all"
        )),
        ("sandbox", v) => Err(format!("{v:?} is not a sandbox setting: on or off")),
        (w, _) => Err(format!(
            "{w:?} is not part of a mode: autonomy, approval or sandbox"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jarvis_mode_arguments_become_a_mode_change() {
        assert_eq!(
            mode_change("approval", "assisted"),
            Ok(json!({ "approval": "assisted" }))
        );
        assert_eq!(
            mode_change("approval", "allow-all"),
            Ok(json!({ "approval": "allow_all" }))
        );
        assert_eq!(
            mode_change("autonomy", "plan"),
            Ok(json!({ "autonomy": "plan" }))
        );
        assert_eq!(
            mode_change("sandbox", "off"),
            Ok(json!({ "sandbox": false }))
        );
        assert_eq!(mode_change("sandbox", "on"), Ok(json!({ "sandbox": true })));
        // A value alone names its setting.
        assert_eq!(
            mode_change("autopilot", ""),
            Ok(json!({ "autonomy": "autopilot" }))
        );
        assert!(mode_change("sandbox", "maybe").is_err());
        assert!(mode_change("colour", "blue").is_err());
    }
}
