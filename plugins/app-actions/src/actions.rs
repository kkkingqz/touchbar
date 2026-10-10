//! Shared shortcut dispatch for independently instantiated sets.
use touchbar_logic_sdk::engine::ENGINE;
pub const BROWSER: &[&str] = &["back", "forward", "reload"];
pub const FORMATTING: &[&str] = &["bold", "italic", "underline"];
pub fn allowed(config: &str, action: &str) -> bool {
    match config {
        "browser" => BROWSER,
        "formatting" => FORMATTING,
        _ => &[],
    }
    .contains(&action)
}
pub fn press(config: &str, action: &str) {
    if allowed(config, action) {
        let _ = ENGINE.keyboard().shortcut(action);
    }
}
