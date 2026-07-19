//! Terminal capability environment shared by local panes, SSH shells, and remote sessions.

use std::collections::BTreeMap;

pub const TERM_VAR: &str = "TERM";
pub const TERM_VALUE: &str = "xterm-256color";
pub const COLORTERM_VAR: &str = "COLORTERM";
pub const COLORTERM_VALUE: &str = "truecolor";

/// Defaults for processes running under Potty. Callers should merge user/profile environment after
/// this if explicit config needs to override these hints.
pub fn defaults() -> BTreeMap<String, String> {
    BTreeMap::from([
        (TERM_VAR.to_string(), TERM_VALUE.to_string()),
        (COLORTERM_VAR.to_string(), COLORTERM_VALUE.to_string()),
    ])
}
