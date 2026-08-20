use clap::{Args, Parser, Subcommand, ValueEnum};
use manuvra_cli::{
    ClientError, Installation, daemon_status, daemon_stop, invoke_daemon, legacy_config_root,
    migrate_legacy, purge_owned_roots,
};
use manuvra_protocol::{
    AGENT_HELP, Invocation, command_default_timeout_ms, command_descriptor, command_help,
    encode_operational_line, error_meta, operational_error, registry_page, schema_pointer,
    validate_command_input,
};
use rand::Rng;
use rand::distr::Alphanumeric;
use serde_json::{Map, Value, json};
use std::io::{self, IsTerminal, Write};
use std::process::Command as ProcessCommand;

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
    },
    Setup,
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
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
    let request_id = invocation_request_id(&cli.command, cli.request_id);
    let result =
        execute_special(&cli.command).unwrap_or_else(|| match build_command(cli.command) {
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
            Err(message) => local_error("invalid_request", &message),
        });
    emit_and_exit(result.0, result.1);
}

fn execute_special(command: &Command) -> Option<(Value, i32)> {
    match command {
        Command::Daemon {
            command: DaemonCommand::Status,
        } => Some(control_result(daemon_status())),
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => Some(control_result(daemon_stop())),
        Command::Setup => Some(run_setup()),
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

fn run_setup() -> (Value, i32) {
    let (doctor, exit) = invoke("system.doctor", json!({}), new_request_id(), 5_000);
    if exit != 0 {
        return (doctor, exit);
    }
    finish_setup(doctor)
}

fn finish_setup(doctor: Value) -> (Value, i32) {
    let missing = missing_permissions(&doctor);
    let opened = match open_permission_panes(&missing) {
        Ok(opened) => opened,
        Err(permission) => {
            return local_error(
                "internal_error",
                &format!("failed to open System Settings for {permission}"),
            );
        }
    };
    let installation = doctor["daemon"]["installation"].clone();
    (
        json!({"opened": opened, "missing": missing, "installation": installation}),
        0,
    )
}

fn open_permission_panes(missing: &[String]) -> Result<Vec<String>, String> {
    missing
        .iter()
        .filter(|permission| permission_settings_url(permission).is_some())
        .map(|permission| open_permission_pane(permission).map(|()| permission.clone()))
        .collect()
}

fn open_permission_pane(permission: &str) -> Result<(), String> {
    if settings_open_suppressed() {
        return Ok(());
    }
    let url = permission_settings_url(permission).ok_or_else(|| permission.to_owned())?;
    open_settings_url(url).ok_or_else(|| permission.to_owned())
}

fn settings_open_suppressed() -> bool {
    cfg!(debug_assertions) && std::env::var_os("MANUVRA_TEST_NO_OPEN").is_some()
}

fn open_settings_url(url: &str) -> Option<()> {
    ProcessCommand::new("/usr/bin/open")
        .arg(url)
        .status()
        .ok()
        .filter(|status| status.success())
        .map(|_| ())
}

fn permission_settings_url(permission: &str) -> Option<&'static str> {
    const PANES: &[(&str, &str)] = &[
        (
            "accessibility",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        ),
        (
            "screen_recording",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        ),
    ];
    PANES
        .iter()
        .find(|(name, _)| *name == permission)
        .map(|(_, url)| *url)
}

fn missing_permissions(doctor: &Value) -> Vec<String> {
    let Some(adapters) = doctor["daemon"]["adapters"].as_array() else {
        return Vec::new();
    };
    let Some(macos) = adapters.iter().find(|adapter| adapter["kind"] == "macos") else {
        return Vec::new();
    };
    ["accessibility", "screen_recording"]
        .into_iter()
        .filter(|permission| macos["permissions"][permission] == false)
        .map(str::to_owned)
        .collect()
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

fn build_command(command: Command) -> Result<BuiltCommand, String> {
    match command {
        Command::Commands { command } => build_commands(command),
        Command::Observe { command } => build_observe(command),
        Command::Raw { command } => build_raw(command),
        command @ (Command::Click { .. }
        | Command::Type { .. }
        | Command::Press { .. }
        | Command::Scroll { .. }
        | Command::Navigate { .. }) => build_action(command),
        command => build_direct(command),
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
        Command::Doctor { session, target_id } => Ok(remote(
            "system.doctor",
            optional_pairs([
                ("session_id", session.map(Value::String)),
                ("target_id", target_id.map(Value::String)),
            ]),
        )),
        command @ (Command::Daemon { .. }
        | Command::Setup
        | Command::Migrate { .. }
        | Command::Purge { .. }) => Ok(build_local_direct(command)),
        _ => unreachable!("routed command category"),
    }
}

fn build_local_direct(command: Command) -> BuiltCommand {
    match command {
        Command::Daemon { command } => local(daemon_command_id(command), json!({}), Value::Null),
        Command::Setup => local("system.setup", json!({}), Value::Null),
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

fn build_commands(command: CommandsCommand) -> Result<BuiltCommand, String> {
    match command {
        CommandsCommand::List { cursor, limit } => build_command_list(cursor, limit),
        CommandsCommand::Get { command } => {
            let value =
                command_help(&command).ok_or_else(|| format!("unknown command {command}"))?;
            Ok(local(
                "system.commands.get",
                json!({"command": command}),
                value,
            ))
        }
        CommandsCommand::Schema { command, side } => build_command_schema(&command, side),
        CommandsCommand::Errors { code } => build_command_error(&code),
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

fn build_command_schema(command: &str, side: SchemaSide) -> Result<BuiltCommand, String> {
    let descriptor =
        command_descriptor(command).ok_or_else(|| format!("unknown command {command}"))?;
    let (key, side_name) = match side {
        SchemaSide::Input => ("input_schema", "input"),
        SchemaSide::Result => ("result_schema", "result"),
    };
    let reference = descriptor[key]
        .as_str()
        .ok_or_else(|| "invalid installed schema pointer".to_owned())?;
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
    let semantic = has_semantic(&args.semantic);
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
    ]))
}

fn has_semantic(args: &SemanticArgs) -> bool {
    args.role.is_some() || args.name.is_some() || args.text.is_some() || args.identifier.is_some()
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
        Ok(response) => match (response.result, response.error) {
            (Some(mut result), None) => {
                if id == "system.doctor"
                    && let Err(error) = augment_doctor(&mut result)
                {
                    return local_error("internal_error", &error);
                }
                let exit = result_exit(&result);
                (result, exit)
            }
            (_, Some(error)) => local_error("invalid_request", &error.message),
            _ => local_error("internal_error", "daemon returned no result"),
        },
        Err(ClientError::Deadline) => local_error("timed_out", "request deadline expired"),
        Err(error @ ClientError::Control(_, _)) => control_result(Err(error)),
        Err(error) => local_error("internal_error", &error.to_string()),
    }
}

fn augment_doctor(result: &mut Value) -> Result<(), String> {
    let installation = Installation::current().map_err(|error| error.to_string())?;
    result["daemon"]["installation"] = installation.identity();
    result["daemon"]["control"] = daemon_status().unwrap_or_else(|_| json!({"running": false}));
    let legacy = legacy_config_root();
    if legacy.exists() {
        let warnings = result["warnings"]
            .as_array_mut()
            .ok_or_else(|| "doctor warnings are not an array".to_owned())?;
        warnings.push(
            format!(
                "legacy_state_detected; run manuvra migrate --from computer-use; source={}",
                legacy.display()
            )
            .into(),
        );
    }
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

fn emit_and_exit(value: Value, exit: i32) -> ! {
    let bytes = encode_operational_line(&value).unwrap_or_else(|_| {
        let (fallback, _) = local_error("internal_result_overflow", "result exceeded 4096 bytes");
        encode_operational_line(&fallback).expect("bounded overflow result")
    });
    io::stdout().write_all(&bytes).expect("stdout");
    std::process::exit(exit)
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

    fn build(args: &[&str]) -> Result<BuiltCommand, String> {
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
            30,
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
        assert!(build(&["commands", "list", "--cursor", "bad"]).is_err());
        assert!(build(&["commands", "get", "missing"]).is_err());
        assert!(build(&["commands", "errors", "missing"]).is_err());
        assert!(
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
            ])
            .is_err()
        );
        assert!(
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
            ])
            .is_err()
        );
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
}
