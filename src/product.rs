//! Single source of truth for Model Rocket's externally visible product contract.

use std::io::{self, Write};

use crate::domain::ModelCatalogue;

pub const MANAGED_ACCOUNT_TYPE: &str = "chatgpt";
pub const CODEX_CLI_VERSION_OUTPUT: &str = "codex-cli 0.153.4";
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-fable-5";
pub const DEFAULT_AVAILABLE_CLAUDE_MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];
pub const CODEX_BASE_INSTRUCTIONS: &str =
    "Act only as the model inside Claude Code and use only host-supplied dynamic tools.";
pub const CODEX_DEVELOPER_GUARD: &str = "Act as the model inside Claude Code. Respond with assistant text or the supplied dynamic tools only. Never invoke Codex built-in shell, file, web, MCP, collaboration, or user-input tools.";
pub const TRUSTED_CHILD_PATH: &str = "/usr/bin:/bin";
pub const BRIDGE_STARTUP_ATTEMPTS: u16 = 300;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const CODEX_NATIVE_SHA256: &str =
    "b973d440acac501fd2594a43e7ca9ce41e0a65b9dfb28d0d7a7837c99e1261e3";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const CODEX_NATIVE_SHA256: &str =
    "88ecd2cbf8044832a49e7710394d9d328f7205fa5e8c8ebbdd015e002b4f6e21";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub const CODEX_NATIVE_SHA256: &str =
    "4d76e542c222ea8c75861d8c4ade60a1a332a63255ce1c60bdaebf7c2a2869e6";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const CODEX_NATIVE_SHA256: &str =
    "56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da";

#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
compile_error!("Model Rocket has no pinned native vendor digests for this target");

/// Writes the shell-facing product contract consumed by the launcher.
///
/// # Errors
///
/// Returns the output writer's error when the contract cannot be written completely.
pub fn write_launcher_contract(
    catalogue: &ModelCatalogue,
    mut output: impl Write,
) -> io::Result<()> {
    let canonical = catalogue.canonical_route();
    for (key, value) in [
        ("default_claude_model", DEFAULT_CLAUDE_MODEL),
        ("canonical_gpt_model", canonical.claude_model.as_str()),
    ] {
        writeln!(output, "{key}={value}")?;
    }
    writeln!(
        output,
        "gpt_context_tokens={}",
        catalogue.minimum_context_tokens().get()
    )?;
    writeln!(output, "bridge_startup_attempts={BRIDGE_STARTUP_ATTEMPTS}")
}

/// Writes the Claude Code worktree-agent definition for the canonical GPT route.
///
/// # Errors
///
/// Returns the output writer's error when the agent definition cannot be written completely.
pub fn write_worker_config(catalogue: &ModelCatalogue, mut output: impl Write) -> io::Result<()> {
    writeln!(output, "---")?;
    writeln!(output, "name: gpt-worktree-worker")?;
    writeln!(
        output,
        "description: Use proactively for self-contained coding tasks that should run with GPT in an isolated git worktree."
    )?;
    writeln!(
        output,
        "model: {}",
        catalogue.canonical_route().claude_model.as_str()
    )?;
    writeln!(output, "isolation: worktree")?;
    writeln!(output, "---\n")?;
    writeln!(
        output,
        "Implement the delegated task in the isolated worktree."
    )?;
    writeln!(
        output,
        "Follow all repository instructions and quality gates."
    )?;
    writeln!(
        output,
        "Return a concise summary of the changes, verification evidence, and any blockers."
    )
}
