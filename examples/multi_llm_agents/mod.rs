mod application;
mod game;
mod http;
mod provider;

use std::time::Duration;

use lamport::{ExitReason, boot_concurrent_application};

use crate::support::wait_until_concurrent_actor_dead;

use self::{application::MultiLlmApplication, provider::ExampleConfig};

const DEFAULT_OPENAI_MODEL: &str = "gpt-5-mini-2025-08-07";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";

const ROOT_NAME: &str = "multi-llm.root";
const COORDINATOR_CHILD: &str = "coordinator";
const COORDINATOR_NAME: &str = "multi-llm.coordinator";
const PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const COORDINATOR_DEADLINE: Duration = Duration::from_secs(180);

pub(crate) fn main() {
    if let Err(error) = run() {
        eprintln!("{error}\n\n{}", usage());
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = ExampleConfig::from_env()?;

    println!(
        "Starting tic-tac-toe match with OpenAI model `{}` and Anthropic model `{}`.\n",
        config.openai.model, config.anthropic.model
    );

    let (runtime, app) = boot_concurrent_application(
        MultiLlmApplication {
            config: config.clone(),
        },
        Default::default(),
    )
    .map_err(|error| format!("boot application failed: {error:?}"))?;
    if !runtime.wait_for_idle(Some(Duration::from_secs(2))) {
        return Err("runtime did not reach idle after boot".to_owned());
    }

    let coordinator = runtime
        .resolve_name(COORDINATOR_NAME)
        .ok_or_else(|| "coordinator did not register".to_owned())?;
    wait_until_concurrent_actor_dead(&runtime, coordinator, COORDINATOR_DEADLINE)?;

    let coordinator_exit = runtime
        .actor_snapshot(coordinator)
        .and_then(|snapshot| snapshot.metrics.last_exit)
        .ok_or_else(|| "coordinator exit reason was not retained".to_owned())?;

    let _ = runtime.exit_actor(app.root_supervisor(), ExitReason::Shutdown);
    let _ = runtime.wait_for_idle(Some(Duration::from_secs(2)));

    match coordinator_exit {
        ExitReason::Normal => Ok(()),
        other => Err(format!("coordinator exited abnormally: {other}")),
    }
}

fn usage() -> String {
    format!(
        "Usage:\n  OPENAI_API_KEY=... ANTHROPIC_API_KEY=... cargo run --example multi_llm_agents\n\nOptional environment variables:\n  OPENAI_MODEL (default: {DEFAULT_OPENAI_MODEL})\n  ANTHROPIC_MODEL (default: {DEFAULT_ANTHROPIC_MODEL})"
    )
}
