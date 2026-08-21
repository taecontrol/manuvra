use clap::{Args, Parser, Subcommand, ValueEnum};
use manuvra_cli::{
    ClientError, Installation, chrome_launch, daemon_status, daemon_stop, invoke_daemon,
    legacy_config_root, migrate_legacy, purge_owned_roots,
};
use manuvra_protocol::{
    AGENT_HELP, Invocation, RpcResponse, command_default_timeout_ms, command_descriptor,
    command_help, encode_operational_line, error_meta, operational_error, registry_page,
    schema_pointer, validate_command_input, validate_command_result,
};
use rand::Rng;
use rand::distr::Alphanumeric;
use serde_json::{Map, Value, json};
use std::io::{self, IsTerminal, Write};

#[derive(Parser)]
#[command(name = "manuvra", version, long_about = AGENT_HELP)]
struct Cli {
    #[arg(long, global = true)]
    timeout_ms: Option<u64>,
    #[arg(long, global = true)]
    request_id: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Commands {
        #[command(subcommand)]
        command: CommandsCommand,
    },
    Targets {
        #[arg(long)]
        kind: Option<TargetKind>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u64,
    },
    Open {
        #[arg(long = "target")]
        target_id: String,
        #[arg(long, value_enum, default_value_t = Role::Actor)]
        role: Role,
        #[arg(long, value_enum, default_value_t = Mode::Background)]
        mode: Mode,
        #[arg(long, default_value_t = 120_000)]
        lease_ttl_ms: u64,
    },
    Close {
        #[arg(long)]
        session: String,
        #[arg(long)]
        cancel_running: bool,
    },
    Lease {
        #[arg(value_enum)]
        action: LeaseAction,
        #[arg(long)]
        session: String,
        #[arg(long)]
        ttl_ms: Option<u64>,
    },
    Click {
        #[arg(long)]
        session: String,
        #[command(flatten)]
        locator: LocatorArgs,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value_t = 1)]
        count: u64,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Type {
        #[arg(long)]
        session: String,
        #[command(flatten)]
        locator: TypeLocatorArgs,
        #[arg(long)]
        text: String,
        #[arg(long)]
        replace: bool,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Press {
        #[arg(long)]
        session: String,
        #[arg(long)]
        key: String,
        #[command(flatten)]
        locator: LocatorArgs,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Scroll {
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 0.0)]
        delta_x: f64,
        #[arg(long)]
        delta_y: f64,
        #[command(flatten)]
        locator: LocatorArgs,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Navigate {
        #[arg(long)]
        session: String,
        #[arg(long)]
        url: String,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Observe {
        #[command(subcommand)]
        command: ObserveCommand,
    },
    Raw {
        #[command(subcommand)]
        command: RawCommand,
    },
    Cancel {
        #[arg(long)]
        session: String,
        #[arg(long)]
        request_id: String,
    },
    Export {
        #[arg(long)]
        session: String,
        #[arg(long = "artifact")]
        artifact_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        destination: String,
    },
    Doctor {
        #[arg(long)]
        session: Option<String>,
        #[arg(long = "target")]
        target_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Setup {
        #[arg(long)]
        json: bool,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Chrome {
        #[command(subcommand)]
        command: ChromeCommand,
    },
    Migrate {
        #[arg(long = "from", value_enum)]
        source: LegacySource,
    },
    Purge {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    Status,
    Stop,
}

#[derive(Subcommand)]
enum ChromeCommand {
    Launch,
}

#[derive(Clone, Copy, ValueEnum)]
enum LegacySource {
    #[value(name = "computer-use")]
    ComputerUse,
}

#[derive(Subcommand)]
enum CommandsCommand {
    List {
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u64,
    },
    Get {
        command: String,
    },
    Schema {
        command: String,
        #[arg(long, value_enum)]
        side: SchemaSide,
    },
    Errors {
        code: String,
    },
    Usage {
        #[command(subcommand)]
        action: UsageAction,
    },
}

#[derive(Subcommand)]
enum UsageAction {
    Enable,
    Show,
    Export { destination: String },
    Reset,
    Disable,
}

#[derive(Subcommand)]
enum ObserveCommand {
    Screenshot {
        #[arg(long)]
        session: String,
    },
    Query {
        #[arg(long)]
        session: String,
        #[command(flatten)]
        semantic: SemanticArgs,
        #[arg(long, default_value_t = 5)]
        limit: u64,
    },
    Tree {
        #[arg(long)]
        session: String,
    },
    Logs {
        #[arg(long)]
        session: String,
    },
    Events {
        #[arg(long)]
        session: String,
    },
    Diagnostics {
        #[arg(long)]
        session: String,
    },
    Timings {
        #[arg(long)]
        session: String,
    },
    Manifest {
        #[arg(long)]
        session: String,
    },
}

#[derive(Subcommand)]
enum RawCommand {
    Cdp {
        #[arg(long)]
        session: String,
        #[arg(long, value_enum)]
        intent: RawIntent,
        #[arg(long)]
        method: String,
        #[arg(long)]
        params: String,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Ax {
        #[command(subcommand)]
        command: AxCommand,
    },
}

#[derive(Subcommand)]
enum AxCommand {
    Get {
        #[arg(long)]
        session: String,
        #[arg(long = "ref")]
        reference: String,
        #[arg(long)]
        attribute: String,
    },
    Set {
        #[arg(long)]
        session: String,
        #[arg(long = "ref")]
        reference: String,
        #[arg(long)]
        attribute: String,
        #[arg(long)]
        value: String,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Perform {
        #[arg(long)]
        session: String,
        #[arg(long = "ref")]
        reference: String,
        #[arg(long)]
        action: String,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
}

#[derive(Args, Default)]
struct LocatorArgs {
    #[command(flatten)]
    semantic: SemanticArgs,
    #[arg(long = "ref")]
    reference: Option<String>,
    #[arg(long, value_parser = parse_point)]
    point: Option<(f64, f64)>,
    #[arg(long = "frame")]
    frame_token: Option<String>,
}

#[derive(Args, Default)]
struct TypeLocatorArgs {
    #[arg(long)]
    role: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long = "locator-text")]
    locator_text: Option<String>,
    #[arg(long)]
    identifier: Option<String>,
    #[arg(long = "within-role")]
    within_role: Option<String>,
    #[arg(long = "within-name")]
    within_name: Option<String>,
    #[arg(long = "ref")]
    reference: Option<String>,
    #[arg(long, value_parser = parse_point)]
    point: Option<(f64, f64)>,
    #[arg(long = "frame")]
    frame_token: Option<String>,
}

impl From<TypeLocatorArgs> for LocatorArgs {
    fn from(value: TypeLocatorArgs) -> Self {
        Self {
            semantic: SemanticArgs {
                role: value.role,
                name: value.name,
                text: value.locator_text,
                identifier: value.identifier,
                within_role: value.within_role,
                within_name: value.within_name,
            },
            reference: value.reference,
            point: value.point,
            frame_token: value.frame_token,
        }
    }
}

#[derive(Args, Default)]
struct SemanticArgs {
    #[arg(long)]
    role: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    text: Option<String>,
    #[arg(long)]
    identifier: Option<String>,
    #[arg(long = "within-role")]
    within_role: Option<String>,
    #[arg(long = "within-name")]
    within_name: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum TargetKind {
    Chrome,
    Macos,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Role {
    Actor,
    Observer,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Background,
    Foreground,
}

#[derive(Clone, Copy, ValueEnum)]
enum LeaseAction {
    Acquire,
    Renew,
    Release,
}

#[derive(Clone, Copy, ValueEnum)]
enum SchemaSide {
    Input,
    Result,
}

#[derive(Clone, Copy, ValueEnum)]
enum RawIntent {
    Query,
    Action,
}

fn main() {
    let cli = Cli::parse();
    let output = diagnostic_output(&cli.command, io::stdout().is_terminal());
    let request_id = invocation_request_id(&cli.command, cli.request_id);
    let result =
        execute_special(&cli.command, request_id.clone(), cli.timeout_ms).unwrap_or_else(|| {
            match build_command(cli.command) {
                Ok(BuiltCommand::Local { id, input, value }) => {
                    debug_assert!(validate_command_input(id, &input).is_ok());
                    (value, 0)
                }
                Ok(BuiltCommand::Remote { id, input }) => invoke(
                    id,
                    input,
                    request_id,
                    cli.timeout_ms
                        .unwrap_or_else(|| command_default_timeout(id)),
                ),
                Err(error) => error.into_local_reply(),
            }
        });
    emit_and_exit(result.0, result.1, output);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticOutput {
    Json,
    Doctor,
    Setup,
}

fn diagnostic_output(command: &Command, terminal: bool) -> DiagnosticOutput {
    match command {
        Command::Doctor { json: false, .. } if terminal => DiagnosticOutput::Doctor,
        Command::Setup { json: false } if terminal => DiagnosticOutput::Setup,
        _ => DiagnosticOutput::Json,
    }
}

fn execute_special(
    command: &Command,
    request_id: String,
    timeout_ms: Option<u64>,
) -> Option<(Value, i32)> {
    match command {
        Command::Daemon {
            command: DaemonCommand::Status,
        } => Some(control_result(daemon_status())),
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => Some(control_result(daemon_stop())),
        Command::Chrome {
            command: ChromeCommand::Launch,
        } => Some(run_chrome_launch(timeout_ms.unwrap_or_else(|| {
            command_default_timeout("system.chrome.launch")
        }))),
        Command::Setup { .. } => Some(run_setup(
            request_id,
            timeout_ms.unwrap_or_else(|| command_default_timeout("system.setup")),
        )),
        Command::Migrate { .. } => Some(
            migrate_legacy()
                .map(|value| (value, 0))
                .unwrap_or_else(|message| local_error("invalid_request", &message)),
        ),
        Command::Purge { all, yes } => Some(run_purge(*all, *yes)),
        _ => None,
    }
}

fn control_result(result: Result<Value, ClientError>) -> (Value, i32) {
    match result {
        Ok(value) => (value, 0),
        Err(ClientError::Control(mut error, daemon)) => {
            let sessions = daemon["active_sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|session| session["session_id"].as_str())
                .collect::<Vec<_>>()
                .join(",");
            if !sessions.is_empty() {
                error.message = format!("{} Active sessions: {sessions}", error.message)
                    .chars()
                    .take(256)
                    .collect();
            }
            let exit = error_meta(&error.code).map(|meta| meta.exit).unwrap_or(70);
            (json!({"error": error}), exit)
        }
        Err(error) => local_error("internal_error", &error.to_string()),
    }
}

fn run_setup(request_id: String, timeout_ms: u64) -> (Value, i32) {
    invoke("system.setup", json!({}), request_id, timeout_ms)
}

fn run_chrome_launch(timeout_ms: u64) -> (Value, i32) {
    match chrome_launch(std::time::Duration::from_millis(timeout_ms)) {
        Ok(value) => match validate_command_result("system.chrome.launch", &value) {
            Ok(()) => (value, 0),
            Err(message) => local_error("internal_error", &message),
        },
        Err(error) => local_error(error.catalog_code(), error.message()),
    }
}

fn run_purge(all: bool, yes: bool) -> (Value, i32) {
    if !all {
        return local_error("invalid_request", "purge requires --all");
    }
    if !yes {
        if !io::stdin().is_terminal() {
            return local_error("invalid_request", "non-interactive purge requires --yes");
        }
        eprint!("Remove Manuvra-owned current-user configuration and temporary state? [y/N] ");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        {
            return local_error("invalid_request", "purge was not confirmed");
        }
    }
    purge_owned_roots()
        .map(|value| (value, 0))
        .unwrap_or_else(|message| local_error("invalid_request", &message))
}

fn invocation_request_id(command: &Command, requested: Option<String>) -> String {
    if matches!(command, Command::Cancel { .. }) {
        new_request_id()
    } else {
        requested.unwrap_or_else(new_request_id)
    }
}

enum BuiltCommand {
    Local {
        id: &'static str,
        input: Value,
        value: Value,
    },
    Remote {
        id: &'static str,
        input: Value,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum BuildError {
    InvalidRequest(String),
    UnknownCommand,
}

impl BuildError {
    fn into_local_reply(self) -> (Value, i32) {
        match self {
            Self::InvalidRequest(message) => local_error("invalid_request", &message),
            Self::UnknownCommand => {
                let (error, exit) = operational_error("unknown_command", None);
                (json!({"error": error}), exit)
            }
        }
    }
}

impl From<String> for BuildError {
    fn from(message: String) -> Self {
        Self::InvalidRequest(message)
    }
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => f.write_str(message),
            Self::UnknownCommand => f.write_str("unregistered command identity"),
        }
    }
}

fn build_command(command: Command) -> Result<BuiltCommand, BuildError> {
    match command {
        Command::Commands { command } => build_commands(command),
        Command::Observe { command } => build_observe(command).map_err(BuildError::from),
        Command::Raw { command } => build_raw(command).map_err(BuildError::from),
        command @ (Command::Click { .. }
        | Command::Type { .. }
        | Command::Press { .. }
        | Command::Scroll { .. }
        | Command::Navigate { .. }) => build_action(command).map_err(BuildError::from),
        command => build_direct(command).map_err(BuildError::from),
    }
}

fn build_direct(command: Command) -> Result<BuiltCommand, String> {
    match command {
        Command::Targets {
            kind,
            cursor,
            limit,
        } => Ok(remote(
            "target.list",
            optional_pairs([
                (
                    "kind",
                    kind.map(|value| Value::String(target_kind(value).to_owned())),
                ),
                ("cursor", cursor.map(Value::String)),
                ("limit", Some(Value::from(limit))),
            ]),
        )),
        Command::Open {
            target_id,
            role,
            mode,
            lease_ttl_ms,
        } => Ok(remote(
            "session.open",
            json!({
                "target_id": target_id, "role": role_name(role), "mode": mode_name(mode), "lease_ttl_ms": lease_ttl_ms,
            }),
        )),
        Command::Close {
            session,
            cancel_running,
        } => Ok(remote(
            "session.close",
            json!({
                "session_id": session, "cancel_running": cancel_running,
            }),
        )),
        Command::Lease {
            action,
            session,
            ttl_ms,
        } => Ok(remote(
            "lease.manage",
            object_with_optional(
                json!({"session_id": session, "action": lease_action(action)}),
                "ttl_ms",
                ttl_ms.map(Value::from),
            ),
        )),
        Command::Cancel {
            session,
            request_id,
        } => Ok(remote(
            "request.cancel",
            json!({
                "session_id": session, "request_id": request_id,
            }),
        )),
        Command::Export {
            session,
            artifact_ids,
            all,
            destination,
        } => Ok(build_export(session, artifact_ids, all, destination)),
        Command::Doctor {
            session,
            target_id,
            json: _,
        } => Ok(remote(
            "system.doctor",
            optional_pairs([
                ("session_id", session.map(Value::String)),
                ("target_id", target_id.map(Value::String)),
            ]),
        )),
        command @ (Command::Daemon { .. }
        | Command::Chrome { .. }
        | Command::Setup { .. }
        | Command::Migrate { .. }
        | Command::Purge { .. }) => Ok(build_local_direct(command)),
        _ => unreachable!("routed command category"),
    }
}

fn build_local_direct(command: Command) -> BuiltCommand {
    match command {
        Command::Daemon { command } => local(daemon_command_id(command), json!({}), Value::Null),
        Command::Chrome {
            command: ChromeCommand::Launch,
        } => local("system.chrome.launch", json!({}), Value::Null),
        Command::Setup { .. } => local("system.setup", json!({}), Value::Null),
        Command::Migrate { source } => local(
            "system.migrate",
            json!({"from": legacy_source(source)}),
            Value::Null,
        ),
        Command::Purge { all, yes } => {
            local("system.purge", json!({"all": all, "yes": yes}), Value::Null)
        }
        _ => unreachable!("non-local direct command"),
    }
}

fn daemon_command_id(command: DaemonCommand) -> &'static str {
    match command {
        DaemonCommand::Status => "daemon.status",
        DaemonCommand::Stop => "daemon.stop",
    }
}

fn build_action(command: Command) -> Result<BuiltCommand, String> {
    match command {
        Command::Click {
            session,
            locator,
            button,
            count,
            mode,
        } => action_with_locator(
            "action.click",
            session,
            locator,
            true,
            json!({"button": button, "count": count}),
            mode,
        ),
        Command::Type {
            session,
            locator,
            text,
            replace,
            mode,
        } => action_with_locator(
            "action.type",
            session,
            locator.into(),
            true,
            json!({"text": text, "replace": replace}),
            mode,
        ),
        Command::Press {
            session,
            key,
            locator,
            mode,
        } => action_with_locator(
            "action.press",
            session,
            locator,
            false,
            json!({"key": key}),
            mode,
        ),
        Command::Scroll {
            session,
            delta_x,
            delta_y,
            locator,
            mode,
        } => action_with_locator(
            "action.scroll",
            session,
            locator,
            false,
            json!({"delta_x": delta_x, "delta_y": delta_y}),
            mode,
        ),
        Command::Navigate { session, url, mode } => Ok(remote(
            "action.navigate",
            mode_input(json!({"session_id": session, "url": url}), mode),
        )),
        _ => unreachable!("routed action command"),
    }
}

fn build_export(
    session: String,
    artifact_ids: Vec<String>,
    all: bool,
    destination: String,
) -> BuiltCommand {
    let mut value = json!({"session_id": session, "destination": destination});
    if all {
        value["all"] = Value::Bool(true);
    }
    if !artifact_ids.is_empty() {
        value["artifact_ids"] = json!(artifact_ids);
    }
    remote("artifact.export", value)
}

fn build_commands(command: CommandsCommand) -> Result<BuiltCommand, BuildError> {
    match command {
        CommandsCommand::List { cursor, limit } => {
            build_command_list(cursor, limit).map_err(BuildError::from)
        }
        CommandsCommand::Get { command } => {
            let value = registered_help(&command)?;
            Ok(local(
                "system.commands.get",
                json!({"command": command}),
                value,
            ))
        }
        CommandsCommand::Schema { command, side } => build_command_schema(&command, side),
        CommandsCommand::Errors { code } => build_command_error(&code).map_err(BuildError::from),
        CommandsCommand::Usage { action } => Ok(build_usage(action)),
    }
}

fn build_command_list(cursor: Option<String>, limit: u64) -> Result<BuiltCommand, String> {
    if !(1..=10).contains(&limit) {
        return Err("limit must be between 1 and 10".to_owned());
    }
    let input = optional_pairs([
        ("cursor", cursor.clone().map(Value::String)),
        ("limit", Some(Value::from(limit))),
    ]);
    let cursor = cursor
        .as_deref()
        .unwrap_or("0")
        .parse::<usize>()
        .map_err(|_| "invalid cursor".to_owned())?;
    Ok(local(
        "system.commands.list",
        input,
        registry_page(cursor, limit as usize),
    ))
}

fn registered_command(id: &str) -> Result<&'static Value, BuildError> {
    command_descriptor(id).ok_or(BuildError::UnknownCommand)
}

fn registered_help(id: &str) -> Result<Value, BuildError> {
    registered_command(id)?;
    command_help(id).ok_or_else(|| BuildError::from("invalid installed command help".to_owned()))
}

fn build_command_schema(command: &str, side: SchemaSide) -> Result<BuiltCommand, BuildError> {
    let descriptor = registered_command(command)?;
    let (key, side_name) = match side {
        SchemaSide::Input => ("input_schema", "input"),
        SchemaSide::Result => ("result_schema", "result"),
    };
    let reference = descriptor[key]
        .as_str()
        .ok_or_else(|| BuildError::from("invalid installed schema pointer".to_owned()))?;
    let value = schema_pointer(reference).map_err(|error| error.to_string())?;
    Ok(local(
        "system.commands.schema",
        json!({"command": command, "side": side_name}),
        value,
    ))
}

fn build_command_error(code: &str) -> Result<BuiltCommand, String> {
    let meta = error_meta(code).ok_or_else(|| format!("unknown error {code}"))?;
    Ok(local(
        "system.commands.errors",
        json!({"code": code}),
        json!({
            "code": meta.code, "meaning": meta.meaning, "effects": meta.effects,
            "retry": meta.retry, "recovery": meta.recovery,
        }),
    ))
}

fn build_usage(action: UsageAction) -> BuiltCommand {
    let input = match action {
        UsageAction::Enable => json!({"action": "enable"}),
        UsageAction::Show => json!({"action": "show"}),
        UsageAction::Export { destination } => {
            json!({"action": "export", "destination": destination})
        }
        UsageAction::Reset => json!({"action": "reset"}),
        UsageAction::Disable => json!({"action": "disable"}),
    };
    remote("system.commands.usage", input)
}

fn build_observe(command: ObserveCommand) -> Result<BuiltCommand, String> {
    match command {
        ObserveCommand::Screenshot { session } => {
            Ok(remote("observe.screenshot", json!({"session_id": session})))
        }
        ObserveCommand::Query {
            session,
            semantic,
            limit,
        } => Ok(remote(
            "observe.query",
            json!({
                "session_id": session, "semantic": semantic_locator(&semantic)?, "limit": limit,
            }),
        )),
        ObserveCommand::Tree { session } => {
            Ok(remote("observe.tree", json!({"session_id": session})))
        }
        ObserveCommand::Logs { session } => Ok(evidence(session, "logs")),
        ObserveCommand::Events { session } => Ok(evidence(session, "events")),
        ObserveCommand::Diagnostics { session } => Ok(evidence(session, "diagnostics")),
        ObserveCommand::Timings { session } => Ok(evidence(session, "timings")),
        ObserveCommand::Manifest { session } => Ok(evidence(session, "manifest")),
    }
}

fn build_raw(command: RawCommand) -> Result<BuiltCommand, String> {
    match command {
        RawCommand::Cdp {
            session,
            intent,
            method,
            params,
            mode,
        } => {
            let params = serde_json::from_str::<Value>(&params)
                .map_err(|error| format!("invalid params JSON: {error}"))?;
            Ok(remote(
                "raw.cdp",
                mode_input(
                    json!({
                        "session_id": session, "intent": raw_intent(intent), "method": method, "params": params,
                    }),
                    mode,
                ),
            ))
        }
        RawCommand::Ax { command } => build_ax(command),
    }
}

fn build_ax(command: AxCommand) -> Result<BuiltCommand, String> {
    match command {
        AxCommand::Get {
            session,
            reference,
            attribute,
        } => Ok(remote(
            "raw.ax.get",
            json!({
                "session_id": session, "ref": reference, "attribute": attribute,
            }),
        )),
        AxCommand::Set {
            session,
            reference,
            attribute,
            value,
            mode,
        } => {
            let value = serde_json::from_str::<Value>(&value)
                .map_err(|error| format!("invalid AX value JSON: {error}"))?;
            Ok(remote(
                "raw.ax.set",
                mode_input(
                    json!({
                        "session_id": session, "ref": reference, "attribute": attribute, "value": value,
                    }),
                    mode,
                ),
            ))
        }
        AxCommand::Perform {
            session,
            reference,
            action,
            mode,
        } => Ok(remote(
            "raw.ax.perform",
            mode_input(
                json!({
                    "session_id": session, "ref": reference, "action": action,
                }),
                mode,
            ),
        )),
    }
}

fn action_with_locator(
    id: &'static str,
    session: String,
    locator: LocatorArgs,
    required: bool,
    extra: Value,
    mode: Option<Mode>,
) -> Result<BuiltCommand, String> {
    let locator = locator_value(&locator)?;
    if required && locator.is_none() {
        return Err("this action requires exactly one locator".to_owned());
    }
    let mut input = json!({"session_id": session});
    merge_object(&mut input, extra);
    if let Some(locator) = locator {
        input["locator"] = locator;
    }
    Ok(remote(id, mode_input(input, mode)))
}

fn locator_value(args: &LocatorArgs) -> Result<Option<Value>, String> {
    let semantic = has_semantic(&args.semantic) || has_within(&args.semantic);
    let kinds = usize::from(semantic)
        + usize::from(args.reference.is_some())
        + usize::from(args.point.is_some());
    if kinds > 1 {
        return Err("choose exactly one semantic, ref, or point locator".to_owned());
    }
    if semantic {
        return semantic_locator(&args.semantic).map(Some);
    }
    if let Some(reference) = &args.reference {
        return Ok(Some(json!({"kind": "ref", "ref": reference})));
    }
    if let Some((x, y)) = args.point {
        let frame = args
            .frame_token
            .as_ref()
            .ok_or_else(|| "--point requires --frame".to_owned())?;
        return Ok(Some(
            json!({"kind": "point", "x": x, "y": y, "frame_token": frame}),
        ));
    }
    if args.frame_token.is_some() {
        return Err("--frame requires --point".to_owned());
    }
    Ok(None)
}

fn semantic_locator(args: &SemanticArgs) -> Result<Value, String> {
    if !has_semantic(args) {
        return Err("at least one semantic field is required".to_owned());
    }
    Ok(optional_pairs([
        ("kind", Some(Value::String("semantic".to_owned()))),
        ("role", args.role.clone().map(Value::String)),
        ("name", args.name.clone().map(Value::String)),
        ("text", args.text.clone().map(Value::String)),
        ("identifier", args.identifier.clone().map(Value::String)),
        ("within_role", args.within_role.clone().map(Value::String)),
        ("within_name", args.within_name.clone().map(Value::String)),
    ]))
}

fn has_semantic(args: &SemanticArgs) -> bool {
    args.role.is_some() || args.name.is_some() || args.text.is_some() || args.identifier.is_some()
}

fn has_within(args: &SemanticArgs) -> bool {
    args.within_role.is_some() || args.within_name.is_some()
}

fn evidence(session: String, kind: &str) -> BuiltCommand {
    remote(
        "observe.evidence",
        json!({"session_id": session, "kind": kind}),
    )
}

fn remote(id: &'static str, input: Value) -> BuiltCommand {
    BuiltCommand::Remote { id, input }
}

fn local(id: &'static str, input: Value, value: Value) -> BuiltCommand {
    BuiltCommand::Local { id, input, value }
}

fn mode_input(mut input: Value, mode: Option<Mode>) -> Value {
    if let Some(mode) = mode {
        input["mode"] = Value::String(mode_name(mode).to_owned());
    }
    input
}

fn object_with_optional(mut value: Value, key: &str, optional: Option<Value>) -> Value {
    if let Some(optional) = optional {
        value[key] = optional;
    }
    value
}

fn optional_pairs<const N: usize>(pairs: [(&str, Option<Value>); N]) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        if let Some(value) = value {
            map.insert(key.to_owned(), value);
        }
    }
    Value::Object(map)
}

fn merge_object(destination: &mut Value, source: Value) {
    if let (Some(destination), Some(source)) = (destination.as_object_mut(), source.as_object()) {
        destination.extend(source.clone());
    }
}

fn invoke(id: &'static str, input: Value, request_id: String, timeout_ms: u64) -> (Value, i32) {
    let invocation = Invocation::new(id, input, request_id, timeout_ms);
    match invoke_daemon(invocation) {
        Ok(response) => invoke_response(id, response),
        Err(error) => invoke_error(error),
    }
}

fn invoke_response(id: &'static str, response: RpcResponse) -> (Value, i32) {
    match (response.result, response.error) {
        (Some(result), None) => invoke_success(id, result),
        (_, Some(error)) => local_error("invalid_request", &error.message),
        _ => local_error("internal_error", "daemon returned no result"),
    }
}

fn invoke_success(id: &'static str, mut result: Value) -> (Value, i32) {
    if id == "system.doctor"
        && let Err(error) = augment_doctor(&mut result)
    {
        return local_error("internal_error", &error);
    }
    let exit = result_exit(&result);
    (result, exit)
}

fn invoke_error(error: ClientError) -> (Value, i32) {
    if matches!(error, ClientError::Deadline) {
        return local_error("timed_out", "request deadline expired");
    }
    invoke_non_deadline_error(error)
}

fn invoke_non_deadline_error(error: ClientError) -> (Value, i32) {
    match error {
        error @ ClientError::Control(_, _) => control_result(Err(error)),
        error => local_error("internal_error", &error.to_string()),
    }
}

fn augment_doctor(result: &mut Value) -> Result<(), String> {
    result["daemon"]["installation"] = current_installation_identity()?;
    result["daemon"]["control"] = doctor_daemon_status();
    let legacy = legacy_config_root();
    append_legacy_warning(result, &legacy)
}

fn current_installation_identity() -> Result<Value, String> {
    match Installation::current() {
        Ok(installation) => Ok(installation.identity()),
        Err(error) => Err(error.to_string()),
    }
}

fn doctor_daemon_status() -> Value {
    match daemon_status() {
        Ok(status) => status,
        Err(_) => json!({"running": false}),
    }
}

fn append_legacy_warning(result: &mut Value, legacy: &std::path::Path) -> Result<(), String> {
    if !legacy.exists() {
        return Ok(());
    }
    let Some(warnings) = result["warnings"].as_array_mut() else {
        return Err("doctor warnings are not an array".to_owned());
    };
    warnings.push(
        format!(
            "legacy_state_detected; run manuvra migrate --from computer-use; source={}",
            legacy.display()
        )
        .into(),
    );
    Ok(())
}

fn command_default_timeout(id: &str) -> u64 {
    command_default_timeout_ms(id).expect("every routed command has a registry timeout")
}

fn result_exit(value: &Value) -> i32 {
    value
        .get("error")
        .filter(|error| !error.is_null())
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .and_then(error_meta)
        .map(|meta| meta.exit)
        .unwrap_or(0)
}

fn local_error(code: &str, message: &str) -> (Value, i32) {
    let (error, exit) = operational_error(code, Some(message));
    (json!({"error": error}), exit)
}

fn emit_and_exit(value: Value, exit: i32, output: DiagnosticOutput) -> ! {
    let bytes = match output {
        DiagnosticOutput::Json => encode_operational_line(&value).unwrap_or_else(|_| {
            let (fallback, _) =
                local_error("internal_result_overflow", "result exceeded 4096 bytes");
            encode_operational_line(&fallback).expect("bounded overflow result")
        }),
        DiagnosticOutput::Doctor => render_doctor(&value).into_bytes(),
        DiagnosticOutput::Setup => render_setup(&value).into_bytes(),
    };
    io::stdout().write_all(&bytes).expect("stdout");
    std::process::exit(exit)
}

struct DoctorWarning {
    display: String,
    recovery: Option<String>,
}

impl DoctorWarning {
    fn action_required(&self) -> bool {
        self.recovery.is_some()
    }
}

fn classify_doctor_warning(value: &Value) -> DoctorWarning {
    let warning = value.as_str().unwrap_or("unknown_warning");
    match warning {
        "verified_orphans_removed" => DoctorWarning {
            display: "[resolved] Verified orphan session state was removed safely.".to_owned(),
            recovery: None,
        },
        "unverified_orphans_preserved" => DoctorWarning {
            display: "[action required] Unverified orphan session state was preserved for safety."
                .to_owned(),
            recovery: Some(
                "Preserve `manuvra doctor --json` output and report warning `unverified_orphans_preserved`; Manuvra will not delete unverified state automatically."
                    .to_owned(),
            ),
        },
        "chrome_endpoint_refused" => DoctorWarning {
            display: "[action required] The Chrome adapter loopback CDP endpoint is refused."
                .to_owned(),
            recovery: Some("Run `manuvra chrome launch`.".to_owned()),
        },
        warning if warning.starts_with("legacy_state_detected;") => DoctorWarning {
            display: format!("[action required] {warning}"),
            recovery: legacy_warning_command(warning)
                .map(|command| format!("Run `{command}`."))
                .or_else(|| {
                    Some(
                        "Preserve `manuvra doctor --json` output and report warning `legacy_state_detected`."
                            .to_owned(),
                    )
                }),
        },
        warning => DoctorWarning {
            display: format!("[action required] {warning}"),
            recovery: Some(format!(
                "Preserve `manuvra doctor --json` output and report warning `{warning}`."
            )),
        },
    }
}

fn legacy_warning_command(warning: &str) -> Option<&str> {
    warning
        .strip_prefix("legacy_state_detected;")?
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("run "))
        .filter(|command| !command.is_empty())
}

fn render_doctor(value: &Value) -> String {
    if let Some(error) = render_operational_error(value) {
        return error;
    }
    let permissions = doctor_permissions(value);
    let host_supported = value["host"]["supported"].as_bool().unwrap_or(false);
    let daemon_running = value["daemon"]["control"]["running"]
        .as_bool()
        .unwrap_or(false);
    let warnings = value["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(classify_doctor_warning)
        .collect::<Vec<_>>();
    let all_permissions = permissions
        .iter()
        .all(|(_, granted)| *granted == Some(true));
    let actionable_warning = warnings.iter().any(DoctorWarning::action_required);
    let ready = host_supported && daemon_running && all_permissions && !actionable_warning;
    let mut lines = vec![format!(
        "Manuvra doctor: {}",
        if ready { "ready" } else { "action required" }
    )];
    lines.push(format!(
        "Host: {} (requires macOS {})",
        if host_supported {
            "supported"
        } else {
            "unsupported"
        },
        value["host"]["minimum_macos"].as_str().unwrap_or("26.0")
    ));
    lines.push(format!(
        "Daemon: {}",
        if daemon_running {
            "running"
        } else {
            "not running"
        }
    ));
    lines.extend(render_installation(&value["daemon"]["installation"]));
    lines.push("Permissions (manuvra-daemon):".to_owned());
    for (name, granted) in permissions {
        lines.push(format!(
            "  [{}] {}",
            match granted {
                Some(true) => "granted",
                Some(false) => "missing",
                None => "unknown",
            },
            permission_label(name)
        ));
    }
    if !all_permissions {
        lines.push(permission_residual_grant_note(
            &value["daemon"]["installation"],
        ));
    }
    let sessions = value["sessions"].as_array().cloned().unwrap_or_default();
    lines.push(format!("Active sessions: {}", sessions.len()));
    for session in sessions {
        lines.push(format!(
            "  {} -> {} ({}, {})",
            session["session_id"].as_str().unwrap_or("unknown"),
            session["target_id"].as_str().unwrap_or("unknown"),
            session["role"].as_str().unwrap_or("unknown"),
            session["mode"].as_str().unwrap_or("unknown")
        ));
    }
    lines.push("Warnings:".to_owned());
    if warnings.is_empty() {
        lines.push("  none".to_owned());
    } else {
        lines.extend(
            warnings
                .iter()
                .map(|warning| format!("  - {}", warning.display)),
        );
    }
    lines.push("Next steps:".to_owned());
    let mut step = 1;
    if !host_supported {
        lines.push(format!("  {step}. Run Manuvra on macOS 26 or later."));
        step += 1;
    }
    if !daemon_running {
        lines.push(format!(
            "  {step}. Run `manuvra doctor` to start and recheck the daemon."
        ));
        step += 1;
    }
    if !all_permissions {
        lines.push(format!(
            "  {step}. Run `manuvra setup` to request missing permissions."
        ));
        step += 1;
    }
    for recovery in warnings
        .iter()
        .filter_map(|warning| warning.recovery.as_ref())
    {
        lines.push(format!("  {step}. {recovery}"));
        step += 1;
    }
    if step == 1 {
        lines.push("  none".to_owned());
    }
    lines.join("\n") + "\n"
}

fn render_setup(value: &Value) -> String {
    if let Some(error) = render_operational_error(value) {
        return error;
    }
    let residual = ["accessibility", "screen_recording", "post_event"]
        .into_iter()
        .filter(|permission| value["permissions"][permission]["residual"] == true)
        .collect::<Vec<_>>();
    let mut lines = vec![format!(
        "Manuvra setup: {}",
        if residual.is_empty() {
            "permissions ready"
        } else {
            "manual action required"
        }
    )];
    lines.extend(render_installation(&value["installation"]));
    lines.push("Permissions (manuvra-daemon):".to_owned());
    for permission in ["accessibility", "screen_recording", "post_event"] {
        let fact = &value["permissions"][permission];
        let state = if fact["granted"] == true && fact["freshly_granted"] == true {
            "granted now"
        } else if fact["granted"] == true {
            "already granted"
        } else if fact["settings_opened"] == true {
            "request sent; System Settings opened"
        } else if fact["prompt_requested"] == true {
            "request sent; still missing"
        } else {
            "not available"
        };
        lines.push(format!("  [{}] {}", state, permission_label(permission)));
    }
    if residual.is_empty() {
        lines.push(
            "No permission prompt or System Settings pane was needed for already granted access."
                .to_owned(),
        );
        lines.push("Next: run `manuvra doctor` to confirm overall readiness.".to_owned());
        return lines.join("\n") + "\n";
    }
    lines.push(
        "macOS consent is manual; Manuvra cannot grant itself access or add itself silently."
            .to_owned(),
    );
    lines.push(permission_residual_grant_note(&value["installation"]));
    lines.push("Complete these steps:".to_owned());
    let mut step = 1;
    if residual.contains(&"accessibility") || residual.contains(&"post_event") {
        lines.push(format!(
            "  {step}. Open System Settings > Privacy & Security > Accessibility."
        ));
        step += 1;
        lines.push(bundle_instruction(step, &value["installation"]));
        step += 1;
    }
    if residual.contains(&"screen_recording") {
        lines.push(format!(
            "  {step}. Open System Settings > Privacy & Security > Screen & System Audio Recording."
        ));
        step += 1;
        lines.push(bundle_instruction(step, &value["installation"]));
        step += 1;
    }
    lines.push(format!("  {step}. Run `manuvra doctor` again."));
    lines.join("\n") + "\n"
}

fn render_operational_error(value: &Value) -> Option<String> {
    let error = value.get("error")?.as_object()?;
    Some(format!(
        "Manuvra error [{}]\n{}\nRecovery: `{}`\nHelp: `{}`\n",
        error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error"),
        error
            .get("recovery_command")
            .and_then(Value::as_str)
            .unwrap_or("manuvra doctor"),
        error
            .get("help_command")
            .and_then(Value::as_str)
            .unwrap_or("manuvra --help")
    ))
}

fn doctor_permissions(value: &Value) -> Vec<(&'static str, Option<bool>)> {
    let macos = value["daemon"]["adapters"]
        .as_array()
        .and_then(|adapters| adapters.iter().find(|adapter| adapter["kind"] == "macos"));
    ["accessibility", "screen_recording", "post_event"]
        .into_iter()
        .map(|permission| {
            (
                permission,
                macos.and_then(|adapter| adapter["permissions"][permission].as_bool()),
            )
        })
        .collect()
}

fn render_installation(installation: &Value) -> Vec<String> {
    let mut lines = if installation["installed"] == true {
        vec![
            "Installation: installed bundle".to_owned(),
            format!(
                "Bundle: {}",
                installation["bundle"]
                    .as_str()
                    .unwrap_or("path unavailable")
            ),
        ]
    } else {
        vec![
            "Installation: development layout".to_owned(),
            "Bundle: unavailable (development layouts have no canonical Manuvra.app path)"
                .to_owned(),
        ]
    };
    lines.extend(render_signature_identity(installation));
    lines
}

fn render_signature_identity(installation: &Value) -> Vec<String> {
    if installation["installed"] != true {
        return Vec::new();
    }
    let cdhash = json_string_field(installation, "cdhash");
    let authority = json_string_field(installation, "authority");
    let designated = json_string_field(installation, "designated_requirement");
    if cdhash.is_none() && authority.is_none() && designated.is_none() {
        return Vec::new();
    }
    let cdhash_value = cdhash.and_then(|value| value);
    let authority_value = authority.and_then(|value| value);
    let designated_value = designated.and_then(|value| value);
    let mut lines = Vec::new();
    if cdhash.is_some() {
        lines.push(format!("CDHash: {}", cdhash_value.unwrap_or("unavailable")));
    }
    if authority.is_some() {
        let rendered = authority_value.unwrap_or_else(|| {
            if cdhash_value.is_some()
                || designated_value.is_some_and(|value| value.starts_with("cdhash "))
            {
                "ad-hoc"
            } else {
                "unavailable"
            }
        });
        lines.push(format!("Authority: {rendered}"));
    }
    if designated.is_some() {
        lines.push(format!(
            "Designated requirement: {}",
            designated_value.unwrap_or("unavailable")
        ));
    }
    lines
}

fn json_string_field<'a>(value: &'a Value, key: &str) -> Option<Option<&'a str>> {
    value.get(key).map(Value::as_str)
}

fn permission_label(permission: &str) -> &'static str {
    match permission {
        "accessibility" => "Accessibility",
        "screen_recording" => "Screen & System Audio Recording",
        "post_event" => "Post Event (Accessibility pane)",
        _ => "Unknown permission",
    }
}

fn permission_residual_grant_note(installation: &Value) -> String {
    let shared = "Other Manuvra.app copies share one TCC row for com.taecontrol.manuvra and stay missing. Do not grant a /tmp prefix; extra copies steal that same row.";
    match (
        installation["installed"].as_bool(),
        installation["bundle"].as_str(),
    ) {
        (Some(true), Some(bundle)) => {
            format!("Enable the exact bundle path `{bundle}`. {shared}")
        }
        _ => format!(
            "Enable the exact bundle path printed by `manuvra doctor` once an installed bundle exists. {shared}"
        ),
    }
}

fn bundle_instruction(step: usize, installation: &Value) -> String {
    match (
        installation["installed"].as_bool(),
        installation["bundle"].as_str(),
    ) {
        (Some(true), Some(bundle)) => format!(
            "  {step}. If Manuvra is absent, click Add, select `{bundle}`, then enable that exact path."
        ),
        (Some(true), None) => format!(
            "  {step}. The installed bundle path is unavailable; reinstall Manuvra, rerun `manuvra setup`, and use the exact path it reports before clicking Add."
        ),
        _ => format!(
            "  {step}. Install the Manuvra.app bundle first; this development layout has no bundle path to add."
        ),
    }
}

fn new_request_id() -> String {
    let suffix: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();
    format!("r_{suffix}")
}

fn parse_point(value: &str) -> Result<(f64, f64), String> {
    let (x, y) = value
        .split_once(',')
        .ok_or_else(|| "point must be x,y".to_owned())?;
    let x = x.parse::<f64>().map_err(|_| "invalid point x".to_owned())?;
    let y = y.parse::<f64>().map_err(|_| "invalid point y".to_owned())?;
    if x < 0.0 || y < 0.0 {
        return Err("point coordinates must be non-negative".to_owned());
    }
    Ok((x, y))
}

fn target_kind(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Chrome => "chrome",
        TargetKind::Macos => "macos",
    }
}
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Actor => "actor",
        Role::Observer => "observer",
    }
}
fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Background => "background",
        Mode::Foreground => "foreground",
    }
}
fn lease_action(action: LeaseAction) -> &'static str {
    match action {
        LeaseAction::Acquire => "acquire",
        LeaseAction::Renew => "renew",
        LeaseAction::Release => "release",
    }
}
fn raw_intent(intent: RawIntent) -> &'static str {
    match intent {
        RawIntent::Query => "query",
        RawIntent::Action => "action",
    }
}

fn legacy_source(source: LegacySource) -> &'static str {
    match source {
        LegacySource::ComputerUse => "computer-use",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manuvra_protocol::all_errors;
    use std::collections::HashSet;
    use std::process::Command as ProcessCommand;

    fn build(args: &[&str]) -> Result<BuiltCommand, BuildError> {
        let cli = Cli::try_parse_from(std::iter::once("manuvra").chain(args.iter().copied()))
            .expect("valid CLI fixture");
        build_command(cli.command)
    }

    fn build_shell_example(line: &str) -> BuiltCommand {
        let output = ProcessCommand::new("/bin/sh")
            .args([
                "-c",
                "eval \"set -- $1\"; printf '%s\\0' \"$@\"",
                "manuvra-example",
                line,
            ])
            .output()
            .expect("execute the platform shell for a registry example");
        assert!(output.status.success(), "invalid shell example: {line}");
        let words = String::from_utf8(output.stdout).expect("UTF-8 registry example");
        let cli = Cli::try_parse_from(words.split_terminator('\0'))
            .unwrap_or_else(|error| panic!("CLI parser rejected {line}: {error}"));
        build_command(cli.command)
            .unwrap_or_else(|error| panic!("CLI builder rejected {line}: {error}"))
    }

    #[test]
    fn every_error_is_known_to_cli_exit_mapping() {
        for error in all_errors() {
            assert_eq!(error_meta(&error.code).unwrap().exit, error.exit);
        }
    }

    #[test]
    fn locator_rejects_mixed_forms() {
        let args = LocatorArgs {
            semantic: SemanticArgs {
                role: Some("button".to_owned()),
                ..Default::default()
            },
            reference: Some("e_1_1".to_owned()),
            ..Default::default()
        };
        assert!(locator_value(&args).is_err());
    }

    #[test]
    fn semantic_locator_includes_optional_ancestor_scope() {
        let built = build(&[
            "click",
            "--session",
            "s_1",
            "--role",
            "button",
            "--name",
            "Checkout",
            "--within-role",
            "region",
            "--within-name",
            "Primary",
        ])
        .unwrap();
        match built {
            BuiltCommand::Remote { input, .. } => {
                assert_eq!(input["locator"]["kind"], "semantic");
                assert_eq!(input["locator"]["within_role"], "region");
                assert_eq!(input["locator"]["within_name"], "Primary");
            }
            BuiltCommand::Local { .. } => panic!("click is a remote command"),
        }
        let query = build(&[
            "observe",
            "query",
            "--session",
            "s_1",
            "--role",
            "button",
            "--within-role",
            "region",
        ])
        .unwrap();
        match query {
            BuiltCommand::Remote { input, .. } => {
                assert_eq!(input["semantic"]["within_role"], "region");
                assert!(input["semantic"].get("within_name").is_none());
            }
            BuiltCommand::Local { .. } => panic!("observe query is a remote command"),
        }
    }

    #[test]
    fn within_scope_is_not_a_standalone_locator() {
        assert!(matches!(
            build(&[
                "click",
                "--session",
                "s_1",
                "--within-role",
                "region",
                "--within-name",
                "Primary"
            ]),
            Err(BuildError::InvalidRequest(_))
        ));
        assert!(
            locator_value(&LocatorArgs {
                semantic: SemanticArgs {
                    within_role: Some("region".to_owned()),
                    ..Default::default()
                },
                reference: Some("e_1".to_owned()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn every_public_route_builds_an_invocation() {
        let routes: &[&[&str]] = &[
            &["commands", "list"],
            &["commands", "get", "action.click"],
            &["commands", "schema", "action.click", "--side", "input"],
            &["commands", "schema", "action.click", "--side", "result"],
            &["commands", "errors", "invalid_request"],
            &["commands", "usage", "enable"],
            &["commands", "usage", "show"],
            &["commands", "usage", "export", "/tmp/usage.json"],
            &["commands", "usage", "reset"],
            &["commands", "usage", "disable"],
            &["targets", "--kind", "chrome", "--cursor", "0"],
            &["open", "--target", "chrome:fake", "--role", "actor"],
            &["close", "--session", "s_1", "--cancel-running"],
            &["lease", "acquire", "--session", "s_1", "--ttl-ms", "10000"],
            &["lease", "renew", "--session", "s_1"],
            &["lease", "release", "--session", "s_1"],
            &["click", "--session", "s_1", "--role", "button"],
            &[
                "type",
                "--session",
                "s_1",
                "--role",
                "textbox",
                "--name",
                "Email",
                "--text",
                "hello",
            ],
            &[
                "type",
                "--session",
                "s_1",
                "--locator-text",
                "Old value",
                "--text",
                "hello",
            ],
            &["press", "--session", "s_1", "--key", "Enter"],
            &[
                "scroll",
                "--session",
                "s_1",
                "--delta-y",
                "10",
                "--point",
                "1,2",
                "--frame",
                "f_1",
            ],
            &[
                "navigate",
                "--session",
                "s_1",
                "--url",
                "https://example.invalid",
            ],
            &["observe", "screenshot", "--session", "s_1"],
            &["observe", "query", "--session", "s_1", "--name", "Save"],
            &["observe", "tree", "--session", "s_1"],
            &["observe", "logs", "--session", "s_1"],
            &["observe", "events", "--session", "s_1"],
            &["observe", "diagnostics", "--session", "s_1"],
            &["observe", "timings", "--session", "s_1"],
            &["observe", "manifest", "--session", "s_1"],
            &[
                "raw",
                "cdp",
                "--session",
                "s_1",
                "--intent",
                "query",
                "--method",
                "Runtime.evaluate",
                "--params",
                "{}",
            ],
            &[
                "raw",
                "ax",
                "get",
                "--session",
                "s_1",
                "--ref",
                "e_1",
                "--attribute",
                "AXTitle",
            ],
            &[
                "raw",
                "ax",
                "set",
                "--session",
                "s_1",
                "--ref",
                "e_1",
                "--attribute",
                "AXValue",
                "--value",
                "\"hello\"",
            ],
            &[
                "raw",
                "ax",
                "perform",
                "--session",
                "s_1",
                "--ref",
                "e_1",
                "--action",
                "AXPress",
            ],
            &["cancel", "--session", "s_1", "--request-id", "r_1"],
            &[
                "export",
                "--session",
                "s_1",
                "--all",
                "--destination",
                "/tmp/export",
            ],
            &[
                "export",
                "--session",
                "s_1",
                "--artifact",
                "a_1",
                "--destination",
                "/tmp/export",
            ],
            &["doctor", "--session", "s_1", "--target", "chrome:fake"],
            &["daemon", "status"],
            &["daemon", "stop"],
            &["chrome", "launch"],
            &["setup"],
            &["migrate", "--from", "computer-use"],
            &["purge", "--all", "--yes"],
        ];
        let mut covered = HashSet::new();
        for route in routes {
            let expected = expected_registry_id(route);
            let built =
                build(route).unwrap_or_else(|error| panic!("failed route {route:?}: {error}"));
            let id = match built {
                BuiltCommand::Local { id, .. } | BuiltCommand::Remote { id, .. } => id,
            };
            assert_eq!(id, expected, "registry routing drift for {route:?}");
            covered.insert(expected);
        }
        assert_eq!(
            covered.len(),
            31,
            "every registry command needs a CLI fixture"
        );
    }

    fn expected_registry_id(args: &[&str]) -> &'static str {
        manuvra_protocol::registry()["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|command| {
                let cli = command["cli"].as_array()?;
                (cli.len() <= args.len()
                    && cli
                        .iter()
                        .enumerate()
                        .all(|(index, part)| part.as_str() == Some(args[index])))
                .then_some((cli.len(), command["id"].as_str().unwrap()))
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, id)| id)
            .expect("fixture route exists in registry")
    }

    #[test]
    fn local_builder_rejects_invalid_inputs() {
        assert!(matches!(
            build(&["commands", "list", "--cursor", "bad"]),
            Err(BuildError::InvalidRequest(_))
        ));
        assert!(matches!(
            build(&["commands", "get", "missing"]),
            Err(BuildError::UnknownCommand)
        ));
        assert!(matches!(
            build(&["commands", "get", "common.press"]),
            Err(BuildError::UnknownCommand)
        ));
        assert!(matches!(
            build(&["commands", "schema", "common.press", "--side", "input"]),
            Err(BuildError::UnknownCommand)
        ));
        assert!(matches!(
            build(&["commands", "errors", "missing"]),
            Err(BuildError::InvalidRequest(_))
        ));
        assert!(matches!(
            build(&[
                "raw",
                "cdp",
                "--session",
                "s_1",
                "--intent",
                "query",
                "--method",
                "x",
                "--params",
                "not-json"
            ]),
            Err(BuildError::InvalidRequest(_))
        ));
        assert!(matches!(
            build(&[
                "raw",
                "ax",
                "set",
                "--session",
                "s_1",
                "--ref",
                "e_1",
                "--attribute",
                "AXValue",
                "--value",
                "not-json"
            ]),
            Err(BuildError::InvalidRequest(_))
        ));
    }

    #[test]
    fn point_parser_checks_shape_numbers_and_sign() {
        assert_eq!(parse_point("1.5,2").unwrap(), (1.5, 2.0));
        assert!(parse_point("1").is_err());
        assert!(parse_point("x,2").is_err());
        assert!(parse_point("1,y").is_err());
        assert!(parse_point("-1,2").is_err());
    }

    #[test]
    fn registry_controls_command_timeouts_and_help_examples() {
        assert_eq!(command_default_timeout("target.list"), 2_000);
        assert_eq!(command_default_timeout("action.click"), 5_000);
        assert_eq!(command_default_timeout("action.navigate"), 10_000);
        assert_eq!(command_default_timeout("observe.tree"), 15_000);
        for command in manuvra_protocol::registry()["commands"].as_array().unwrap() {
            let id = command["id"].as_str().unwrap();
            let help = command_help(id).unwrap();
            let example = help["examples"][0].as_str().unwrap();
            assert!(example.starts_with("manuvra "));
            assert!(!example.ends_with(" --help"));
            assert_eq!(
                help["defaults"]["timeout_ms"].as_u64(),
                command["default_timeout_ms"].as_u64()
            );
        }
    }

    #[test]
    fn every_registry_example_parses_and_builds_its_canonical_input() {
        for command in manuvra_protocol::registry()["commands"].as_array().unwrap() {
            let expected_id = command["id"].as_str().unwrap();
            for example in command["examples"].as_array().unwrap() {
                let line = example["cli"].as_str().unwrap();
                let built = build_shell_example(line);
                let (actual_id, actual_input) = match built {
                    BuiltCommand::Local { id, input, .. } | BuiltCommand::Remote { id, input } => {
                        (id, input)
                    }
                };
                assert_eq!(actual_id, expected_id, "wrong command for {line}");
                assert_eq!(actual_input, example["input"], "wrong input for {line}");
            }
        }
    }

    fn parsed_command(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("manuvra").chain(args.iter().copied()))
            .unwrap()
            .command
    }

    fn setup_fact(before: bool, requested: bool, granted: bool) -> Value {
        json!({
            "before_granted": before,
            "prompt_requested": requested,
            "settings_opened": false,
            "granted": granted,
            "freshly_granted": !before && granted,
            "residual": !granted
        })
    }

    fn setup_fixture(accessibility: Value, screen: Value, post_event: Value) -> Value {
        json!({
            "permissions": {
                "accessibility": accessibility,
                "screen_recording": screen,
                "post_event": post_event
            },
            "installation": {}
        })
    }

    fn doctor_fixture(granted: bool) -> Value {
        json!({
            "host": {"minimum_macos": "26.0", "supported": true},
            "daemon": {
                "installation": {
                    "installed": true,
                    "bundle": "/opt/homebrew/opt/manuvra/libexec/Manuvra.app"
                },
                "control": {"running": true},
                "adapters": [{
                    "kind": "macos",
                    "permissions": {
                        "accessibility": granted,
                        "screen_recording": granted,
                        "post_event": granted
                    }
                }]
            },
            "permissions": {"same_user_socket": true},
            "sessions": [],
            "warnings": []
        })
    }

    #[test]
    fn diagnostics_are_human_only_for_terminal_without_json_override() {
        let doctor = parsed_command(&["doctor"]);
        let doctor_json = parsed_command(&["doctor", "--json"]);
        let setup = parsed_command(&["setup"]);
        let setup_json = parsed_command(&["setup", "--json"]);
        let targets = parsed_command(&["targets"]);

        assert_eq!(diagnostic_output(&doctor, true), DiagnosticOutput::Doctor);
        assert_eq!(diagnostic_output(&doctor, false), DiagnosticOutput::Json);
        assert_eq!(
            diagnostic_output(&doctor_json, true),
            DiagnosticOutput::Json
        );
        assert_eq!(diagnostic_output(&setup, true), DiagnosticOutput::Setup);
        assert_eq!(diagnostic_output(&setup, false), DiagnosticOutput::Json);
        assert_eq!(diagnostic_output(&setup_json, true), DiagnosticOutput::Json);
        assert_eq!(diagnostic_output(&targets, true), DiagnosticOutput::Json);
    }

    #[test]
    fn doctor_renderer_covers_healthy_missing_and_operational_error_states() {
        let healthy = render_doctor(&doctor_fixture(true));
        assert!(healthy.contains("Manuvra doctor: ready"));
        assert!(healthy.contains("[granted] Accessibility"));
        assert!(healthy.contains("Next steps:\n  none"));
        assert!(!healthy.contains("Enable the exact bundle path"));
        assert!(!healthy.contains("TCC row"));

        let mut missing = doctor_fixture(false);
        missing["warnings"] = json!([
            "legacy_state_detected; run manuvra migrate --from computer-use; source=/tmp/legacy"
        ]);
        let missing = render_doctor(&missing);
        assert!(missing.contains("Manuvra doctor: action required"));
        assert!(missing.contains("[missing] Screen & System Audio Recording"));
        assert!(missing.contains("Run `manuvra setup`"));
        assert!(missing.contains("legacy_state_detected"));
        assert!(missing.contains("Run `manuvra migrate --from computer-use`."));
        assert!(missing.contains(
            "Enable the exact bundle path `/opt/homebrew/opt/manuvra/libexec/Manuvra.app`."
        ));
        assert!(missing.contains(
            "Other Manuvra.app copies share one TCC row for com.taecontrol.manuvra and stay missing."
        ));
        assert!(missing.contains("Do not grant a /tmp prefix"));

        let (error, _) = local_error("timed_out", "daemon did not answer");
        let rendered = render_doctor(&error);
        assert!(rendered.contains("Manuvra error [timed_out]"));
        assert!(rendered.contains("daemon did not answer"));
        assert!(rendered.contains("Recovery: `"));
        assert!(rendered.contains("Help: `manuvra commands errors timed_out`"));
    }

    #[test]
    fn doctor_renderer_shows_authority_and_designated_requirement_beside_cdhash() {
        let mut doctor = doctor_fixture(true);
        doctor["daemon"]["installation"]["cdhash"] = json!("abc123");
        doctor["daemon"]["installation"]["authority"] = json!("Manuvra Local");
        doctor["daemon"]["installation"]["designated_requirement"] =
            json!("identifier \"com.taecontrol.manuvra\" and certificate leaf = H\"ABCD\"");
        let rendered = render_doctor(&doctor);
        assert!(rendered.contains("CDHash: abc123"));
        assert!(rendered.contains("Authority: Manuvra Local"));
        assert!(rendered.contains(
            "Designated requirement: identifier \"com.taecontrol.manuvra\" and certificate leaf = H\"ABCD\""
        ));

        let mut adhoc = doctor_fixture(true);
        adhoc["daemon"]["installation"]["cdhash"] = json!("def456");
        adhoc["daemon"]["installation"]["authority"] = Value::Null;
        adhoc["daemon"]["installation"]["designated_requirement"] = json!("cdhash H\"DEF456\"");
        let rendered = render_doctor(&adhoc);
        assert!(rendered.contains("CDHash: def456"));
        assert!(rendered.contains("Authority: ad-hoc"));
        assert!(rendered.contains("Designated requirement: cdhash H\"DEF456\""));

        let mut missing_signature = doctor_fixture(true);
        missing_signature["daemon"]["installation"]["cdhash"] = Value::Null;
        missing_signature["daemon"]["installation"]["authority"] = Value::Null;
        missing_signature["daemon"]["installation"]["designated_requirement"] = Value::Null;
        let rendered = render_doctor(&missing_signature);
        assert!(rendered.contains("CDHash: unavailable"));
        assert!(rendered.contains("Authority: unavailable"));
        assert!(rendered.contains("Designated requirement: unavailable"));
        assert!(!rendered.contains("Authority: ad-hoc"));
    }

    #[test]
    fn doctor_renderer_classifies_completed_and_preserved_orphan_cleanup() {
        let mut completed = doctor_fixture(true);
        completed["warnings"] = json!(["verified_orphans_removed"]);
        let completed = render_doctor(&completed);
        assert!(completed.contains("Manuvra doctor: ready"));
        assert!(completed.contains("[resolved] Verified orphan session state was removed safely."));
        assert!(completed.contains("Next steps:\n  none"));

        let mut preserved = doctor_fixture(true);
        preserved["warnings"] = json!(["unverified_orphans_preserved"]);
        let preserved = render_doctor(&preserved);
        assert!(preserved.contains("Manuvra doctor: action required"));
        assert!(preserved.contains("Unverified orphan session state was preserved for safety."));
        assert!(preserved.contains("Preserve `manuvra doctor --json` output"));
        assert!(preserved.contains("Manuvra will not delete unverified state automatically."));
    }

    #[test]
    fn doctor_renderer_names_chrome_launch_when_the_endpoint_is_refused() {
        let mut doctor = doctor_fixture(true);
        doctor["warnings"] = json!(["chrome_endpoint_refused"]);
        let rendered = render_doctor(&doctor);
        assert!(rendered.contains("Manuvra doctor: action required"));
        assert!(rendered.contains("Chrome adapter loopback CDP endpoint is refused"));
        assert!(rendered.contains("Run `manuvra chrome launch`."));
    }

    #[test]
    fn setup_renderer_preserves_daemon_reported_pane_and_permission_states() {
        let mut setup = setup_fixture(
            setup_fact(false, true, false),
            setup_fact(true, false, true),
            setup_fact(false, true, false),
        );
        setup["permissions"]["accessibility"]["settings_opened"] = Value::Bool(true);
        setup["permissions"]["post_event"]["settings_opened"] = Value::Bool(true);
        setup["installation"] = json!({
            "installed": true,
            "bundle": "/opt/homebrew/opt/manuvra/libexec/Manuvra.app"
        });

        assert_eq!(
            setup["permissions"]["accessibility"]["settings_opened"],
            true
        );
        assert_eq!(setup["permissions"]["post_event"]["settings_opened"], true);
        assert_eq!(
            setup["permissions"]["screen_recording"]["settings_opened"],
            false
        );
        manuvra_protocol::validate_command_result("system.setup", &setup).unwrap();
        let rendered = render_setup(&setup);
        assert!(rendered.contains("manual action required"));
        assert!(rendered.contains("macOS consent is manual"));
        assert!(rendered.contains("/opt/homebrew/opt/manuvra/libexec/Manuvra.app"));
        assert!(rendered.contains("Run `manuvra doctor` again"));
        assert!(rendered.contains(
            "Enable the exact bundle path `/opt/homebrew/opt/manuvra/libexec/Manuvra.app`."
        ));
        assert!(rendered.contains(
            "Other Manuvra.app copies share one TCC row for com.taecontrol.manuvra and stay missing."
        ));
        assert!(rendered.contains("Do not grant a /tmp prefix"));
        assert!(rendered.contains("enable that exact path"));
        assert!(rendered.contains("Accessibility"));
        assert!(rendered.contains("Post Event (Accessibility pane)"));
        assert!(!rendered.contains("Privacy & Security > Post Event"));
    }

    #[test]
    fn setup_all_granted_and_development_layout_never_invent_actions_or_bundle_path() {
        let granted = setup_fact(true, false, true);
        let ready = setup_fixture(granted.clone(), granted.clone(), granted);
        let rendered = render_setup(&ready);
        assert!(rendered.contains("permissions ready"));
        assert!(rendered.contains("No permission prompt or System Settings pane was needed"));
        assert!(!rendered.contains("Complete these steps"));
        assert!(!rendered.contains("Enable the exact bundle path"));
        assert!(!rendered.contains("TCC row"));

        let residual = setup_fixture(
            setup_fact(false, true, false),
            setup_fact(true, false, true),
            setup_fact(true, false, true),
        );
        let rendered = render_setup(&residual);
        assert!(rendered.contains("development layouts have no canonical Manuvra.app path"));
        assert!(rendered.contains("this development layout has no bundle path to add"));
        assert!(rendered.contains(
            "Enable the exact bundle path printed by `manuvra doctor` once an installed bundle exists."
        ));
        assert!(rendered.contains(
            "Other Manuvra.app copies share one TCC row for com.taecontrol.manuvra and stay missing."
        ));
        assert!(rendered.contains("Do not grant a /tmp prefix"));
        assert!(!rendered.contains("/Applications/Manuvra.app"));

        let mut installed_without_bundle = residual;
        installed_without_bundle["installation"] = json!({"installed": true, "bundle": null});
        let rendered = render_setup(&installed_without_bundle);
        assert!(rendered.contains("installed bundle path is unavailable"));
        assert!(rendered.contains("reinstall Manuvra"));
        assert!(!rendered.contains("this development layout has no bundle path to add"));
    }

    #[test]
    fn permission_residual_lines_name_exact_bundle_and_shared_tcc_row() {
        let mut doctor = doctor_fixture(false);
        doctor["daemon"]["adapters"][0]["permissions"]["accessibility"] = json!(true);
        doctor["daemon"]["adapters"][0]["permissions"]["post_event"] = json!(false);
        let rendered = render_doctor(&doctor);
        assert!(rendered.contains("[missing] Post Event (Accessibility pane)"));
        assert!(rendered.contains(
            "Enable the exact bundle path `/opt/homebrew/opt/manuvra/libexec/Manuvra.app`."
        ));
        assert!(rendered.contains("share one TCC row for com.taecontrol.manuvra"));
        assert!(rendered.contains("Do not grant a /tmp prefix; extra copies steal that same row."));

        let mut setup = setup_fixture(
            setup_fact(true, false, true),
            setup_fact(false, true, false),
            setup_fact(true, false, true),
        );
        setup["installation"] = json!({
            "installed": true,
            "bundle": "/opt/manuvra-local/Manuvra.app"
        });
        let rendered = render_setup(&setup);
        assert!(
            rendered.contains("Enable the exact bundle path `/opt/manuvra-local/Manuvra.app`.")
        );
        assert!(rendered.contains("share one TCC row for com.taecontrol.manuvra"));
        assert!(rendered.contains("Do not grant a /tmp prefix"));
        assert!(rendered.contains("Screen & System Audio Recording"));
        assert!(!rendered.contains("Privacy & Security > Accessibility"));
    }
}
