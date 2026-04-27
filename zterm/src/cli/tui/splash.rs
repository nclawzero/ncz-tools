use crate::cli::theme::Theme;

/// Display ZTerm splash screen on startup
pub fn display_splash(session_name: &str, gateway_url: &str, model: &str, provider: &str) {
    println!();
    println!(
        "{}╔════════════════════════════════════════════════════════════════════╗{}",
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}║{}                                                                    {}║{}",
        Theme::CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}║  {}✨ Welcome to ZTerm v{} ✨{}                     {}║{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        env!("CARGO_PKG_VERSION"),
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}║{}                                                                    {}║{}",
        Theme::CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}║  {}Terminal REPL for Zeroclaw Gateway{}            {}║{}",
        Theme::CYAN,
        Theme::BRIGHT_CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}║{}                                                                    {}║{}",
        Theme::CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}╚════════════════════════════════════════════════════════════════════╝{}",
        Theme::CYAN,
        Theme::RESET
    );
    println!();

    // Session information
    println!(
        "{}┌─ {}Session Information{}{}───────────────────────────────────────────┐{}",
        Theme::BLUE,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::BLUE,
        Theme::RESET
    );
    println!(
        "{}│ {}Session{}:  {:48} {}│{}",
        Theme::BLUE,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        session_name,
        Theme::BLUE,
        Theme::RESET
    );
    println!(
        "{}│ {}Gateway{}:  {:48} {}│{}",
        Theme::BLUE,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        gateway_url,
        Theme::BLUE,
        Theme::RESET
    );
    println!(
        "{}│ {}Model{}:    {:48} {}│{}",
        Theme::BLUE,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        &format!("{} ({})", model, provider),
        Theme::BLUE,
        Theme::RESET
    );
    println!(
        "{}└─────────────────────────────────────────────────────────────────┘{}",
        Theme::BLUE,
        Theme::RESET
    );
    println!();

    // Quick help
    println!(
        "{}╭─ {}Quick Help{}{}────────────────────────────────────────────────╮{}",
        Theme::CYAN,
        Theme::BRIGHT_CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│{}                                                                 {}│{}",
        Theme::CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  💬 {}Chat{}:        Type your message and press Enter              {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  ❓ {}Help{}:        /help              (show all commands)         {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  🤖 {}Models{}:      /models list       (view available models)     {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  📋 {}Sessions{}:    /session list      (view all sessions)         {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  📝 {}History{}:     /history           (show conversation)         {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  🧠 {}Memory{}:      /memory <query>    (search your memory)        {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  🚀 {}Skills{}:      /skills list       (view available skills)     {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  ⏰ {}Cron{}:        /cron list         (scheduled tasks)           {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│  🚪 {}Exit{}:        /exit              (exit gracefully)            {}│{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}│{}                                                                 {}│{}",
        Theme::CYAN,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}╰─────────────────────────────────────────────────────────────────╯{}",
        Theme::CYAN,
        Theme::RESET
    );
    println!();

    // Tip
    println!(
        "{}💡 {}Tip{}: Type /help anytime for a complete command reference{}",
        Theme::BRIGHT_CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::RESET
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_splash_display() {
        // Just verify that display functions don't panic. Args use
        // neutral config-key strings rather than vendor model names.
        display_splash("main", "http://localhost:8888", "primary", "zeroclaw");
    }
}
