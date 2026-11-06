use std::env;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod bot_process;
mod gateway_session;
use bot_process::{
    prepare_input, prepare_output, take_input, take_output, wait_for_exit, BotReadInterrupt,
    ManagedChild,
};
#[cfg(test)]
use gateway_session::{
    gateway_session_tmp_path, write_gateway_session_file, write_gateway_session_tmp,
};
use gateway_session::{load_gateway_resume_session, save_gateway_resume_session};

#[cfg(all(unix, test))]
use std::os::unix::fs::PermissionsExt;

use dfor77::{
    diagnostic_field, read_request, validate_audit_log_text_with_options,
    validate_runtime_log_text, write_diagnostic, write_response, AuditContentPolicy,
    AuditLogValidationOptions, Config, DforRuntime, GatewayAction, GatewayBackoffInfo,
    GatewayBackoffNext, GatewayConnectResult, GatewayPumpResult, GatewayStartup, GatewayTransport,
    HttpRestBackend, Request, Response, RuntimeLogField, RuntimeSettings,
    TungsteniteGatewayTransport, D7FOR, D7OK, D7TIM, WHY_AUTH_INVALID,
};

#[cfg(test)]
use dfor77::GatewayResumeSession;

use dfor77::audit::{json_escape, json_log_escape};

const EXIT_BOT_CRASHED: i32 = 10;
const EXIT_PROTOCOL: i32 = 11;
const EXIT_CONFIG: i32 = 12;
const EXIT_GATEWAY_FATAL: i32 = 13;
const EXIT_REST_AUTH_FATAL: i32 = 14;
const EXIT_INTERNAL: i32 = 70;

struct SharedRuntime {
    runtime: Mutex<DforRuntime>,
    event_cv: Condvar,
}

type RuntimeHandle = Arc<SharedRuntime>;

impl SharedRuntime {
    fn new(runtime: DforRuntime) -> RuntimeHandle {
        Arc::new(Self {
            runtime: Mutex::new(runtime),
            event_cv: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::LockResult<MutexGuard<'_, DforRuntime>> {
        self.runtime.lock()
    }

    fn notify_events(&self) {
        self.event_cv.notify_all();
    }
}

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(AppError::Exit(code)) => code,
        Err(AppError::Message(code, message)) => {
            write_diagnostic(format_args!("{message}"));
            code
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), AppError> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut options = Options::default();
    let mut bot_command = None;
    let mut mode_flag = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--validate-config" => {
                set_mode(
                    &mut options,
                    &mut mode_flag,
                    Mode::ValidateConfig,
                    "--validate-config",
                )?;
                index += 1;
            }
            "--validate-audit-log" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--validate-audit-log requires a path")
                    })?
                    .clone();
                set_mode(
                    &mut options,
                    &mut mode_flag,
                    Mode::ValidateAuditLog,
                    "--validate-audit-log",
                )?;
                options.validate_audit_log_path = Some(path);
                index += 2;
            }
            "--validate-runtime-log" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--validate-runtime-log requires a path")
                    })?
                    .clone();
                set_mode(
                    &mut options,
                    &mut mode_flag,
                    Mode::ValidateRuntimeLog,
                    "--validate-runtime-log",
                )?;
                options.validate_runtime_log_path = Some(path);
                index += 2;
            }
            "--audit-require-action" => {
                let action = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--audit-require-action requires an action")
                    })?
                    .clone();
                if action.is_empty() {
                    return Err(AppError::message(
                        EXIT_CONFIG,
                        "--audit-require-action requires a non-empty action",
                    ));
                }
                options.audit_required_actions.push(action);
                index += 2;
            }
            "--audit-forbid-action" => {
                let action = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--audit-forbid-action requires an action")
                    })?
                    .clone();
                if action.is_empty() {
                    return Err(AppError::message(
                        EXIT_CONFIG,
                        "--audit-forbid-action requires a non-empty action",
                    ));
                }
                options.audit_forbidden_actions.push(action);
                index += 2;
            }
            "--audit-require-live-actions" => {
                options.audit_require_live_actions = true;
                index += 1;
            }
            "--stdio-server" => {
                set_mode(
                    &mut options,
                    &mut mode_flag,
                    Mode::StdioServer,
                    "--stdio-server",
                )?;
                index += 1;
            }
            "--bot-io-timeout-ms" => {
                let value = args
                    .get(index + 1)
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        AppError::message(
                            EXIT_CONFIG,
                            "--bot-io-timeout-ms requires positive milliseconds",
                        )
                    })?;
                options.bot_io_timeout_ms = value;
                index += 2;
            }
            "--demo-events" => {
                options.demo_events = true;
                index += 1;
            }
            "--dry-run" => {
                options.dry_run = true;
                index += 1;
            }
            "--log-only" => {
                options.log_only = true;
                index += 1;
            }
            "--config" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| AppError::message(EXIT_CONFIG, "--config requires a path"))?;
                let config = Config::from_file(path)
                    .map_err(|error| AppError::message(EXIT_CONFIG, error.to_string()))?;
                emit_config_warnings(&config);
                options.dry_run = config.dry_run;
                options.log_only = config.log_only;
                options.runtime_settings = Some(RuntimeSettings::from_config(&config));
                options.audit_path = Some(config.audit_path.clone());
                options.audit_content_policy = audit_content_policy_from_config(&config)?;
                options.token_env = Some(config.token_env.clone());
                options.audit_hash_key_env = Some(config.audit_hash_key_env.clone());
                options.rest_base_url = Some(config.rest_base_url.clone());
                options.gateway_live = config.gateway_live;
                options.runtime_log_path = config.runtime_log_path.clone();
                index += 2;
            }
            "--audit-log" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| AppError::message(EXIT_CONFIG, "--audit-log requires a path"))?
                    .clone();
                options.audit_path = Some(path);
                index += 2;
            }
            "--runtime-log" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| AppError::message(EXIT_CONFIG, "--runtime-log requires a path"))?
                    .clone();
                options.runtime_log_path = Some(path);
                index += 2;
            }
            "--mock-gateway" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--mock-gateway requires a path")
                    })?
                    .clone();
                options.mock_gateway_path = Some(path);
                index += 2;
            }
            "--mock-rest" => {
                let url = args
                    .get(index + 1)
                    .ok_or_else(|| AppError::message(EXIT_CONFIG, "--mock-rest requires a URL"))?
                    .clone();
                options.rest_base_url = Some(url);
                index += 2;
            }
            "--rest-url" => {
                let url = args
                    .get(index + 1)
                    .ok_or_else(|| AppError::message(EXIT_CONFIG, "--rest-url requires a URL"))?
                    .clone();
                options.rest_base_url = Some(url);
                index += 2;
            }
            "--mock-rest-server" => {
                let addr = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--mock-rest-server requires HOST:PORT")
                    })?
                    .clone();
                set_mode(
                    &mut options,
                    &mut mode_flag,
                    Mode::MockRestServer,
                    "--mock-rest-server",
                )?;
                options.mock_rest_server_addr = Some(addr);
                index += 2;
            }
            "--mock-rest-max-requests" => {
                let max = args
                    .get(index + 1)
                    .and_then(|value| value.parse::<usize>().ok())
                    .ok_or_else(|| {
                        AppError::message(
                            EXIT_CONFIG,
                            "--mock-rest-max-requests requires a positive integer",
                        )
                    })?;
                if max == 0 {
                    return Err(AppError::message(
                        EXIT_CONFIG,
                        "--mock-rest-max-requests requires a positive integer",
                    ));
                }
                options.mock_rest_max_requests = Some(max);
                index += 2;
            }
            "--mock-gateway-url" => {
                let url = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--mock-gateway-url requires a URL")
                    })?
                    .clone();
                options.mock_gateway_url = Some(url);
                index += 2;
            }
            "--gateway-connect-once" => {
                set_mode(
                    &mut options,
                    &mut mode_flag,
                    Mode::GatewayConnectOnce,
                    "--gateway-connect-once",
                )?;
                index += 1;
            }
            "--gateway-live" => {
                options.gateway_live = true;
                index += 1;
            }
            "--shards" => {
                let shards = args
                    .get(index + 1)
                    .and_then(|value| value.parse::<u32>().ok())
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--shards requires a positive integer")
                    })?;
                if shards == 0 {
                    return Err(AppError::message(
                        EXIT_CONFIG,
                        "--shards requires a positive integer",
                    ));
                }
                options.supervisor_shards = Some(shards);
                index += 2;
            }
            "--shard-restart-limit" => {
                let limit = args
                    .get(index + 1)
                    .and_then(|value| value.parse::<u32>().ok())
                    .ok_or_else(|| {
                        AppError::message(
                            EXIT_CONFIG,
                            "--shard-restart-limit requires a non-negative integer",
                        )
                    })?;
                options.shard_restart_limit = Some(limit);
                index += 2;
            }
            "--gateway-session-dir" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--gateway-session-dir requires a path")
                    })?
                    .clone();
                options.gateway_session_dir = Some(path);
                index += 2;
            }
            "--hot-restart-file" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        AppError::message(EXIT_CONFIG, "--hot-restart-file requires a path")
                    })?
                    .clone();
                options.hot_restart_file = Some(path);
                index += 2;
            }
            "--bot" => {
                let command = args
                    .get(index + 1)
                    .ok_or_else(|| AppError::message(EXIT_CONFIG, "--bot requires a command"))?
                    .clone();
                let command_args = args[index + 2..].to_vec();
                set_mode(&mut options, &mut mode_flag, Mode::BotSupervisor, "--bot")?;
                bot_command = Some((command, command_args));
                break;
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--version" | "-V" => {
                print_version();
                return Ok(());
            }
            unknown => {
                return Err(AppError::message(
                    EXIT_CONFIG,
                    format!("unknown argument: {}", unknown),
                ));
            }
        }
    }

    if !options.audit_required_actions.is_empty() && options.mode != Mode::ValidateAuditLog {
        return Err(AppError::message(
            EXIT_CONFIG,
            "--audit-require-action requires --validate-audit-log",
        ));
    }
    if !options.audit_forbidden_actions.is_empty() && options.mode != Mode::ValidateAuditLog {
        return Err(AppError::message(
            EXIT_CONFIG,
            "--audit-forbid-action requires --validate-audit-log",
        ));
    }
    if options.audit_require_live_actions && options.mode != Mode::ValidateAuditLog {
        return Err(AppError::message(
            EXIT_CONFIG,
            "--audit-require-live-actions requires --validate-audit-log",
        ));
    }

    match options.mode {
        Mode::ValidateConfig => run_validate_config(options),
        Mode::ValidateAuditLog => run_validate_audit_log(options),
        Mode::ValidateRuntimeLog => run_validate_runtime_log(options),
        Mode::StdioServer => run_stdio_server(options),
        Mode::MockRestServer => run_mock_rest_server(options),
        Mode::GatewayConnectOnce => run_gateway_connect_once(options),
        Mode::BotSupervisor => {
            let Some((command, command_args)) = bot_command else {
                return Err(AppError::message(EXIT_CONFIG, "--bot command missing"));
            };
            if options.supervisor_shards.unwrap_or(1) > 1 {
                run_multi_shard_supervisor(options, &command, &command_args)
            } else {
                run_shard_supervisor_with_restarts(options, &command, &command_args)
            }
        }
    }
}

fn set_mode(
    options: &mut Options,
    mode_flag: &mut Option<&'static str>,
    mode: Mode,
    flag: &'static str,
) -> Result<(), AppError> {
    if let Some(previous) = *mode_flag {
        if options.mode != mode {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{flag} conflicts with {previous}"),
            ));
        }
        return Ok(());
    }
    options.mode = mode;
    *mode_flag = Some(flag);
    Ok(())
}

fn run_validate_config(options: Options) -> Result<(), AppError> {
    if options.runtime_settings.is_none() {
        return Err(AppError::message(
            EXIT_CONFIG,
            "--validate-config requires --config PATH",
        ));
    }
    validate_config_filesystem(&options)?;
    println!("dfor77 config ok");
    Ok(())
}

fn run_validate_audit_log(options: Options) -> Result<(), AppError> {
    let path = options
        .validate_audit_log_path
        .as_deref()
        .ok_or_else(|| AppError::message(EXIT_CONFIG, "--validate-audit-log requires a path"))?;
    let text = fs::read_to_string(path).map_err(|error| {
        AppError::message(
            EXIT_CONFIG,
            format!("audit log read failed for {path}: {error}"),
        )
    })?;
    let validation = validate_audit_log_text_with_options(
        &text,
        &AuditLogValidationOptions {
            required_actions: options.audit_required_actions,
            forbidden_actions: options.audit_forbidden_actions,
            require_live_actions: options.audit_require_live_actions,
        },
    )
    .map_err(|error| {
        AppError::message(
            EXIT_CONFIG,
            format!("audit log validation failed for {path}: {error}"),
        )
    })?;
    println!("dfor77 audit log ok: {} records", validation.records);
    Ok(())
}

fn run_validate_runtime_log(options: Options) -> Result<(), AppError> {
    let path = options
        .validate_runtime_log_path
        .as_deref()
        .ok_or_else(|| AppError::message(EXIT_CONFIG, "--validate-runtime-log requires a path"))?;
    let text = fs::read_to_string(path).map_err(|error| {
        AppError::message(
            EXIT_CONFIG,
            format!("runtime log read failed for {path}: {error}"),
        )
    })?;
    let validation = validate_runtime_log_text(&text).map_err(|error| {
        AppError::message(
            EXIT_CONFIG,
            format!("runtime log validation failed for {path}: {error}"),
        )
    })?;
    println!("dfor77 runtime log ok: {} records", validation.records);
    Ok(())
}

fn validate_config_filesystem(options: &Options) -> Result<(), AppError> {
    if let Some(path) = options.audit_path.as_deref() {
        validate_writable_file_path("audit.path", path)?;
    }
    if let Some(path) = options.runtime_log_path.as_deref() {
        validate_writable_file_path("runtime.log_path", path)?;
    }
    if let Some(path) = options.gateway_session_dir.as_deref() {
        validate_writable_directory_or_parent("--gateway-session-dir", path)?;
    }
    if let Some(path) = options.hot_restart_file.as_deref() {
        validate_writable_file_path("--hot-restart-file", path)?;
    }
    Ok(())
}

fn validate_writable_directory_or_parent(label: &str, value: &str) -> Result<(), AppError> {
    let path = Path::new(value);
    if path.as_os_str().is_empty() {
        return Err(AppError::message(
            EXIT_CONFIG,
            format!("{label} must not be empty"),
        ));
    }
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            return write_validate_probe(path).map_err(|error| {
                AppError::message(EXIT_CONFIG, format!("{label} is not writable: {error}"))
            });
        }
        Ok(_) => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} exists but is not a directory"),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} metadata failed: {error}"),
            ));
        }
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match fs::metadata(parent) {
        Ok(metadata) if metadata.is_dir() => write_validate_probe(parent).map_err(|error| {
            AppError::message(
                EXIT_CONFIG,
                format!("{label} parent is not writable: {error}"),
            )
        }),
        Ok(_) => Err(AppError::message(
            EXIT_CONFIG,
            format!("{label} parent is not a directory"),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(AppError::message(
            EXIT_CONFIG,
            format!("{label} parent does not exist"),
        )),
        Err(error) => Err(AppError::message(
            EXIT_CONFIG,
            format!("{label} parent metadata failed: {error}"),
        )),
    }
}

fn validate_writable_file_path(label: &str, value: &str) -> Result<(), AppError> {
    let path = Path::new(value);
    if path.as_os_str().is_empty() {
        return Err(AppError::message(
            EXIT_CONFIG,
            format!("{label} must not be empty"),
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} points to a directory"),
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} must be a regular file"),
            ));
        }
        Ok(_) => {
            dfor77::private_file::open_append(path, false).map_err(|error| {
                AppError::message(EXIT_CONFIG, format!("{label} is not writable: {error}"))
            })?;
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} metadata failed: {error}"),
            ));
        }
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match fs::metadata(parent) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} parent is not a directory"),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} parent does not exist"),
            ));
        }
        Err(error) => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("{label} parent metadata failed: {error}"),
            ));
        }
    }
    write_validate_probe(parent).map_err(|error| {
        AppError::message(
            EXIT_CONFIG,
            format!("{label} parent is not writable: {error}"),
        )
    })
}

fn write_validate_probe(parent: &Path) -> io::Result<()> {
    for attempt in 0..16 {
        let path = parent.join(format!(
            ".dfor77-validate-{}-{}-{}",
            std::process::id(),
            now_ms_u64(),
            attempt
        ));
        match write_validate_probe_once(&path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "validation probe path already exists",
    ))
}

fn write_validate_probe_once(path: &Path) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    let result = file.write_all(b"dfor77").and_then(|_| file.sync_all());
    drop(file);
    let remove_result = fs::remove_file(path);
    match (result, remove_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(_)) => Err(error),
    }
}

fn run_mock_rest_server(options: Options) -> Result<(), AppError> {
    let addr = options
        .mock_rest_server_addr
        .as_deref()
        .ok_or_else(|| AppError::message(EXIT_CONFIG, "--mock-rest-server address missing"))?;
    let listener = TcpListener::bind(addr).map_err(|error| {
        AppError::message(EXIT_CONFIG, format!("mock REST bind failed: {error}"))
    })?;
    let gateway_url = options
        .mock_gateway_url
        .unwrap_or_else(|| "ws://127.0.0.1:0/gateway".to_string());
    let max_requests = options.mock_rest_max_requests.unwrap_or(usize::MAX);
    for _ in 0..max_requests {
        let (mut stream, _) = listener.accept().map_err(|error| {
            AppError::message(EXIT_INTERNAL, format!("mock REST accept failed: {error}"))
        })?;
        handle_mock_rest_stream(&mut stream, &gateway_url)?;
    }
    Ok(())
}

fn run_stdio_server(options: Options) -> Result<(), AppError> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let runtime = SharedRuntime::new(make_runtime(options.clone())?);
    let settings = options.runtime_settings.clone().unwrap_or_default();
    let mut gateway_live = start_gateway_live(&options, Arc::clone(&runtime))?;
    loop {
        let request = read_request(&mut reader)
            .map_err(|error| AppError::message(EXIT_PROTOCOL, error.to_string()))?;
        let Some(request) = request else {
            return stop_gateway_live(gateway_live.take());
        };
        let end_request = request.verb == "END";
        let request_result =
            handle_runtime_request(&runtime, request, settings.gateway_next_event_timeout_ms)?;
        let RuntimeRequestResult::Response(response, rest_auth_fatal) = request_result else {
            return Err(AppError::message(
                EXIT_INTERNAL,
                "hot restart is unsupported in stdio server mode",
            ));
        };
        let end_success = end_request && response.status == D7OK;
        write_response(&mut writer, &response)
            .map_err(|error| AppError::message(EXIT_PROTOCOL, error.to_string()))?;
        writer
            .flush()
            .map_err(|error| AppError::message(EXIT_PROTOCOL, error.to_string()))?;
        if end_success {
            return stop_gateway_live(gateway_live.take());
        }
        if rest_auth_fatal {
            return Err(AppError::message(EXIT_REST_AUTH_FATAL, "REST auth fatal"));
        }
    }
}

fn run_bot_supervisor_once(
    options: Options,
    command: &str,
    args: &[String],
) -> Result<BotSupervisorExit, AppError> {
    let settings = options.runtime_settings.clone().unwrap_or_default();
    let mut hot_restart = HotRestartWatcher::from_options(&options)?;
    let mut command_builder = Command::new(command);
    let ipc_timeout = Duration::from_millis(options.bot_io_timeout_ms);
    let prepared_input = prepare_input(&mut command_builder, ipc_timeout)
        .map_err(|error| AppError::message(EXIT_CONFIG, error.to_string()))?;
    let prepared_output = prepare_output(&mut command_builder)
        .map_err(|error| AppError::message(EXIT_CONFIG, error.to_string()))?;
    strip_bot_secrets(&mut command_builder, &options);
    command_builder
        .args(args)
        .env("DFOR77_SHARD_ID", settings.gateway_shard_id.to_string())
        .env(
            "DFOR77_SHARD_COUNT",
            settings.gateway_shard_count.to_string(),
        )
        .env("GFORTRAN_UNBUFFERED_PRECONNECTED", "y")
        .env("GFORTRAN_UNBUFFERED_ALL", "y")
        .stderr(Stdio::inherit());
    let mut child = ManagedChild::new(command_builder.spawn().map_err(|error| {
        AppError::message(EXIT_BOT_CRASHED, format!("spawn failed: {}", error))
    })?);
    drop(command_builder);

    let stdout = take_output(prepared_output, &mut child)
        .map_err(|error| AppError::message(EXIT_INTERNAL, error.to_string()))?;
    let interrupt = stdout
        .interrupt_handle()
        .map_err(|error| AppError::message(EXIT_INTERNAL, error.to_string()))?;
    let stdin = take_input(prepared_input, &mut child)
        .map_err(|error| AppError::message(EXIT_INTERNAL, error.to_string()))?;
    let mut writer = BufWriter::new(stdin);
    let mut request_reader = start_bot_request_reader(stdout);
    request_reader.interrupt = interrupt;
    let runtime = SharedRuntime::new(make_runtime(options.clone())?);
    {
        let mut runtime = lock_runtime(&runtime)?;
        runtime.set_bot_process_pid(child.id());
    }
    log_runtime_process_lifecycle(
        &runtime,
        "bot_start",
        &[
            RuntimeLogField::u32("bot_process_pid", child.id()),
            RuntimeLogField::string("command", command),
            RuntimeLogField::usize("args_count", args.len()),
        ],
    );
    let mut gateway_live = start_gateway_live(&options, Arc::clone(&runtime))?;

    loop {
        if let Err(error) = check_gateway_live_finished(&mut gateway_live) {
            drop(writer);
            let _ = stop_child_for_hot_restart(&mut child);
            request_reader.detach();
            return Err(error);
        }
        let stopping = supervisor_stopping(&options);
        if stopping || hot_restart.changed()? {
            log_runtime_process_lifecycle(
                &runtime,
                "hot_restart",
                &[RuntimeLogField::u32("bot_process_pid", child.id())],
            );
            let stop_result = stop_gateway_live(gateway_live.take());
            drop(writer);
            let child_result = stop_child_for_hot_restart(&mut child);
            stop_result?;
            child_result?;
            request_reader.detach();
            return Ok(if stopping {
                BotSupervisorExit::Finished
            } else {
                BotSupervisorExit::HotRestart
            });
        }
        let request_read = match request_reader.recv_timeout(Duration::from_millis(20)) {
            Ok(read) => read,
            Err(error) => {
                let _ = stop_gateway_live(gateway_live.take());
                drop(writer);
                let _ = stop_child_for_hot_restart(&mut child);
                request_reader.detach();
                return Err(error);
            }
        };
        let request = match request_read {
            BotRequestRead::Pending => match child.try_wait() {
                Ok(Some(_)) => None,
                Ok(None) => continue,
                Err(error) => {
                    return Err(AppError::message(EXIT_BOT_CRASHED, error.to_string()));
                }
            },
            BotRequestRead::Eof => None,
            BotRequestRead::Request(request) => Some(request),
        };
        let Some(request) = request else {
            drop(writer);
            let status = wait_for_exit(&mut child, ipc_timeout).map_err(|error| {
                AppError::message(EXIT_BOT_CRASHED, format!("wait failed: {}", error))
            })?;
            request_reader.join()?;
            log_runtime_process_lifecycle(
                &runtime,
                "bot_exit",
                &[
                    RuntimeLogField::u32("bot_process_pid", child.id()),
                    RuntimeLogField::bool("success", status.success()),
                    RuntimeLogField::i32("exit_code", status.code().unwrap_or(-1)),
                ],
            );
            if status.success() {
                stop_gateway_live(gateway_live.take())?;
                return Ok(BotSupervisorExit::Finished);
            }
            let _ = stop_gateway_live(gateway_live.take());
            return Err(AppError::Exit(EXIT_BOT_CRASHED));
        };
        let end_request = request.verb == "END";
        let request_result = handle_runtime_request_with_hot_restart(
            &runtime,
            request,
            settings.gateway_next_event_timeout_ms,
            &mut hot_restart,
            options.supervisor_stop.as_deref(),
        )?;
        let RuntimeRequestResult::Response(response, rest_auth_fatal) = request_result else {
            let stop_result = stop_gateway_live(gateway_live.take());
            drop(writer);
            let child_result = stop_child_for_hot_restart(&mut child);
            stop_result?;
            child_result?;
            request_reader.detach();
            return Ok(BotSupervisorExit::HotRestart);
        };
        let end_success = end_request && response.status == D7OK;
        writer
            .get_mut()
            .begin_response()
            .map_err(|error| AppError::message(EXIT_PROTOCOL, error.to_string()))?;
        write_response(&mut writer, &response).map_err(bot_ipc_error)?;
        writer
            .flush()
            .map_err(|error| AppError::message(EXIT_BOT_CRASHED, error.to_string()))?;
        if end_success {
            let stop_result = stop_gateway_live(gateway_live.take());
            drop(writer);
            let status = wait_for_exit(&mut child, ipc_timeout).map_err(|error| {
                AppError::message(EXIT_BOT_CRASHED, format!("wait failed: {}", error))
            })?;
            request_reader.join()?;
            log_runtime_process_lifecycle(
                &runtime,
                "bot_exit",
                &[
                    RuntimeLogField::u32("bot_process_pid", child.id()),
                    RuntimeLogField::bool("success", status.success()),
                    RuntimeLogField::i32("exit_code", status.code().unwrap_or(-1)),
                ],
            );
            stop_result?;
            if status.success() {
                return Ok(BotSupervisorExit::Finished);
            }
            return Err(AppError::Exit(EXIT_BOT_CRASHED));
        }
        if rest_auth_fatal {
            let _ = stop_gateway_live(gateway_live.take());
            drop(writer);
            let _ = stop_child_for_hot_restart(&mut child);
            request_reader.detach();
            return Err(AppError::message(EXIT_REST_AUTH_FATAL, "REST auth fatal"));
        }
    }
}

fn run_multi_shard_supervisor(
    mut options: Options,
    command: &str,
    args: &[String],
) -> Result<(), AppError> {
    let shard_count = options.supervisor_shards.unwrap_or(1);
    if shard_count <= 1 {
        return run_shard_supervisor_with_restarts(options, command, args);
    }
    let stop = Arc::new(AtomicBool::new(false));
    options.supervisor_stop = Some(Arc::clone(&stop));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    let mut failure = None;
    for shard_id in 0..shard_count {
        let tx = tx.clone();
        let shard_options = options_for_shard(&options, shard_id, shard_count);
        let command = command.to_string();
        let args = args.to_vec();
        let handle = thread::Builder::new().spawn(move || {
            let result = std::panic::catch_unwind(|| {
                let delay_ms = u64::from(shard_id).saturating_mul(5_000);
                if supervisor_delay(&shard_options, Duration::from_millis(delay_ms)) {
                    return Ok(());
                }
                run_shard_supervisor_with_restarts(shard_options, &command, &args)
            })
            .unwrap_or_else(|_| {
                Err(AppError::message(
                    EXIT_INTERNAL,
                    "shard supervisor panicked",
                ))
            });
            let _ = tx.send((shard_id, result));
        });
        match handle {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                stop.store(true, Ordering::Relaxed);
                failure = Some(AppError::message(
                    EXIT_INTERNAL,
                    format!("shard supervisor spawn failed: {error}"),
                ));
                break;
            }
        }
    }
    drop(tx);
    for _ in 0..handles.len() {
        let Some((shard_id, result)) = rx.recv().ok() else {
            stop.store(true, Ordering::Relaxed);
            failure = Some(AppError::message(
                EXIT_INTERNAL,
                "multi-shard supervisor failed to receive shard result",
            ));
            break;
        };
        match result {
            Ok(()) => {}
            Err(error) => {
                stop.store(true, Ordering::Relaxed);
                if failure.is_none() {
                    failure = Some(match error {
                        AppError::Message(code, message) => AppError::Message(
                            code,
                            format!("shard {} failed: {}", shard_id, message),
                        ),
                        error => error,
                    });
                }
            }
        }
    }
    for handle in handles {
        if handle.join().is_err() && failure.is_none() {
            failure = Some(AppError::message(
                EXIT_INTERNAL,
                "shard supervisor panicked",
            ));
        }
    }
    failure.map_or(Ok(()), Err)
}

fn supervisor_stopping(options: &Options) -> bool {
    options
        .supervisor_stop
        .as_ref()
        .is_some_and(|stop| stop.load(Ordering::Relaxed))
}

fn supervisor_delay(options: &Options, duration: Duration) -> bool {
    let started = Instant::now();
    loop {
        if supervisor_stopping(options) {
            return true;
        }
        let remaining = duration.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn run_shard_supervisor_with_restarts(
    options: Options,
    command: &str,
    args: &[String],
) -> Result<(), AppError> {
    let restart_limit = options.shard_restart_limit.unwrap_or(0);
    let mut restarts = 0;
    loop {
        if supervisor_stopping(&options) {
            return Ok(());
        }
        match run_bot_supervisor_once(options.clone(), command, args) {
            Ok(BotSupervisorExit::Finished) => return Ok(()),
            Ok(BotSupervisorExit::HotRestart) => {
                if supervisor_stopping(&options) {
                    return Ok(());
                }
                let shard_id = options
                    .runtime_settings
                    .as_ref()
                    .map(|settings| settings.gateway_shard_id)
                    .unwrap_or(0);
                write_diagnostic(format_args!(
                    "dfor77 info shard {shard_id}: hot restart requested"
                ));
                if supervisor_delay(&options, Duration::from_millis(500)) {
                    return Ok(());
                }
            }
            Err(error) if restarts < restart_limit && shard_error_restartable(&error) => {
                restarts += 1;
                let shard_id = options
                    .runtime_settings
                    .as_ref()
                    .map(|settings| settings.gateway_shard_id)
                    .unwrap_or(0);
                write_diagnostic(format_args!(
                    "dfor77 warn shard {shard_id}: restarting after failure ({restarts}/{restart_limit}) reason={}",
                    app_error_log_text(&error)
                ));
                if supervisor_delay(
                    &options,
                    Duration::from_millis(1_000_u64.saturating_mul(u64::from(restarts))),
                ) {
                    return Ok(());
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn shard_error_restartable(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Exit(EXIT_BOT_CRASHED)
            | AppError::Message(EXIT_BOT_CRASHED, _)
            | AppError::Message(EXIT_GATEWAY_FATAL, _)
    )
}

fn options_for_shard(options: &Options, shard_id: u32, shard_count: u32) -> Options {
    let mut shard_options = options.clone();
    let mut settings = shard_options.runtime_settings.clone().unwrap_or_default();
    settings.gateway_shard_id = shard_id;
    settings.gateway_shard_count = shard_count;
    shard_options.runtime_settings = Some(settings);
    shard_options.gateway_session_path = options
        .gateway_session_dir
        .as_deref()
        .map(|dir| shard_session_path(dir, shard_id));
    shard_options.hot_restart_shard_file = options
        .hot_restart_file
        .as_deref()
        .filter(|_| shard_count > 1)
        .map(|path| hot_restart_shard_path(path, shard_id));
    shard_options
}

fn shard_session_path(dir: &str, shard_id: u32) -> String {
    PathBuf::from(dir)
        .join(format!("shard-{}.json", shard_id))
        .to_string_lossy()
        .into_owned()
}

fn hot_restart_shard_path(path: &str, shard_id: u32) -> String {
    format!("{path}.shard-{shard_id}")
}

fn hot_restart_paths(options: &Options) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = &options.hot_restart_file {
        paths.push(path.clone());
    }
    if let Some(path) = &options.hot_restart_shard_file {
        if !paths.iter().any(|existing| existing == path) {
            paths.push(path.clone());
        }
    }
    paths
}

fn gateway_session_path(options: &Options) -> Option<String> {
    if let Some(path) = &options.gateway_session_path {
        return Some(path.clone());
    }
    let dir = options.gateway_session_dir.as_deref()?;
    let settings = options.runtime_settings.clone().unwrap_or_default();
    Some(shard_session_path(dir, settings.gateway_shard_id))
}

fn gateway_live_save_session(
    runtime: &RuntimeHandle,
    path: Option<&str>,
) -> Option<GatewayLiveExit> {
    let path = path?;
    save_gateway_resume_session(runtime, path)
        .err()
        .map(GatewayLiveExit::from_app_error)
}

fn handle_runtime_request(
    runtime: &RuntimeHandle,
    request: Request,
    next_event_timeout_ms: u64,
) -> Result<RuntimeRequestResult, AppError> {
    let mut watcher = HotRestartWatcher::new(Vec::new())?;
    handle_runtime_request_with_hot_restart(
        runtime,
        request,
        next_event_timeout_ms,
        &mut watcher,
        None,
    )
}

fn handle_runtime_request_with_hot_restart(
    runtime: &RuntimeHandle,
    request: Request,
    next_event_timeout_ms: u64,
    hot_restart: &mut HotRestartWatcher,
    supervisor_stop: Option<&AtomicBool>,
) -> Result<RuntimeRequestResult, AppError> {
    if request.verb != "NXT" {
        let (response, rest_auth_fatal) = handle_runtime_request_once(runtime, request)?;
        return Ok(RuntimeRequestResult::Response(response, rest_auth_fatal));
    }
    let deadline = if next_event_timeout_ms == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(next_event_timeout_ms))
    };
    loop {
        if supervisor_stop.is_some_and(|stop| stop.load(Ordering::Relaxed))
            || hot_restart.changed()?
        {
            return Ok(RuntimeRequestResult::HotRestart);
        }
        let mut guard = lock_runtime(runtime)?;
        let response = guard.handle(request.clone());
        let rest_auth_fatal = guard.rest_auth_fatal();
        if response.status != D7TIM || rest_auth_fatal {
            return Ok(RuntimeRequestResult::Response(response, rest_auth_fatal));
        }
        let wait_for = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(RuntimeRequestResult::Response(response, rest_auth_fatal));
                }
                (deadline - now).min(Duration::from_millis(250))
            }
            None => Duration::from_millis(250),
        };
        drop(
            runtime
                .event_cv
                .wait_timeout(guard, wait_for)
                .map_err(|_| AppError::message(EXIT_INTERNAL, "runtime lock poisoned"))?,
        );
    }
}

fn handle_runtime_request_once(
    runtime: &RuntimeHandle,
    request: Request,
) -> Result<(Response, bool), AppError> {
    let mut runtime = lock_runtime(runtime)?;
    let response = runtime.handle(request);
    let rest_auth_fatal = runtime.rest_auth_fatal();
    Ok((response, rest_auth_fatal))
}

fn run_gateway_connect_once(options: Options) -> Result<(), AppError> {
    if options.rest_base_url.is_none() {
        return Err(AppError::message(
            EXIT_CONFIG,
            "--gateway-connect-once requires --rest-url URL or --mock-rest URL",
        ));
    }
    let token = discord_token(&options)?;
    let settings = options.runtime_settings.clone().unwrap_or_default();
    let mut runtime = make_runtime(options)?;
    let mut transport = TungsteniteGatewayTransport::new(settings.rest_http_timeout_ms);
    let result = runtime.connect_gateway_once(&mut transport, &token, now_ms_u64());
    let next_action = gateway_action_detail(&result.last_action);
    let mut fields = vec![
        RuntimeLogField::string("kind", "gateway_transition"),
        RuntimeLogField::string("phase", "connect_once_result"),
        RuntimeLogField::i32("status", result.status),
        RuntimeLogField::string("why", &result.why),
        RuntimeLogField::bool("connected", result.connected),
        RuntimeLogField::bool("identify_sent", result.identify_sent),
        RuntimeLogField::usize("received_frames", result.received_frames),
        RuntimeLogField::string("next_action", &next_action),
        RuntimeLogField::u32("daily_identify_count", result.startup.daily_identify_count),
        RuntimeLogField::u32(
            "daily_identify_remaining",
            result.startup.daily_identify_remaining,
        ),
    ];
    push_runtime_usize_option(
        &mut fields,
        "gateway_outbound_remaining",
        result.gateway_outbound_remaining,
    );
    let _ = runtime.write_runtime_event("runtime", &fields);
    emit_gateway_daily_identify_warning(&result.startup);
    println!("{}", gateway_connect_json(&result));
    if result.status == D7OK {
        Ok(())
    } else {
        Err(AppError::Exit(gateway_exit_code(
            result.status,
            &result.why,
        )))
    }
}

fn start_gateway_live(
    options: &Options,
    runtime: RuntimeHandle,
) -> Result<Option<GatewayLive>, AppError> {
    if !options.gateway_live {
        return Ok(None);
    }
    if options.rest_base_url.is_none() {
        return Err(AppError::message(
            EXIT_CONFIG,
            "--gateway-live requires --rest-url URL, --mock-rest URL, or --config PATH",
        ));
    }
    let token = discord_token(options)?;
    let settings = options.runtime_settings.clone().unwrap_or_default();
    let timeout_ms = gateway_live_timeout_ms(&settings);
    let mut transport = TungsteniteGatewayTransport::new(settings.rest_http_timeout_ms);
    let stop = Arc::new(AtomicBool::new(false));
    let session_path = gateway_session_path(options);
    write_diagnostic(format_args!(
        "dfor77 info gateway live transition phase=start shard={} shard_count={} timeout_ms={} session_path={}",
        settings.gateway_shard_id,
        settings.gateway_shard_count,
        timeout_ms,
        log_option(session_path.as_deref())
    ));
    let mut start_fields = vec![
        RuntimeLogField::u32("shard_id", settings.gateway_shard_id),
        RuntimeLogField::u32("shard_count", settings.gateway_shard_count),
        RuntimeLogField::u64("timeout_ms", timeout_ms),
    ];
    push_runtime_string_option(&mut start_fields, "session_path", session_path.as_deref());
    log_runtime_gateway_transition(&runtime, "start", &start_fields);
    if let Some(path) = session_path.as_deref() {
        if let Some(session) = load_gateway_resume_session(path)? {
            write_diagnostic(format_args!(
                "dfor77 info gateway live transition phase=restore result=loaded shard={} seq={} bot={} path={}",
                settings.gateway_shard_id,
                session.last_seen_seq,
                log_option(session.bot_user_id.as_deref()),
                diagnostic_field(path)
            ));
            let mut restore_fields = vec![
                RuntimeLogField::string("result", "loaded"),
                RuntimeLogField::u32("shard_id", settings.gateway_shard_id),
                RuntimeLogField::i64("seq", session.last_seen_seq),
                RuntimeLogField::string("path", path),
            ];
            push_runtime_string_option(
                &mut restore_fields,
                "bot_user_id",
                session.bot_user_id.as_deref(),
            );
            log_runtime_gateway_transition(&runtime, "restore", &restore_fields);
            let restored = {
                let mut runtime = lock_runtime(&runtime)?;
                runtime.restore_gateway_resume_session(session)
            };
            match restored {
                Ok(action) => {
                    let next_action = gateway_action_detail(&action);
                    write_diagnostic(format_args!(
                        "dfor77 info gateway live transition phase=restore result=accepted next_action={}",
                        next_action
                    ));
                    log_runtime_gateway_transition(
                        &runtime,
                        "restore",
                        &[
                            RuntimeLogField::string("result", "accepted"),
                            RuntimeLogField::string("next_action", &next_action),
                        ],
                    );
                    if let Some(exit) = handle_gateway_live_action(
                        &runtime,
                        &mut transport,
                        &token,
                        action,
                        &stop,
                        session_path.as_deref(),
                    ) {
                        return Err(AppError::message(
                            exit.status,
                            format!("gateway live resume failed: {}", exit.why),
                        ));
                    }
                }
                Err(why) => {
                    write_diagnostic(format_args!(
                        "dfor77 warn gateway live transition phase=restore result=ignored why={why}"
                    ));
                    log_runtime_gateway_transition(
                        &runtime,
                        "restore",
                        &[
                            RuntimeLogField::string("result", "ignored"),
                            RuntimeLogField::string("why", why),
                        ],
                    );
                }
            }
        }
    }
    let has_resume_session = {
        let runtime = lock_runtime(&runtime)?;
        runtime.gateway_resume_session().is_some()
    };
    if !has_resume_session {
        write_diagnostic(format_args!(
            "dfor77 info gateway live transition phase=identify_start reason=no_resume_session shard={}",
            settings.gateway_shard_id
        ));
        log_runtime_gateway_transition(
            &runtime,
            "identify_start",
            &[
                RuntimeLogField::string("reason", "no_resume_session"),
                RuntimeLogField::u32("shard_id", settings.gateway_shard_id),
            ],
        );
        let result = {
            let mut runtime = lock_runtime(&runtime)?;
            runtime.connect_gateway_once(&mut transport, &token, now_ms_u64())
        };
        log_gateway_connect_result(&runtime, "identify", &result);
        if result.received_frames > 0
            || result.status != D7OK
            || result.last_action != GatewayAction::None
        {
            runtime.notify_events();
        }
        if let Some(exit) = handle_gateway_live_connect_result(
            &runtime,
            &mut transport,
            &token,
            result,
            &stop,
            session_path.as_deref(),
        ) {
            return Err(AppError::message(
                exit.status,
                format!("gateway live connect failed: {}", exit.why),
            ));
        }
    }
    if let Some(exit) = gateway_live_save_session(&runtime, session_path.as_deref()) {
        return Err(AppError::message(
            exit.status,
            format!("gateway live session save failed: {}", exit.why),
        ));
    }
    transport.set_timeout_ms(timeout_ms);
    let stop_thread = Arc::clone(&stop);
    let session_path_thread = session_path.clone();
    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            let result = match runtime.lock() {
                Ok(mut runtime) => runtime.pump_gateway_once(&mut transport, now_ms_u64()),
                Err(_) => {
                    return GatewayLiveExit {
                        status: EXIT_INTERNAL,
                        why: "RUNTIME_LOCK_POISONED".to_string(),
                    };
                }
            };
            if let Some(exit) = gateway_live_save_session(&runtime, session_path_thread.as_deref())
            {
                return exit;
            }
            if result.status != D7OK || result.action != GatewayAction::None {
                log_gateway_pump_result(&runtime, &result);
            }
            if result.received_frame
                || result.status != D7OK
                || result.action != GatewayAction::None
            {
                runtime.notify_events();
            }
            if result.status != D7OK {
                let action = gateway_action_detail(&result.action);
                write_diagnostic(format_args!(
                    "dfor77 warn gateway live transition phase=exit status={} why={} action={}",
                    result.status, result.why, action
                ));
                log_runtime_gateway_transition(
                    &runtime,
                    "exit",
                    &[
                        RuntimeLogField::i32("status", result.status),
                        RuntimeLogField::string("why", &result.why),
                        RuntimeLogField::string("action", &action),
                    ],
                );
                return GatewayLiveExit {
                    status: gateway_exit_code(result.status, &result.why),
                    why: result.why,
                };
            }
            if let Some(exit) = handle_gateway_live_action(
                &runtime,
                &mut transport,
                &token,
                result.action,
                &stop_thread,
                session_path_thread.as_deref(),
            ) {
                if let Some(save_exit) =
                    gateway_live_save_session(&runtime, session_path_thread.as_deref())
                {
                    return save_exit;
                }
                return exit;
            }
            thread::sleep(Duration::from_millis(timeout_ms));
        }
        let exit = gateway_live_graceful_shutdown(&runtime, &mut transport);
        if let Some(save_exit) = gateway_live_save_session(&runtime, session_path_thread.as_deref())
        {
            return save_exit;
        }
        exit
    });
    Ok(Some(GatewayLive {
        stop,
        handle: Some(handle),
    }))
}

fn handle_gateway_live_action(
    runtime: &RuntimeHandle,
    transport: &mut TungsteniteGatewayTransport,
    token: &str,
    action: GatewayAction,
    stop: &AtomicBool,
    session_path: Option<&str>,
) -> Option<GatewayLiveExit> {
    if let Some(exit) = gateway_live_save_session(runtime, session_path) {
        return Some(exit);
    }
    match action {
        GatewayAction::None | GatewayAction::SendHeartbeat | GatewayAction::CloseAndResume => None,
        GatewayAction::SendResume { seq } => {
            write_diagnostic(format_args!(
                "dfor77 info gateway live transition phase=resume_start seq={seq}"
            ));
            log_runtime_gateway_transition(
                runtime,
                "resume_start",
                &[RuntimeLogField::i64("seq", seq)],
            );
            let result = match runtime.lock() {
                Ok(mut runtime) => runtime.resume_gateway_once(transport, token, seq, now_ms_u64()),
                Err(_) => {
                    return Some(GatewayLiveExit {
                        status: EXIT_INTERNAL,
                        why: "RUNTIME_LOCK_POISONED".to_string(),
                    });
                }
            };
            log_gateway_resume_result(runtime, &result);
            if let Some(exit) = gateway_live_save_session(runtime, session_path) {
                return Some(exit);
            }
            if result.received_frame
                || result.status != D7OK
                || result.action != GatewayAction::None
            {
                runtime.notify_events();
            }
            if let Some(exit) = gateway_live_exit_for_result(result.status, result.why) {
                return Some(exit);
            }
            handle_gateway_live_action(runtime, transport, token, result.action, stop, session_path)
        }
        action @ (GatewayAction::FetchGatewayBot | GatewayAction::SendIdentify) => {
            let reason = match action {
                GatewayAction::FetchGatewayBot => "refetch_gateway_bot",
                _ => "gateway_action",
            };
            let action_detail = gateway_action_detail(&action);
            write_diagnostic(format_args!(
                "dfor77 info gateway live transition phase=identify_start reason={} action={}",
                reason, action_detail
            ));
            log_runtime_gateway_transition(
                runtime,
                "identify_start",
                &[
                    RuntimeLogField::string("reason", reason),
                    RuntimeLogField::string("action", &action_detail),
                ],
            );
            let result = match runtime.lock() {
                Ok(mut runtime) => runtime.connect_gateway_once(transport, token, now_ms_u64()),
                Err(_) => {
                    return Some(GatewayLiveExit {
                        status: EXIT_INTERNAL,
                        why: "RUNTIME_LOCK_POISONED".to_string(),
                    });
                }
            };
            log_gateway_connect_result(runtime, "identify", &result);
            if result.received_frames > 0
                || result.status != D7OK
                || result.last_action != GatewayAction::None
            {
                runtime.notify_events();
            }
            handle_gateway_live_connect_result(
                runtime,
                transport,
                token,
                result,
                stop,
                session_path,
            )
        }
        GatewayAction::Backoff { delay_ms } => {
            let info = gateway_backoff_info(runtime);
            log_gateway_backoff_start(delay_ms, info);
            log_runtime_gateway_backoff_start(runtime, delay_ms, info);
            if !gateway_live_wait_for_backoff(stop, gateway_live_backoff_delay_ms(delay_ms)) {
                return Some(gateway_live_graceful_shutdown(runtime, transport));
            }
            let next = match runtime.lock() {
                Ok(mut runtime) => runtime.gateway_backoff_elapsed(),
                Err(_) => {
                    return Some(GatewayLiveExit {
                        status: EXIT_INTERNAL,
                        why: "RUNTIME_LOCK_POISONED".to_string(),
                    });
                }
            };
            let next_action = gateway_action_detail(&next);
            write_diagnostic(format_args!(
                "dfor77 info gateway live transition phase=backoff_elapsed next_action={}",
                next_action
            ));
            log_runtime_gateway_transition(
                runtime,
                "backoff_elapsed",
                &[RuntimeLogField::string("next_action", &next_action)],
            );
            if next != GatewayAction::None {
                runtime.notify_events();
            }
            handle_gateway_live_action(runtime, transport, token, next, stop, session_path)
        }
        GatewayAction::Fatal(reason) => Some(GatewayLiveExit {
            status: EXIT_GATEWAY_FATAL,
            why: reason.to_string(),
        }),
    }
}

fn handle_gateway_live_connect_result(
    runtime: &RuntimeHandle,
    transport: &mut TungsteniteGatewayTransport,
    token: &str,
    result: GatewayConnectResult,
    stop: &AtomicBool,
    session_path: Option<&str>,
) -> Option<GatewayLiveExit> {
    if let Some(exit) = gateway_live_save_session(runtime, session_path) {
        return Some(exit);
    }
    emit_gateway_daily_identify_warning(&result.startup);
    if result.status != D7OK {
        return Some(GatewayLiveExit {
            status: gateway_exit_code(result.status, &result.why),
            why: result.why,
        });
    }
    handle_gateway_live_action(
        runtime,
        transport,
        token,
        result.last_action,
        stop,
        session_path,
    )
}

fn emit_gateway_daily_identify_warning(startup: &GatewayStartup) {
    if startup.daily_identify_warning {
        write_diagnostic(format_args!(
            "dfor77 warn gateway: daily identify count {} reached warning threshold; remaining {}",
            startup.daily_identify_count, startup.daily_identify_remaining
        ));
    }
}

fn log_gateway_connect_result(runtime: &RuntimeHandle, phase: &str, result: &GatewayConnectResult) {
    let next_action = gateway_action_detail(&result.last_action);
    write_diagnostic(format_args!(
        "dfor77 info gateway live transition phase={phase}_result status={} why={} connected={} identify_sent={} frames={} next_action={} outbound={} identify_count={} identify_remaining={}",
        result.status,
        result.why,
        json_bool(result.connected),
        json_bool(result.identify_sent),
        result.received_frames,
        next_action,
        json_usize_option(result.gateway_outbound_remaining),
        result.startup.daily_identify_count,
        result.startup.daily_identify_remaining
    ));
    let mut fields = vec![
        RuntimeLogField::i32("status", result.status),
        RuntimeLogField::string("why", &result.why),
        RuntimeLogField::bool("connected", result.connected),
        RuntimeLogField::bool("identify_sent", result.identify_sent),
        RuntimeLogField::usize("received_frames", result.received_frames),
        RuntimeLogField::string("next_action", &next_action),
        RuntimeLogField::u32("daily_identify_count", result.startup.daily_identify_count),
        RuntimeLogField::u32(
            "daily_identify_remaining",
            result.startup.daily_identify_remaining,
        ),
    ];
    push_runtime_usize_option(
        &mut fields,
        "gateway_outbound_remaining",
        result.gateway_outbound_remaining,
    );
    let event_phase = format!("{phase}_result");
    log_runtime_gateway_transition(runtime, &event_phase, &fields);
}

fn log_gateway_resume_result(runtime: &RuntimeHandle, result: &GatewayPumpResult) {
    let next_action = gateway_action_detail(&result.action);
    write_diagnostic(format_args!(
        "dfor77 info gateway live transition phase=resume_result status={} why={} frame={} next_action={} outbound={}",
        result.status,
        result.why,
        json_bool(result.received_frame),
        next_action,
        json_usize_option(result.gateway_outbound_remaining)
    ));
    let mut fields = vec![
        RuntimeLogField::i32("status", result.status),
        RuntimeLogField::string("why", &result.why),
        RuntimeLogField::bool("received_frame", result.received_frame),
        RuntimeLogField::string("next_action", &next_action),
    ];
    push_runtime_usize_option(
        &mut fields,
        "gateway_outbound_remaining",
        result.gateway_outbound_remaining,
    );
    log_runtime_gateway_transition(runtime, "resume_result", &fields);
}

fn log_gateway_pump_result(runtime: &RuntimeHandle, result: &GatewayPumpResult) {
    let next_action = gateway_action_detail(&result.action);
    write_diagnostic(format_args!(
        "dfor77 info gateway live transition phase=pump status={} why={} heartbeat={} frame={} next_action={} outbound={}",
        result.status,
        result.why,
        json_bool(result.heartbeat_sent),
        json_bool(result.received_frame),
        next_action,
        json_usize_option(result.gateway_outbound_remaining)
    ));
    let mut fields = vec![
        RuntimeLogField::i32("status", result.status),
        RuntimeLogField::string("why", &result.why),
        RuntimeLogField::bool("heartbeat_sent", result.heartbeat_sent),
        RuntimeLogField::bool("received_frame", result.received_frame),
        RuntimeLogField::string("next_action", &next_action),
    ];
    push_runtime_usize_option(
        &mut fields,
        "gateway_outbound_remaining",
        result.gateway_outbound_remaining,
    );
    log_runtime_gateway_transition(runtime, "pump", &fields);
}

fn gateway_backoff_info(runtime: &RuntimeHandle) -> Option<GatewayBackoffInfo> {
    runtime
        .lock()
        .ok()
        .and_then(|runtime| runtime.gateway_backoff_info())
}

fn log_runtime_event(runtime: &RuntimeHandle, event: &str, fields: &[RuntimeLogField<'_>]) {
    if let Ok(mut runtime) = runtime.lock() {
        let _ = runtime.write_runtime_event(event, fields);
    }
}

fn log_runtime_gateway_transition(
    runtime: &RuntimeHandle,
    phase: &str,
    fields: &[RuntimeLogField<'_>],
) {
    let mut event_fields = vec![
        RuntimeLogField::string("kind", "gateway_transition"),
        RuntimeLogField::string("phase", phase),
    ];
    event_fields.extend_from_slice(fields);
    log_runtime_event(runtime, "runtime", &event_fields);
}

fn log_runtime_process_lifecycle(
    runtime: &RuntimeHandle,
    phase: &str,
    fields: &[RuntimeLogField<'_>],
) {
    let mut event_fields = vec![
        RuntimeLogField::string("kind", "process_lifecycle"),
        RuntimeLogField::string("phase", phase),
    ];
    event_fields.extend_from_slice(fields);
    log_runtime_event(runtime, "runtime", &event_fields);
}

fn push_runtime_usize_option<'a>(
    fields: &mut Vec<RuntimeLogField<'a>>,
    key: &'a str,
    value: Option<usize>,
) {
    match value {
        Some(value) => fields.push(RuntimeLogField::usize(key, value)),
        None => fields.push(RuntimeLogField::null(key)),
    }
}

fn push_runtime_string_option<'a>(
    fields: &mut Vec<RuntimeLogField<'a>>,
    key: &'a str,
    value: Option<&'a str>,
) {
    match value {
        Some(value) => fields.push(RuntimeLogField::string(key, value)),
        None => fields.push(RuntimeLogField::null(key)),
    }
}

fn log_gateway_backoff_start(delay_ms: u64, info: Option<GatewayBackoffInfo>) {
    match info {
        Some(info) => write_diagnostic(format_args!(
            "dfor77 warn gateway live transition phase=backoff reason={} next={} delay_ms={} action_delay_ms={}",
            info.reason,
            gateway_backoff_next_text(info.next),
            info.delay_ms,
            delay_ms
        )),
        None => write_diagnostic(format_args!(
            "dfor77 warn gateway live transition phase=backoff reason=unknown next=unknown delay_ms={delay_ms} action_delay_ms={delay_ms}"
        )),
    }
}

fn log_runtime_gateway_backoff_start(
    runtime: &RuntimeHandle,
    delay_ms: u64,
    info: Option<GatewayBackoffInfo>,
) {
    let mut fields = vec![RuntimeLogField::u64("action_delay_ms", delay_ms)];
    match info {
        Some(info) => {
            fields.push(RuntimeLogField::string("reason", info.reason));
            fields.push(RuntimeLogField::string(
                "next",
                gateway_backoff_next_text(info.next),
            ));
            fields.push(RuntimeLogField::u64("delay_ms", info.delay_ms));
        }
        None => {
            fields.push(RuntimeLogField::string("reason", "unknown"));
            fields.push(RuntimeLogField::string("next", "unknown"));
            fields.push(RuntimeLogField::u64("delay_ms", delay_ms));
        }
    }
    log_runtime_gateway_transition(runtime, "backoff", &fields);
}

fn gateway_backoff_next_text(next: GatewayBackoffNext) -> &'static str {
    match next {
        GatewayBackoffNext::Resume => "Resume",
        GatewayBackoffNext::NewIdentify => "NewIdentify",
        GatewayBackoffNext::RefetchGatewayBot => "RefetchGatewayBot",
        GatewayBackoffNext::Shutdown => "Shutdown",
    }
}

fn gateway_live_exit_for_result(status: i32, why: String) -> Option<GatewayLiveExit> {
    if status == D7OK {
        None
    } else {
        Some(GatewayLiveExit {
            status: gateway_exit_code(status, &why),
            why,
        })
    }
}

fn gateway_live_graceful_shutdown(
    runtime: &RuntimeHandle,
    transport: &mut TungsteniteGatewayTransport,
) -> GatewayLiveExit {
    match runtime.lock() {
        Ok(mut runtime) => runtime.shutdown_gateway(),
        Err(_) => {
            return GatewayLiveExit {
                status: EXIT_INTERNAL,
                why: "RUNTIME_LOCK_POISONED".to_string(),
            };
        }
    }
    let _ = transport.close_gracefully();
    GatewayLiveExit::ok()
}

fn gateway_exit_code(status: i32, why: &str) -> i32 {
    if status == D7FOR && why == WHY_AUTH_INVALID {
        EXIT_REST_AUTH_FATAL
    } else {
        EXIT_GATEWAY_FATAL
    }
}

fn stop_gateway_live(gateway_live: Option<GatewayLive>) -> Result<(), AppError> {
    let Some(gateway_live) = gateway_live else {
        return Ok(());
    };
    gateway_live.stop.store(true, Ordering::Relaxed);
    let mut gateway_live = gateway_live;
    let handle = gateway_live
        .handle
        .take()
        .ok_or_else(|| AppError::message(EXIT_INTERNAL, "gateway live thread missing"))?;
    let result = handle
        .join()
        .map_err(|_| AppError::message(EXIT_INTERNAL, "gateway live thread panicked"))?;
    if result.status == D7OK {
        Ok(())
    } else {
        Err(AppError::message(
            result.status,
            format!("gateway live failed: {}", result.why),
        ))
    }
}

fn check_gateway_live_finished(gateway_live: &mut Option<GatewayLive>) -> Result<(), AppError> {
    let finished = gateway_live
        .as_ref()
        .and_then(|gateway_live| gateway_live.handle.as_ref())
        .is_some_and(JoinHandle::is_finished);
    if !finished {
        return Ok(());
    }
    let mut gateway_live = gateway_live
        .take()
        .ok_or_else(|| AppError::message(EXIT_INTERNAL, "gateway live missing"))?;
    let handle = gateway_live
        .handle
        .take()
        .ok_or_else(|| AppError::message(EXIT_INTERNAL, "gateway live thread missing"))?;
    let exit = handle
        .join()
        .map_err(|_| AppError::message(EXIT_INTERNAL, "gateway live thread panicked"))?;
    Err(gateway_live_exit_error(exit))
}

fn gateway_live_exit_error(exit: GatewayLiveExit) -> AppError {
    if exit.status == D7OK {
        AppError::message(EXIT_GATEWAY_FATAL, "gateway live stopped unexpectedly")
    } else {
        AppError::message(exit.status, format!("gateway live failed: {}", exit.why))
    }
}

fn stop_child_for_hot_restart(child: &mut Child) -> Result<(), AppError> {
    let deadline = Instant::now() + Duration::from_millis(2_000);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => {
                return Err(AppError::message(
                    EXIT_INTERNAL,
                    format!("hot restart child wait failed: {error}"),
                ));
            }
        }
        if Instant::now() >= deadline {
            child.kill().map_err(|error| {
                AppError::message(
                    EXIT_INTERNAL,
                    format!("hot restart child kill failed: {error}"),
                )
            })?;
            child.wait().map_err(|error| {
                AppError::message(
                    EXIT_INTERNAL,
                    format!("hot restart child wait failed: {error}"),
                )
            })?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn runtime_secret_key(name: &std::ffi::OsStr) -> bool {
    let name = name.as_encoded_bytes();
    #[cfg(windows)]
    let name = name.to_ascii_uppercase();
    name.starts_with(b"DFOR77_") && name.ends_with(b"_KEY")
}

fn strip_bot_secrets(command: &mut Command, options: &Options) {
    for (name, _) in env::vars_os() {
        if runtime_secret_key(&name) {
            command.env_remove(name);
        }
    }
    for name in [
        "DISCORD_BOT_TOKEN",
        "DFOR77_AUDIT_HASH_KEY",
        "CLOUDFLARE_API_TOKEN",
        "CLOUDFLARE_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ]
    .into_iter()
    .chain(options.token_env.as_deref())
    .chain(options.audit_hash_key_env.as_deref())
    {
        command.env_remove(name);
    }
}

fn start_bot_request_reader(stdout: impl Read + Send + 'static) -> BotRequestReader {
    let (tx, rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let result = read_request(&mut reader).map_err(bot_ipc_error);
            let done = matches!(result, Ok(None) | Err(_));
            if tx.send(result).is_err() || done {
                break;
            }
        }
    });
    BotRequestReader {
        rx: Some(rx),
        handle: Some(handle),
        interrupt: None,
    }
}

fn bot_ipc_error(error: dfor77::ipc::IpcError) -> AppError {
    let code = if error.io_error_kind().is_some() {
        EXIT_BOT_CRASHED
    } else {
        EXIT_PROTOCOL
    };
    AppError::message(code, error.to_string())
}

struct BotRequestReader {
    rx: Option<mpsc::Receiver<Result<Option<Request>, AppError>>>,
    handle: Option<JoinHandle<()>>,
    interrupt: Option<BotReadInterrupt>,
}

impl BotRequestReader {
    fn recv_timeout(&self, timeout: Duration) -> Result<BotRequestRead, AppError> {
        let Some(rx) = &self.rx else {
            return Ok(BotRequestRead::Eof);
        };
        match rx.recv_timeout(timeout) {
            Ok(Ok(Some(request))) => Ok(BotRequestRead::Request(request)),
            Ok(Ok(None)) => Ok(BotRequestRead::Eof),
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(BotRequestRead::Pending),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(BotRequestRead::Eof),
        }
    }

    fn join(&mut self) -> Result<(), AppError> {
        self.cancel();
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| AppError::message(EXIT_INTERNAL, "bot request reader thread panicked"))
    }

    fn detach(&mut self) {
        self.cancel();
        let _ = self.handle.take();
    }

    fn cancel(&mut self) {
        if let Some(interrupt) = &self.interrupt {
            interrupt.stop();
        }
        self.rx.take();
    }
}

impl Drop for BotRequestReader {
    fn drop(&mut self) {
        self.cancel();
        if self.interrupt.is_some() {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

enum BotRequestRead {
    Pending,
    Eof,
    Request(Request),
}

fn lock_runtime(
    runtime: &RuntimeHandle,
) -> Result<std::sync::MutexGuard<'_, DforRuntime>, AppError> {
    runtime
        .lock()
        .map_err(|_| AppError::message(EXIT_INTERNAL, "runtime lock poisoned"))
}

fn gateway_live_timeout_ms(settings: &RuntimeSettings) -> u64 {
    match settings.gateway_next_event_timeout_ms {
        0 => 100,
        value => value.clamp(1, 1000),
    }
}

fn gateway_live_wait_for_backoff(stop: &AtomicBool, delay_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(delay_ms);
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
    false
}

fn gateway_live_backoff_delay_ms(delay_ms: u64) -> u64 {
    delay_ms
}

fn default_cli_user_agent() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let repository = option_env!("CARGO_PKG_REPOSITORY").unwrap_or("");
    if repository.is_empty() {
        format!("dfor77/{version}")
    } else {
        format!("dfor77/{version} ({repository})")
    }
}

fn make_runtime(options: Options) -> Result<DforRuntime, AppError> {
    let settings = options.runtime_settings.clone().unwrap_or_default();
    let mut runtime = if options.demo_events {
        DforRuntime::with_demo_events_and_settings(options.dry_run, options.log_only, settings)
    } else {
        DforRuntime::with_settings(options.dry_run, options.log_only, settings)
    };
    if let Some(base_url) = &options.rest_base_url {
        let token = discord_token(&options)?;
        let settings = options.runtime_settings.clone().unwrap_or_default();
        let backend = HttpRestBackend::new(
            base_url,
            token,
            default_cli_user_agent(),
            settings.rest_http_timeout_ms,
        )
        .map_err(|error| AppError::message(EXIT_CONFIG, error))?;
        runtime.use_http_rest_backend(backend);
    }
    if let Some(path) = &options.mock_gateway_path {
        load_mock_gateway(&mut runtime, path)?;
    }
    if let Some(path) = &options.audit_path {
        runtime
            .enable_audit_with_policy(path, options.audit_content_policy.clone())
            .map_err(|error| {
                AppError::message(EXIT_CONFIG, format!("audit open failed: {}", error))
            })?;
    }
    if let Some(path) = &options.runtime_log_path {
        runtime.enable_runtime_log(path).map_err(|error| {
            AppError::message(EXIT_CONFIG, format!("runtime log open failed: {}", error))
        })?;
    }
    Ok(runtime)
}

fn audit_content_policy_from_config(config: &Config) -> Result<AuditContentPolicy, AppError> {
    match config.audit_content_mode.as_str() {
        "none" => Ok(AuditContentPolicy::none()),
        "hash" => AuditContentPolicy::hash_from_env(&config.audit_hash_key_env)
            .map_err(|error| AppError::message(EXIT_CONFIG, error.to_string())),
        "truncated" => Ok(AuditContentPolicy::Truncated {
            preview_bytes: config.audit_content_preview_bytes,
        }),
        _ => Err(AppError::message(
            EXIT_CONFIG,
            "audit.content_mode is invalid",
        )),
    }
}

fn emit_config_warnings(config: &Config) {
    for warning in &config.warnings {
        write_diagnostic(format_args!("dfor77 warn config: {warning}"));
    }
}

fn discord_token(options: &Options) -> Result<String, AppError> {
    let token_env = options.token_env.as_deref().unwrap_or("DISCORD_BOT_TOKEN");
    let token = env::var(token_env).map_err(|_| {
        AppError::message(
            EXIT_CONFIG,
            format!("environment variable {} is not set", token_env),
        )
    })?;
    validate_cli_bot_token_value(&token).map_err(|error| {
        AppError::message(
            EXIT_CONFIG,
            format!("environment variable {} is invalid: {}", token_env, error),
        )
    })?;
    Ok(token)
}

fn validate_cli_bot_token_value(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("bot token must not be empty");
    }
    if value.bytes().any(|byte| byte <= b' ' || byte == 0x7F) {
        return Err("bot token must not contain whitespace or control characters");
    }
    Ok(())
}

fn load_mock_gateway(runtime: &mut DforRuntime, path: &str) -> Result<(), AppError> {
    let contents = fs::read_to_string(path).map_err(|error| {
        AppError::message(EXIT_CONFIG, format!("mock gateway read failed: {}", error))
    })?;
    for (index, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        runtime
            .ingest_gateway_json(trimmed, now_ms_u64())
            .map_err(|error| {
                AppError::message(
                    EXIT_CONFIG,
                    format!("mock gateway frame {} failed: {}", index + 1, error),
                )
            })?;
    }
    Ok(())
}

fn now_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| {
            let millis = duration.as_millis();
            if millis > u128::from(u64::MAX) {
                u64::MAX
            } else {
                millis as u64
            }
        })
        .unwrap_or(0)
}

fn handle_mock_rest_stream(stream: &mut TcpStream, gateway_url: &str) -> Result<(), AppError> {
    let request = read_mock_http_request(stream)?;
    let (method, path) = parse_mock_http_request_line(&request)?;
    let (status, body) = mock_rest_response(method, path, gateway_url);
    let response = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).map_err(|error| {
        AppError::message(EXIT_INTERNAL, format!("mock REST write failed: {error}"))
    })
}

fn read_mock_http_request(stream: &mut TcpStream) -> Result<String, AppError> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buffer).map_err(|error| {
            AppError::message(EXIT_INTERNAL, format!("mock REST read failed: {error}"))
        })?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some((head, body_start)) = split_http_head_body(&bytes)? {
            let content_length = mock_content_length(head)?;
            if bytes.len().saturating_sub(body_start) >= content_length {
                break;
            }
        }
        if bytes.len() > 1024 * 1024 {
            return Err(AppError::message(
                EXIT_PROTOCOL,
                "mock REST request is too large",
            ));
        }
    }
    mock_request_text(bytes)
}

fn mock_request_text(bytes: Vec<u8>) -> Result<String, AppError> {
    if !bytes.is_empty() && split_http_head_body(&bytes)?.is_none() {
        return Err(AppError::message(
            EXIT_PROTOCOL,
            "mock REST request is incomplete",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| AppError::message(EXIT_PROTOCOL, "mock REST request contains invalid UTF-8"))
}

fn parse_mock_http_request_line(request: &str) -> Result<(&str, &str), AppError> {
    let line = request
        .lines()
        .next()
        .ok_or_else(|| AppError::message(EXIT_PROTOCOL, "mock REST request line missing"))?;
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| AppError::message(EXIT_PROTOCOL, "mock REST method missing"))?;
    let path = parts
        .next()
        .ok_or_else(|| AppError::message(EXIT_PROTOCOL, "mock REST path missing"))?;
    let version = parts
        .next()
        .ok_or_else(|| AppError::message(EXIT_PROTOCOL, "mock REST HTTP version missing"))?;
    if parts.next().is_some() {
        return Err(AppError::message(
            EXIT_PROTOCOL,
            "mock REST request line has extra fields",
        ));
    }
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(AppError::message(
            EXIT_PROTOCOL,
            "mock REST HTTP version is unsupported",
        ));
    }
    if !path.starts_with('/') {
        return Err(AppError::message(
            EXIT_PROTOCOL,
            "mock REST request target must be origin-form",
        ));
    }
    Ok((method, path))
}

fn mock_rest_response(method: &str, path: &str, gateway_url: &str) -> (u16, String) {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    if method == "GET" && path == "/api/v10/gateway/bot" {
        return (
            200,
            format!(
                "{{\"url\":\"{}\",\"session_start_limit\":{{\"total\":1000,\"remaining\":999,\"reset_after\":60000,\"max_concurrency\":1}}}}",
                json_escape(gateway_url)
            ),
        );
    }
    if method == "GET" && path == "/api/v10/users/@me" {
        return (200, "{\"id\":\"100000000000000009\"}".to_string());
    }
    if let Some(route) = path.strip_prefix("/api/v10/channels/") {
        let parts: Vec<&str> = route.split('/').collect();
        if method == "GET" && parts.len() == 3 && parts[1] == "messages" {
            return (
                200,
                format!(
                    "{{\"id\":\"{}\",\"channel_id\":\"{}\",\"guild_id\":\"100000000000000001\",\"type\":0,\"content\":\"mock message\",\"author\":{{\"id\":\"100000000000000009\",\"bot\":false}}}}",
                    json_escape(parts[2]),
                    json_escape(parts[0])
                ),
            );
        }
        if method == "GET" && parts.len() == 3 && parts[1] == "thread-members" {
            return (
                200,
                format!(
                    "{{\"id\":\"{}\",\"user_id\":\"{}\",\"join_timestamp\":\"2026-06-03T00:00:00.000Z\",\"flags\":0}}",
                    json_escape(parts[0]),
                    json_escape(parts[2])
                ),
            );
        }
        if method == "GET"
            && ((parts.len() == 6
                && parts[1] == "users"
                && parts[2] == "@me"
                && parts[3] == "threads"
                && parts[4] == "archived"
                && parts[5] == "private")
                || (parts.len() == 4
                    && parts[1] == "threads"
                    && parts[2] == "archived"
                    && matches!(parts[3], "public" | "private")))
        {
            return (200, mock_archived_threads_body(parts[0]));
        }
        if method == "GET" && parts.len() == 1 {
            return (
                200,
                format!(
                    "{{\"id\":\"{}\",\"guild_id\":\"100000000000000001\",\"type\":0}}",
                    json_escape(parts[0])
                ),
            );
        }
        if method == "POST" && parts.len() == 2 && parts[1] == "messages" {
            return (200, "{\"id\":\"100000000000000099\"}".to_string());
        }
        if method == "DELETE" && parts.len() == 3 && parts[1] == "messages" {
            return (204, String::new());
        }
    }
    if let Some(route) = path.strip_prefix("/api/v10/guilds/") {
        let parts: Vec<&str> = route.split('/').collect();
        if method == "GET" && parts.len() == 2 && parts[1] == "roles" {
            return (
                200,
                format!(
                    "[{{\"id\":\"{}\",\"permissions\":\"1099511702528\",\"position\":0}}]",
                    json_escape(parts[0])
                ),
            );
        }
        if method == "GET" && parts.len() == 3 && parts[1] == "members" {
            return (
                200,
                format!(
                    "{{\"user\":{{\"id\":\"{}\"}},\"roles\":[],\"communication_disabled_until\":null}}",
                    json_escape(parts[2])
                ),
            );
        }
        if method == "PATCH" && parts.len() == 3 && parts[1] == "members" {
            return (200, "{\"id\":\"100000000000000077\"}".to_string());
        }
    }
    (
        404,
        "{\"code\":10003,\"message\":\"mock route not found\"}".to_string(),
    )
}

fn mock_archived_threads_body(parent_channel_id: &str) -> String {
    format!(
        "{{\"threads\":[{{\"id\":\"100000000000000002\",\"guild_id\":\"100000000000000001\",\"parent_id\":\"{}\",\"type\":12,\"thread_metadata\":{{\"archived\":true,\"locked\":false}}}}],\"members\":[{{\"id\":\"100000000000000002\",\"user_id\":\"100000000000000009\"}}],\"has_more\":false}}",
        json_escape(parent_channel_id)
    )
}

fn split_http_head_body(bytes: &[u8]) -> Result<Option<(&str, usize)>, AppError> {
    let marker = b"\r\n\r\n";
    let Some(index) = bytes
        .windows(marker.len())
        .position(|window| window == marker)
    else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&bytes[..index]).map_err(|_| {
        AppError::message(
            EXIT_PROTOCOL,
            "mock REST request header contains invalid UTF-8",
        )
    })?;
    Ok(Some((head, index + marker.len())))
}

fn mock_content_length(head: &str) -> Result<usize, AppError> {
    let mut length = None;
    for (key, value) in head.lines().filter_map(|line| line.split_once(':')) {
        if !key.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        if length.is_some() {
            return Err(AppError::message(
                EXIT_PROTOCOL,
                "mock REST duplicate content-length",
            ));
        }
        let parsed = value
            .trim()
            .parse::<usize>()
            .map_err(|_| AppError::message(EXIT_PROTOCOL, "mock REST content-length is invalid"))?;
        length = Some(parsed);
    }
    Ok(length.unwrap_or(0))
}

fn gateway_connect_json(result: &GatewayConnectResult) -> String {
    format!(
        "{{\"status\":{},\"why\":{},\"startup_action\":{},\"gateway_url\":{},\"connected\":{},\"identify_sent\":{},\"sent_payload_bytes\":{},\"received_frames\":{},\"last_action\":{},\"identify_bucket\":{},\"identify_remaining\":{},\"daily_identify_count\":{},\"daily_identify_remaining\":{},\"daily_identify_warning\":{},\"gateway_outbound_remaining\":{}}}",
        result.status,
        json_string(&result.why),
        json_string(gateway_action_text(&result.startup.action)),
        json_string_option(result.startup.gateway_url.as_deref()),
        json_bool(result.connected),
        json_bool(result.identify_sent),
        result.sent_payload_bytes,
        result.received_frames,
        json_string(gateway_action_text(&result.last_action)),
        json_u32_option(result.startup.identify_bucket),
        json_u32_option(result.startup.identify_remaining),
        result.startup.daily_identify_count,
        result.startup.daily_identify_remaining,
        json_bool(result.startup.daily_identify_warning),
        json_usize_option(result.gateway_outbound_remaining)
    )
}

fn gateway_action_text(action: &GatewayAction) -> &'static str {
    match action {
        GatewayAction::None => "None",
        GatewayAction::FetchGatewayBot => "FetchGatewayBot",
        GatewayAction::SendIdentify => "SendIdentify",
        GatewayAction::SendResume { .. } => "SendResume",
        GatewayAction::SendHeartbeat => "SendHeartbeat",
        GatewayAction::CloseAndResume => "CloseAndResume",
        GatewayAction::Backoff { .. } => "Backoff",
        GatewayAction::Fatal(_) => "Fatal",
    }
}

fn gateway_action_detail(action: &GatewayAction) -> String {
    match action {
        GatewayAction::SendResume { seq } => format!("SendResume(seq={seq})"),
        GatewayAction::Backoff { delay_ms } => format!("Backoff(delay_ms={delay_ms})"),
        GatewayAction::Fatal(reason) => format!("Fatal(reason={reason})"),
        _ => gateway_action_text(action).to_string(),
    }
}

fn log_option(value: Option<&str>) -> String {
    diagnostic_field(value.unwrap_or("-"))
}

fn json_string(value: &str) -> String {
    format!("\"{}\"", json_log_escape(value))
}

fn json_string_option(value: Option<&str>) -> String {
    value.map(json_string).unwrap_or_else(|| "null".to_string())
}

fn json_u32_option(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn json_usize_option(value: Option<usize>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn json_bool(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

struct GatewayLive {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<GatewayLiveExit>>,
}

impl Drop for GatewayLive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct GatewayLiveExit {
    status: i32,
    why: String,
}

impl GatewayLiveExit {
    fn ok() -> Self {
        Self {
            status: D7OK,
            why: "OK".to_string(),
        }
    }

    fn from_app_error(error: AppError) -> Self {
        match error {
            AppError::Exit(code) => Self {
                status: code,
                why: "EXIT".to_string(),
            },
            AppError::Message(code, message) => Self {
                status: code,
                why: message,
            },
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum HotRestartFileState {
    Missing,
    Present { modified_ns: u128, len: u64 },
}

#[derive(Debug)]
struct HotRestartPathState {
    path: String,
    state: HotRestartFileState,
}

#[derive(Debug)]
struct HotRestartWatcher {
    paths: Vec<HotRestartPathState>,
}

impl HotRestartWatcher {
    fn from_options(options: &Options) -> Result<Self, AppError> {
        Self::new(hot_restart_paths(options))
    }

    fn new(paths: Vec<String>) -> Result<Self, AppError> {
        let mut watched = Vec::new();
        for path in paths {
            if watched
                .iter()
                .any(|existing: &HotRestartPathState| existing.path == path)
            {
                continue;
            }
            watched.push(HotRestartPathState {
                state: read_hot_restart_file_state(&path)?,
                path,
            });
        }
        Ok(Self { paths: watched })
    }

    fn changed(&mut self) -> Result<bool, AppError> {
        let mut changed = false;
        for watched in &mut self.paths {
            let current = read_hot_restart_file_state(&watched.path)?;
            if current != watched.state {
                watched.state = current;
                changed = true;
            }
        }
        Ok(changed)
    }
}

fn read_hot_restart_file_state(path: &str) -> Result<HotRestartFileState, AppError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HotRestartFileState::Missing);
        }
        Err(error) => {
            return Err(AppError::message(
                EXIT_CONFIG,
                format!("hot restart file metadata failed: {error}"),
            ));
        }
    };
    let modified_ns = metadata
        .modified()
        .map_err(|error| {
            AppError::message(
                EXIT_CONFIG,
                format!("hot restart file modified time failed: {error}"),
            )
        })?
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Ok(HotRestartFileState::Present {
        modified_ns,
        len: metadata.len(),
    })
}

enum RuntimeRequestResult {
    Response(Response, bool),
    HotRestart,
}

enum BotSupervisorExit {
    Finished,
    HotRestart,
}

fn print_help() {
    println!(
        "dfor77\n\n\
         Usage:\n\
           dfor77 --help\n\
           dfor77 --version\n\
           dfor77 --validate-config --config PATH\n\
           dfor77 --validate-audit-log PATH [--audit-require-action ACTION]... [--audit-forbid-action ACTION]... [--audit-require-live-actions]\n\
           dfor77 --validate-runtime-log PATH\n\
           dfor77 --stdio-server [--demo-events] [--mock-gateway PATH] [--gateway-live] [--rest-url URL] [--mock-rest URL] [--audit-log PATH] [--runtime-log PATH] [--dry-run] [--log-only] [--config PATH]\n\
           dfor77 --gateway-connect-once [--rest-url URL] [--mock-rest URL] [--audit-log PATH] [--runtime-log PATH] [--config PATH]\n\
           dfor77 [--demo-events] [--mock-gateway PATH] [--gateway-live] [--gateway-session-dir PATH] [--hot-restart-file PATH] [--bot-io-timeout-ms N] [--shards N] [--shard-restart-limit N] [--rest-url URL] [--mock-rest URL] [--audit-log PATH] [--runtime-log PATH] [--dry-run] [--log-only] [--config PATH] --bot COMMAND [ARGS...]\n\n\
           dfor77 --mock-rest-server HOST:PORT [--mock-gateway-url URL] [--mock-rest-max-requests N]\n\n\
         The stdio server reads @D7/1 request frames from stdin and writes responses to stdout.\n\
         The bot supervisor launches a FORTRAN77 bot and connects its stdout/stdin to the IPC runtime."
    );
}

fn print_version() {
    println!("dfor77 {}", env!("CARGO_PKG_VERSION"));
}

#[derive(Clone, Debug)]
struct Options {
    bot_io_timeout_ms: u64,
    supervisor_stop: Option<Arc<AtomicBool>>,
    mode: Mode,
    demo_events: bool,
    dry_run: bool,
    log_only: bool,
    gateway_live: bool,
    mock_gateway_path: Option<String>,
    rest_base_url: Option<String>,
    mock_rest_server_addr: Option<String>,
    mock_rest_max_requests: Option<usize>,
    mock_gateway_url: Option<String>,
    audit_path: Option<String>,
    runtime_log_path: Option<String>,
    audit_content_policy: AuditContentPolicy,
    token_env: Option<String>,
    audit_hash_key_env: Option<String>,
    runtime_settings: Option<RuntimeSettings>,
    supervisor_shards: Option<u32>,
    shard_restart_limit: Option<u32>,
    gateway_session_dir: Option<String>,
    gateway_session_path: Option<String>,
    hot_restart_file: Option<String>,
    hot_restart_shard_file: Option<String>,
    validate_audit_log_path: Option<String>,
    validate_runtime_log_path: Option<String>,
    audit_required_actions: Vec<String>,
    audit_forbidden_actions: Vec<String>,
    audit_require_live_actions: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bot_io_timeout_ms: 10_000,
            supervisor_stop: None,
            mode: Mode::StdioServer,
            demo_events: false,
            dry_run: false,
            log_only: false,
            gateway_live: false,
            mock_gateway_path: None,
            rest_base_url: None,
            mock_rest_server_addr: None,
            mock_rest_max_requests: None,
            mock_gateway_url: None,
            audit_path: None,
            runtime_log_path: None,
            audit_content_policy: AuditContentPolicy::none(),
            token_env: None,
            audit_hash_key_env: None,
            runtime_settings: None,
            supervisor_shards: None,
            shard_restart_limit: None,
            gateway_session_dir: None,
            gateway_session_path: None,
            hot_restart_file: None,
            hot_restart_shard_file: None,
            validate_audit_log_path: None,
            validate_runtime_log_path: None,
            audit_required_actions: Vec::new(),
            audit_forbidden_actions: Vec::new(),
            audit_require_live_actions: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    ValidateConfig,
    ValidateAuditLog,
    ValidateRuntimeLog,
    StdioServer,
    BotSupervisor,
    MockRestServer,
    GatewayConnectOnce,
}

#[derive(Debug)]
enum AppError {
    Exit(i32),
    Message(i32, String),
}

impl AppError {
    fn message(code: i32, message: impl Into<String>) -> Self {
        Self::Message(code, message.into())
    }
}

fn app_error_log_text(error: &AppError) -> String {
    match error {
        AppError::Exit(code) => format!("exit_code={code}"),
        AppError::Message(code, message) => format!("code={code} message={message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_temp_creation_failure_does_not_remove_an_existing_file() {
        let root =
            std::env::temp_dir().join(format!("dfor77-session-owned-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let path = root.join("session.json");
        let temporary = root.join("already-exists");
        fs::write(&temporary, "other file").unwrap();
        let error = write_gateway_session_tmp(&path, &temporary, "session").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&temporary).unwrap(), "other file");
        assert!(!path.exists());
        fs::remove_file(&temporary).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(write_gateway_session_tmp(&path, &temporary, "session").is_err());
        assert!(!temporary.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_temp_names_remain_unique_within_one_clock_tick() {
        let mut names = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(names.insert(gateway_session_tmp_path(Path::new("session.json"))));
        }
    }

    #[test]
    fn runtime_secret_key_names_are_reserved() {
        use std::ffi::OsStr;

        assert!(runtime_secret_key(OsStr::new("DFOR77_EXAMPLE_KEY")));
        assert!(!runtime_secret_key(OsStr::new("DFOR77_SHARD_ID")));
        assert!(!runtime_secret_key(OsStr::new("APP_EXAMPLE_KEY")));
        assert_eq!(
            runtime_secret_key(OsStr::new("dfor77_example_key")),
            cfg!(windows)
        );
    }

    #[test]
    fn bot_child_does_not_inherit_runtime_credentials() {
        let options = Options {
            token_env: Some("CUSTOM_BOT_TOKEN".into()),
            audit_hash_key_env: Some("CUSTOM_AUDIT_KEY".into()),
            ..Options::default()
        };
        let mut command = Command::new("unused-test-command");
        strip_bot_secrets(&mut command, &options);
        let removed = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        for key in [
            "DISCORD_BOT_TOKEN",
            "DFOR77_AUDIT_HASH_KEY",
            "CUSTOM_BOT_TOKEN",
            "CUSTOM_AUDIT_KEY",
        ] {
            assert!(removed.iter().any(|value| value == key));
        }
    }

    #[test]
    fn bounded_ipc_reader_can_be_joined_after_a_request_flood() {
        let frame = "@D7/1 REQ 1 INI 0\n@D7/1 END 1\n";
        let mut reader = start_bot_request_reader(std::io::Cursor::new(frame.repeat(100_000)));
        thread::sleep(Duration::from_millis(20));
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ipc_reader_can_be_joined_while_another_process_holds_stdout_open() {
        let mut command = Command::new("unused");
        let stdout = prepare_output(&mut command).unwrap().unwrap();
        let interrupt = stdout.interrupt_handle().unwrap();
        let mut reader = start_bot_request_reader(stdout);
        reader.interrupt = interrupt;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(reader.join());
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        drop(command);
    }

    #[test]
    fn gateway_live_backoff_wait_stops_without_full_delay() {
        let stop = AtomicBool::new(true);
        let start = Instant::now();
        assert!(!gateway_live_wait_for_backoff(&stop, 10_000));
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn gateway_live_backoff_delay_is_not_capped() {
        assert_eq!(gateway_live_backoff_delay_ms(60_000), 60_000);
    }

    #[test]
    fn gateway_connect_json_includes_daily_identify_warning() {
        let startup = GatewayStartup {
            status: D7OK,
            why: "OK".to_string(),
            action: GatewayAction::SendIdentify,
            gateway_url: Some("wss://gateway.discord.gg".to_string()),
            identify_payload: None,
            identify_bucket: Some(0),
            identify_remaining: Some(2),
            identify_reset_after_ms: Some(60_000),
            daily_identify_count: 900,
            daily_identify_remaining: 99,
            daily_identify_warning: true,
        };
        let result = GatewayConnectResult {
            status: D7OK,
            why: "OK".to_string(),
            startup,
            connected: true,
            identify_sent: true,
            sent_payload_bytes: 10,
            gateway_outbound_remaining: Some(119),
            received_frames: 2,
            last_action: GatewayAction::None,
        };
        let json = gateway_connect_json(&result);
        assert!(json.contains("\"daily_identify_count\":900"));
        assert!(json.contains("\"daily_identify_remaining\":99"));
        assert!(json.contains("\"daily_identify_warning\":true"));
    }

    #[test]
    fn options_for_shard_overrides_gateway_shard_tuple() {
        let options = Options {
            runtime_settings: Some(RuntimeSettings {
                gateway_shard_id: 7,
                gateway_shard_count: 9,
                ..RuntimeSettings::default()
            }),
            supervisor_shards: Some(4),
            ..Options::default()
        };
        let shard = options_for_shard(&options, 2, 4);
        let settings = shard.runtime_settings.unwrap();
        assert_eq!(settings.gateway_shard_id, 2);
        assert_eq!(settings.gateway_shard_count, 4);
        assert_eq!(shard.supervisor_shards, Some(4));
    }

    #[test]
    fn supervisor_stop_interrupts_startup_and_restart_delays() {
        let options = Options {
            supervisor_stop: Some(Arc::new(AtomicBool::new(true))),
            ..Options::default()
        };
        let started = Instant::now();
        assert!(supervisor_delay(&options, Duration::from_secs(30)));
        assert!(run_shard_supervisor_with_restarts(options, "must-not-run", &[]).is_ok());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn supervisor_stop_interrupts_an_unbounded_next_event_wait() {
        let runtime = SharedRuntime::new(DforRuntime::new(true, false));
        assert_eq!(
            lock_runtime(&runtime)
                .unwrap()
                .handle(Request {
                    id: 1,
                    verb: "INI".into(),
                    fields: vec![],
                })
                .status,
            D7OK
        );
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut watcher = HotRestartWatcher::new(Vec::new()).unwrap();
            let result = handle_runtime_request_with_hot_restart(
                &runtime,
                Request {
                    id: 2,
                    verb: "NXT".into(),
                    fields: vec![],
                },
                0,
                &mut watcher,
                Some(&thread_stop),
            );
            let _ = tx.send(result);
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        stop.store(true, Ordering::Relaxed);
        let result = rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        handle.join().unwrap();
        assert!(matches!(result, RuntimeRequestResult::HotRestart));
    }

    #[test]
    fn options_for_shard_assigns_session_path() {
        let options = Options {
            gateway_session_dir: Some("/tmp/dfor77-sessions".to_string()),
            ..Options::default()
        };
        let shard = options_for_shard(&options, 3, 8);
        assert_eq!(
            shard.gateway_session_path.as_deref(),
            Some("/tmp/dfor77-sessions/shard-3.json")
        );
        assert_eq!(
            gateway_session_path(&shard).as_deref(),
            Some("/tmp/dfor77-sessions/shard-3.json")
        );
    }

    #[test]
    fn session_loading_rejects_oversized_valid_json_before_parsing() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-session-load-limit-{}.json",
            std::process::id()
        ));
        let session = GatewayResumeSession::new("saved", "wss://resume", 1, None).unwrap();
        let text = session.to_json() + &" ".repeat(64 * 1024);
        fs::write(&path, text).unwrap();
        let result = load_gateway_resume_session(path.to_str().unwrap());
        fs::remove_file(path).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn saving_a_server_invalidated_session_removes_the_old_checkpoint() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-session-invalidated-{}.json",
            std::process::id()
        ));
        let runtime = SharedRuntime::new(DforRuntime::new(false, false));
        lock_runtime(&runtime).unwrap().ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"saved","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
        assert!(load_gateway_resume_session(path.to_str().unwrap())
            .unwrap()
            .is_some());
        lock_runtime(&runtime)
            .unwrap()
            .ingest_gateway_json(r#"{"op":9,"d":false}"#, 1)
            .unwrap();
        save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
        let exists = path.exists();
        if exists {
            fs::remove_file(&path).unwrap();
        }
        assert!(!exists);
    }

    #[test]
    fn live_gateway_actions_persist_invalidation_before_idle_or_fatal_return() {
        for action in [GatewayAction::None, GatewayAction::Fatal("TEST_FATAL")] {
            let path = gateway_session_tmp_path(&std::env::temp_dir().join("dfor77-live-session"));
            let runtime = SharedRuntime::new(DforRuntime::new(false, false));
            lock_runtime(&runtime).unwrap().ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"saved","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
            save_gateway_resume_session(&runtime, path.to_str().unwrap()).unwrap();
            lock_runtime(&runtime)
                .unwrap()
                .ingest_gateway_json(r#"{"op":9,"d":false}"#, 1)
                .unwrap();
            let fatal = matches!(action, GatewayAction::Fatal(_));
            let mut transport = TungsteniteGatewayTransport::new(100);
            let exit = handle_gateway_live_action(
                &runtime,
                &mut transport,
                "unused-test-token",
                action,
                &AtomicBool::new(false),
                path.to_str(),
            );
            assert_eq!(exit.is_some(), fatal);
            assert!(!path.exists());
        }
    }

    #[test]
    fn gateway_session_write_uses_owner_only_permissions() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-session-perms-{}-{}.json",
            std::process::id(),
            now_ms_u64()
        ));
        let _ = fs::remove_file(&path);
        fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_gateway_session_file(&path, r#"{"session_id":"abc"}"#).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"session_id":"abc"}"#
        );
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn options_for_shard_assigns_hot_restart_shard_file() {
        let options = Options {
            hot_restart_file: Some("/tmp/dfor77-restart".to_string()),
            ..Options::default()
        };
        let shard = options_for_shard(&options, 2, 4);
        assert_eq!(
            hot_restart_paths(&shard),
            vec![
                "/tmp/dfor77-restart".to_string(),
                "/tmp/dfor77-restart.shard-2".to_string()
            ]
        );
    }

    #[test]
    fn hot_restart_watcher_detects_changes_after_baseline() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-hot-restart-{}-{}",
            std::process::id(),
            now_ms_u64()
        ));
        let path = path.to_string_lossy().into_owned();
        let _ = fs::remove_file(&path);
        let mut watcher = HotRestartWatcher::new(vec![path.clone()]).unwrap();
        assert!(!watcher.changed().unwrap());
        fs::write(&path, "one").unwrap();
        assert!(watcher.changed().unwrap());
        assert!(!watcher.changed().unwrap());
        fs::write(&path, "one plus more").unwrap();
        assert!(watcher.changed().unwrap());
        fs::remove_file(&path).unwrap();
        assert!(watcher.changed().unwrap());
    }

    #[test]
    fn shard_restartable_errors_are_limited_to_transient_failures() {
        assert!(shard_error_restartable(&AppError::Exit(EXIT_BOT_CRASHED)));
        assert!(shard_error_restartable(&AppError::Message(
            EXIT_GATEWAY_FATAL,
            "gateway".to_string()
        )));
        assert!(!shard_error_restartable(&AppError::Message(
            EXIT_REST_AUTH_FATAL,
            "auth".to_string()
        )));
        assert!(!shard_error_restartable(&AppError::Message(
            EXIT_CONFIG,
            "config".to_string()
        )));
    }

    #[test]
    fn gateway_live_unexpected_ok_exit_is_restartable_gateway_failure() {
        match gateway_live_exit_error(GatewayLiveExit::ok()) {
            AppError::Message(code, message) => {
                assert_eq!(code, EXIT_GATEWAY_FATAL);
                assert!(message.contains("gateway live stopped unexpectedly"));
            }
            AppError::Exit(_) => panic!("unexpected exit error"),
        }
    }

    #[test]
    fn gateway_live_failed_exit_preserves_status_and_reason() {
        let exit = GatewayLiveExit {
            status: EXIT_GATEWAY_FATAL,
            why: "GATEWAY_CLOSED".to_string(),
        };
        match gateway_live_exit_error(exit) {
            AppError::Message(code, message) => {
                assert_eq!(code, EXIT_GATEWAY_FATAL);
                assert!(message.contains("GATEWAY_CLOSED"));
            }
            AppError::Exit(_) => panic!("unexpected exit error"),
        }
    }

    #[test]
    fn mock_rest_request_line_accepts_origin_form_http_versions() {
        let (method, path) =
            parse_mock_http_request_line("GET /api/v10/gateway/bot HTTP/1.1\r\n").unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/v10/gateway/bot");
        assert!(
            parse_mock_http_request_line("POST /api/v10/channels/1/messages HTTP/1.0\r\n").is_ok()
        );
    }

    #[test]
    fn mock_rest_request_line_rejects_malformed_targets() {
        for request in [
            "GET /api/v10/gateway/bot\r\n",
            "GET https://discord.com/api/v10/gateway/bot HTTP/1.1\r\n",
            "GET /api/v10/gateway/bot HTTP/2\r\n",
            "GET /api/v10/gateway/bot HTTP/1.1 extra\r\n",
        ] {
            assert!(parse_mock_http_request_line(request).is_err(), "{request}");
        }
    }

    #[test]
    fn mock_rest_content_length_is_strict() {
        assert_eq!(
            mock_content_length("POST / HTTP/1.1\r\nContent-Length: 12\r\n").unwrap(),
            12
        );
        assert_eq!(mock_content_length("GET / HTTP/1.1\r\n").unwrap(), 0);
        assert!(mock_content_length("POST / HTTP/1.1\r\nContent-Length: nope\r\n").is_err());
        assert!(mock_content_length(
            "POST / HTTP/1.1\r\nContent-Length: 1\r\ncontent-length: 1\r\n"
        )
        .is_err());
    }

    #[test]
    fn mock_rest_request_text_rejects_invalid_or_incomplete_bytes() {
        assert_eq!(
            split_http_head_body(b"GET / HTTP/1.1\r\n\r\nbody")
                .unwrap()
                .unwrap()
                .1,
            18
        );
        assert_app_error_contains(
            split_http_head_body(b"GET /\xFF HTTP/1.1\r\n\r\n").unwrap_err(),
            "invalid UTF-8",
        );
        assert_app_error_contains(
            mock_request_text(b"GET / HTTP/1.1\r\nHost: local\r\n".to_vec()).unwrap_err(),
            "incomplete",
        );
        assert_app_error_contains(
            mock_request_text(b"POST / HTTP/1.1\r\n\r\n\xFF".to_vec()).unwrap_err(),
            "invalid UTF-8",
        );
    }

    #[test]
    fn mock_rest_response_serves_read_only_completion_routes() {
        let message = mock_rest_response(
            "GET",
            "/api/v10/channels/100000000000000010/messages/100000000000000011",
            "ws://127.0.0.1/gateway",
        );
        assert_eq!(message.0, 200);
        assert!(message.1.contains("\"id\":\"100000000000000011\""));
        assert!(message.1.contains("\"channel_id\":\"100000000000000010\""));

        let thread_member = mock_rest_response(
            "GET",
            "/api/v10/channels/100000000000000012/thread-members/100000000000000009",
            "ws://127.0.0.1/gateway",
        );
        assert_eq!(thread_member.0, 200);
        assert!(thread_member
            .1
            .contains("\"user_id\":\"100000000000000009\""));

        let archived = mock_rest_response(
            "GET",
            "/api/v10/channels/100000000000000013/users/@me/threads/archived/private?limit=50",
            "ws://127.0.0.1/gateway",
        );
        assert_eq!(archived.0, 200);
        assert!(archived.1.contains("\"threads\""));
        assert!(archived.1.contains("\"parent_id\":\"100000000000000013\""));
    }

    fn assert_app_error_contains(error: AppError, expected: &str) {
        match error {
            AppError::Message(_, message) => assert!(message.contains(expected), "{message}"),
            AppError::Exit(code) => panic!("unexpected exit error: {code}"),
        }
    }
}
