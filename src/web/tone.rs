//! One state-to-tone mapping for chips, dots and timelines (the UI library, M15). Pages and scripts
//! never pick a state's colour themselves: "running" was green in Settings and violet on the
//! Machine page, and open questions red on Projects and amber on a project's overview (N-19).

/// The tone of a state word: "" (neutral), "accent", "ok", "warn", "bad", "info" or "quiet", the
/// `.chip` and `.dot` classes of static/ui.css.
pub fn tone(state: &str) -> &'static str {
    match state {
        // Under way
        "doing" | "running" | "live" | "active" | "approved" => "accent",
        // Finished well
        "done" | "merged" | "works" | "tested" | "built" | "ready" | "answered" | "current" | "concluded"
        | "closed" => "ok",
        // Waiting, on you or on something
        "waiting" | "open" | "review" | "to test" | "paused" | "proposed" => "warn",
        // Went wrong or stuck
        "blocked" | "failed" | "problem" | "conflict" | "failing" | "urgent" => "bad",
        // New, or not final
        "draft" | "new" => "info",
        // Out of the way
        "parked" | "skipped" | "archived" | "replaced" | "cancelled" => "quiet",
        // todo, planned, queued and anything else
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::tone;

    #[test]
    fn a_state_has_one_tone_everywhere() {
        assert_eq!(tone("running"), "accent");
        assert_eq!(tone("open"), "warn", "questions waiting are amber on every page");
        assert_eq!(tone("blocked"), "bad");
        assert_eq!(tone("done"), "ok");
        assert_eq!(tone("parked"), "quiet");
        assert_eq!(tone("todo"), "");
        assert_eq!(tone("queued"), "");
    }
}
