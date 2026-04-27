use anyhow::{anyhow, Result};
use std::io::{self, Write};
use tracing::info;

use crate::cli::storage;
use crate::cli::theme::Theme;

/// Run the onboarding wizard (Phase 1: stub, Phase 2+: integrate zeroclaw-tui)
pub async fn run_onboarding() -> Result<()> {
    println!();
    println!(
        "{}╔═══════════════════════════════════════════════════════════════════╗{}",
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}║  {}🎯 ZTerm Onboarding Wizard{}                                     {}║{}",
        Theme::CYAN,
        Theme::BRIGHT_BLUE,
        Theme::RESET,
        Theme::CYAN,
        Theme::RESET
    );
    println!(
        "{}╚═══════════════════════════════════════════════════════════════════╝{}",
        Theme::CYAN,
        Theme::RESET
    );
    println!();
    println!(
        "{}This wizard will set up your ZTerm configuration.{}",
        Theme::BLUE,
        Theme::RESET
    );
    println!();

    // Phase 1: Stub implementation (collect minimal info)
    // Phase 2: Will integrate zeroclaw-tui::onboarding (31 screens)

    let gateway_url = prompt_gateway_url()?;
    let gateway_token = prompt_gateway_token()?;

    // Create config file. Default model is the neutral config-key
    // `"primary"` — zterm's `/models` selector resolves this against
    // the live daemon's `[providers.models.*]` table at boot. Avoid
    // baking specific upstream model identifiers into bootstrap
    // defaults.
    let config = format!(
        r#"# ZTerm Configuration

[gateway]
url = "{}"
token = "{}"

[agent]
model = "primary"
provider = "zeroclaw"

[ui]
splash_screen = true
"#,
        gateway_url, gateway_token
    );

    storage::save_config(&config)?;
    info!("Config saved");

    println!();
    println!(
        "{}✅ Configuration saved to ~/.zeroclaw/config.toml{}",
        Theme::BRIGHT_GREEN,
        Theme::RESET
    );
    println!();
    println!(
        "{}🚀 Ready to launch ZTerm!{}",
        Theme::BRIGHT_CYAN,
        Theme::RESET
    );
    println!();

    Ok(())
}

fn prompt_gateway_url() -> Result<String> {
    print!(
        "{}🌐 Gateway URL (default: http://localhost:8888): {}",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let url = input.trim().to_string();
    if url.is_empty() {
        Ok("http://localhost:8888".to_string())
    } else {
        Ok(url)
    }
}

fn prompt_gateway_token() -> Result<String> {
    print!(
        "{}🔑 API Token (Bearer token): {}",
        Theme::BRIGHT_BLUE,
        Theme::RESET
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let token = input.trim().to_string();
    if token.is_empty() {
        Err(anyhow!("API token is required"))
    } else {
        Ok(token)
    }
}
