use agent_guard::audit::AuditStore;
use agent_guard::claude::{self, Scope};
use agent_guard::codex;
use agent_guard::compatible::{self, Agent as CompatibleAgent};
use agent_guard::core::{Action, CanonicalEvent};
use agent_guard::cursor;
use agent_guard::gemini;
use agent_guard::kiro;
use agent_guard::opencode;
use agent_guard::paths;
use agent_guard::policy::PolicyEngine;
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "agent-guard",
    version,
    about = "Local AI agent policy enforcement and audit"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Setup(InstallArgs),
    Install(InstallArgs),
    Uninstall(InstallArgs),
    Detect,
    Status(StatusArgs),
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
}

#[derive(Args)]
struct InstallArgs {
    #[arg(long, default_value = "claude")]
    agent: String,
    #[arg(long, value_enum, default_value = "project")]
    scope: ScopeArg,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum ScopeArg {
    Project,
    User,
}

impl From<ScopeArg> for Scope {
    fn from(value: ScopeArg) -> Self {
        match value {
            ScopeArg::Project => Scope::Project,
            ScopeArg::User => Scope::User,
        }
    }
}

#[derive(Args)]
struct StatusArgs {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
}

#[derive(Subcommand)]
enum PolicyCommand {
    Validate {
        path: PathBuf,
    },
    Test {
        #[arg(long)]
        event: PathBuf,
        #[arg(long)]
        policy: Option<PathBuf>,
    },
    Explain {
        #[arg(long)]
        event: PathBuf,
        #[arg(long)]
        policy: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    Findings {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Export {
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum HookCommand {
    Dispatch {
        #[arg(long)]
        agent: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        policy: Option<PathBuf>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("agent-guard: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Setup(args) | Command::Install(args) => install(args),
        Command::Uninstall(args) => uninstall(args),
        Command::Detect => detect(),
        Command::Status(args) => status(args),
        Command::Policy { command } => policy_command(command),
        Command::Audit { command } => audit_command(command),
        Command::Hook { command } => hook_command(command),
    }
}

fn install(args: InstallArgs) -> Result<()> {
    let workspace = canonical_workspace(&args.workspace)?;
    let executable = std::env::current_exe()?;
    let path = match args.agent.as_str() {
        "claude" => claude::install_hook(args.scope.into(), &workspace, &executable)?,
        "codebuddy" | "codebuddy-code" => compatible::install_hook(
            CompatibleAgent::CodeBuddy,
            args.scope.into(),
            &workspace,
            &executable,
        )?,
        "codex" => codex::install_hook(args.scope.into(), &workspace, &executable)?,
        "cursor" => cursor::install_hook(args.scope.into(), &workspace, &executable)?,
        "gemini" => gemini::install_hook(args.scope.into(), &workspace, &executable)?,
        "kiro" | "kiro-cli" =>
            kiro::install_hook(args.scope.into(), &workspace, &executable)?,
        "opencode" => opencode::install_plugin(args.scope.into(), &workspace, &executable)?,
        "qoder" | "qoder-cli" => compatible::install_hook(
            CompatibleAgent::Qoder,
            args.scope.into(),
            &workspace,
            &executable,
        )?,
        "qwen" | "qwen-code" | "qwen-code-cli" => compatible::install_hook(
            CompatibleAgent::Qwen,
            args.scope.into(),
            &workspace,
            &executable,
        )?,
        _ => bail!("supported agents: claude, codebuddy, codex, cursor, gemini, kiro, opencode, qoder, qwen"),
    };
    println!("installed {} integration in {}", args.agent, path.display());
    println!("mode is controlled by the active policy; built-in rules default to observe");
    Ok(())
}

fn uninstall(args: InstallArgs) -> Result<()> {
    let workspace = canonical_workspace(&args.workspace)?;
    let path = match args.agent.as_str() {
        "claude" => claude::uninstall_hook(args.scope.into(), &workspace)?,
        "codebuddy" | "codebuddy-code" => compatible::uninstall_hook(
            CompatibleAgent::CodeBuddy,
            args.scope.into(),
            &workspace,
        )?,
        "codex" => codex::uninstall_hook(args.scope.into(), &workspace)?,
        "cursor" => cursor::uninstall_hook(args.scope.into(), &workspace)?,
        "gemini" => gemini::uninstall_hook(args.scope.into(), &workspace)?,
        "kiro" | "kiro-cli" => kiro::uninstall_hook(args.scope.into(), &workspace)?,
        "opencode" => opencode::uninstall_plugin(args.scope.into(), &workspace)?,
        "qoder" | "qoder-cli" => compatible::uninstall_hook(
            CompatibleAgent::Qoder,
            args.scope.into(),
            &workspace,
        )?,
        "qwen" | "qwen-code" | "qwen-code-cli" => compatible::uninstall_hook(
            CompatibleAgent::Qwen,
            args.scope.into(),
            &workspace,
        )?,
        _ => bail!("supported agents: claude, codebuddy, codex, cursor, gemini, kiro, opencode, qoder, qwen"),
    };
    println!(
        "removed Agent Guard {} integration from {}",
        args.agent,
        path.display()
    );
    Ok(())
}

fn detect() -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let claude_config = home
        .as_ref()
        .map(|home| home.join(".claude"))
        .map(|path| path.exists())
        .unwrap_or(false);
    let opencode_config = home
        .as_ref()
        .map(|home| home.join(".config/opencode"))
        .map(|path| path.exists())
        .unwrap_or(false);
    let codex_config = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".codex")))
        .map(|path| path.exists())
        .unwrap_or(false);
    let cursor_config = home
        .as_ref()
        .map(|home| home.join(".cursor"))
        .map(|path| path.exists())
        .unwrap_or(false);
    let gemini_config = home
        .as_ref()
        .map(|home| home.join(".gemini"))
        .map(|path| path.exists())
        .unwrap_or(false);
    let codebuddy_config = home
        .as_ref()
        .map(|home| home.join(".codebuddy"))
        .map(|path| path.exists())
        .unwrap_or(false);
    let kiro_config = home
        .as_ref()
        .map(|home| home.join(".kiro"))
        .map(|path| path.exists())
        .unwrap_or(false);
    let qoder_config = home
        .as_ref()
        .map(|home| home.join(".qoder"))
        .map(|path| path.exists())
        .unwrap_or(false);
    let qwen_config = home
        .as_ref()
        .map(|home| home.join(".qwen"))
        .map(|path| path.exists())
        .unwrap_or(false);
    println!(
        "claude\t{}",
        if command_on_path("claude") || claude_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "codebuddy\t{}",
        if command_on_path("codebuddy") || codebuddy_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "codex\t{}",
        if command_on_path("codex") || codex_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "cursor\t{}",
        if command_on_path("cursor") || cursor_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "gemini\t{}",
        if command_on_path("gemini") || gemini_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "kiro\t{}",
        if command_on_path("kiro-cli") || kiro_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "opencode\t{}",
        if command_on_path("opencode") || opencode_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "qoder\t{}",
        if command_on_path("qoder") || qoder_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    println!(
        "qwen\t{}",
        if command_on_path("qwen") || qwen_config {
            "detected"
        } else {
            "not-detected"
        }
    );
    Ok(())
}

fn status(args: StatusArgs) -> Result<()> {
    let workspace = canonical_workspace(&args.workspace)?;
    let project = claude::hook_installed(Scope::Project, &workspace)?;
    let user = claude::hook_installed(Scope::User, &workspace)?;
    let codebuddy_project =
        compatible::hook_installed(CompatibleAgent::CodeBuddy, Scope::Project, &workspace)?;
    let codebuddy_user =
        compatible::hook_installed(CompatibleAgent::CodeBuddy, Scope::User, &workspace)?;
    let codex_project = codex::hook_installed(Scope::Project, &workspace)?;
    let codex_user = codex::hook_installed(Scope::User, &workspace)?;
    let cursor_project = cursor::hook_installed(Scope::Project, &workspace)?;
    let cursor_user = cursor::hook_installed(Scope::User, &workspace)?;
    let gemini_project = gemini::hook_installed(Scope::Project, &workspace)?;
    let gemini_user = gemini::hook_installed(Scope::User, &workspace)?;
    let kiro_project = kiro::hook_installed(Scope::Project, &workspace)?;
    let kiro_user = kiro::hook_installed(Scope::User, &workspace)?;
    let opencode_project = opencode::plugin_installed(Scope::Project, &workspace)?;
    let opencode_user = opencode::plugin_installed(Scope::User, &workspace)?;
    let qoder_project =
        compatible::hook_installed(CompatibleAgent::Qoder, Scope::Project, &workspace)?;
    let qoder_user = compatible::hook_installed(CompatibleAgent::Qoder, Scope::User, &workspace)?;
    let qwen_project =
        compatible::hook_installed(CompatibleAgent::Qwen, Scope::Project, &workspace)?;
    let qwen_user = compatible::hook_installed(CompatibleAgent::Qwen, Scope::User, &workspace)?;
    let policy = load_policy(&workspace, None)?;
    let db_path = paths::data_dir()?.join("audit.db");
    let events = if db_path.exists() {
        AuditStore::open(&db_path)?.event_count()?
    } else {
        0
    };
    println!("Claude project hook: {}", state(project));
    println!("Claude user hook:    {}", state(user));
    println!("CodeBuddy project:   {}", state(codebuddy_project));
    println!("CodeBuddy user:      {}", state(codebuddy_user));
    println!("Codex project hook:  {}", state(codex_project));
    println!("Codex user hook:     {}", state(codex_user));
    println!("Cursor project hook: {}", state(cursor_project));
    println!("Cursor user hook:    {}", state(cursor_user));
    println!("Gemini project hook: {}", state(gemini_project));
    println!("Gemini user hook:    {}", state(gemini_user));
    println!("Kiro project hook:   {}", state(kiro_project));
    println!("Kiro user hook:      {}", state(kiro_user));
    println!("OpenCode project:    {}", state(opencode_project));
    println!("OpenCode user:       {}", state(opencode_user));
    println!("Qoder project hook:  {}", state(qoder_project));
    println!("Qoder user hook:     {}", state(qoder_user));
    println!("Qwen project hook:   {}", state(qwen_project));
    println!("Qwen user hook:      {}", state(qwen_user));
    println!("Policy rules:        {}", policy.rule_count());
    println!("Audit events:        {events}");
    println!("Audit database:      {}", db_path.display());
    Ok(())
}

fn policy_command(command: PolicyCommand) -> Result<()> {
    match command {
        PolicyCommand::Validate { path } => {
            let engine = PolicyEngine::from_path(&path)?;
            println!("valid policy: {} rules", engine.rule_count());
            Ok(())
        }
        PolicyCommand::Test { event, policy } | PolicyCommand::Explain { event, policy } => {
            let workspace = std::env::current_dir()?;
            let canonical = read_canonical_or_claude_event(&event)?;
            let engine = load_policy(&workspace, policy.as_deref())?;
            let decision = engine.evaluate(&canonical)?;
            println!("{}", serde_json::to_string_pretty(&decision)?);
            if decision.action == Action::Deny {
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

fn audit_command(command: AuditCommand) -> Result<()> {
    let path = paths::data_dir()?.join("audit.db");
    let store = AuditStore::open(&path)?;
    match command {
        AuditCommand::Findings { limit } => {
            for finding in store.recent_findings(limit)? {
                println!("{}", serde_json::to_string(&finding)?);
            }
        }
        AuditCommand::Export { output } => {
            let mut file = fs::File::create(&output)?;
            for event in store.export_events()? {
                serde_json::to_writer(&mut file, &event)?;
                file.write_all(b"\n")?;
            }
            println!("exported audit events to {}", output.display());
        }
    }
    Ok(())
}

fn hook_command(command: HookCommand) -> Result<()> {
    match command {
        HookCommand::Dispatch {
            agent,
            workspace,
            policy,
        } => {
            let workspace = canonical_workspace(&workspace)?;
            let payload = read_stdin_json()?;
            let event_name = payload
                .get("hook_event_name")
                .and_then(Value::as_str)
                .unwrap_or("PreToolUse")
                .to_owned();
            let event = match agent.as_str() {
                "claude" => claude::normalize_event(payload)?,
                "codebuddy" | "codebuddy-code" =>
                    compatible::normalize_event(CompatibleAgent::CodeBuddy, payload)?,
                "codex" => codex::normalize_event(payload)?,
                "cursor" => cursor::normalize_event(payload)?,
                "gemini" => gemini::normalize_event(payload)?,
                "kiro" | "kiro-cli" => kiro::normalize_event(payload)?,
                "opencode" => opencode::normalize_event(payload)?,
                "qoder" | "qoder-cli" =>
                    compatible::normalize_event(CompatibleAgent::Qoder, payload)?,
                "qwen" | "qwen-code" | "qwen-code-cli" =>
                    compatible::normalize_event(CompatibleAgent::Qwen, payload)?,
                _ => bail!("supported agents: claude, codebuddy, codex, cursor, gemini, kiro, opencode, qoder, qwen"),
            };
            let engine = load_policy(&workspace, policy.as_deref())?;
            let decision = engine.evaluate(&event)?;
            record_best_effort(&event, &decision);
            if decision.action == Action::Deny {
                if agent != "claude" {
                    eprintln!(
                        "{}",
                        decision
                            .reason()
                            .unwrap_or_else(|| "Blocked by Agent Guard".to_owned())
                    );
                    std::process::exit(2);
                }
                if let Some(response) = claude::render_decision(&decision, &event_name) {
                    println!("{}", serde_json::to_string(&response)?);
                }
            }
            Ok(())
        }
    }
}

fn load_policy(workspace: &Path, explicit: Option<&Path>) -> Result<PolicyEngine> {
    match paths::policy_path(workspace, explicit)? {
        Some(path) => PolicyEngine::from_path(&path),
        None => PolicyEngine::default_policy(),
    }
}

fn record_best_effort(event: &CanonicalEvent, decision: &agent_guard::core::Decision) {
    let result = paths::data_dir()
        .map(|dir| dir.join("audit.db"))
        .and_then(|path| AuditStore::open(&path))
        .and_then(|mut store| store.record(event, decision));
    if let Err(error) = result {
        eprintln!("agent-guard audit warning: {error:#}");
    }
}

fn read_stdin_json() -> Result<Value> {
    let mut input = String::new();
    io::stdin().take(1024 * 1024).read_to_string(&mut input)?;
    if input.len() >= 1024 * 1024 {
        bail!("hook payload exceeds the 1 MiB limit");
    }
    serde_json::from_str(&input).context("invalid hook JSON on stdin")
}

fn read_canonical_or_claude_event(path: &Path) -> Result<CanonicalEvent> {
    let text = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)?;
    if value.get("schema_version").is_some() {
        Ok(serde_json::from_value(value)?)
    } else {
        claude::normalize_event(value)
    }
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("workspace {} does not exist", path.display()))
}

fn command_on_path(command: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join(command))
                .any(|path| path.is_file())
        })
        .unwrap_or(false)
}

fn state(enabled: bool) -> &'static str {
    if enabled {
        "installed"
    } else {
        "not-installed"
    }
}
