use clap::{
    ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    parser::ValueSource,
};
use clap_complete::Shell;
use clap_complete::generate;
use clap_mangen::Man;
use console::{Emoji, Style};
use crossterm::event::{
    self, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use indicatif::{HumanBytes, HumanDuration, ProgressBar, ProgressStyle};
use is_terminal::IsTerminal;
use regex::Regex;
use serialport::{SerialPortInfo, SerialPortType, available_ports};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use unbrk_core::error::{ConsoleTail, UnbrkError};
use unbrk_core::event::{
    EVENT_SCHEMA_VERSION, Event, EventPayload, FailureClass, ImageKind, RecoveryStage,
    TransferStage,
};
use unbrk_core::flash::{DEFAULT_RESET_TIMEOUT, FlashConfig, flash_from_uboot};
use unbrk_core::prompt::advance_to_prompt_allowing_trailing_space_with_regex;
use unbrk_core::recovery::{
    DEFAULT_PROMPT_TIMEOUT, RecoveryConfig, RecoveryImages, recover_to_uboot,
};
use unbrk_core::target::{
    AN7581, BlockCount, BlockOffset, EraseRange, FlashPlan, PromptPattern, PromptPatterns,
    TargetProfile, WriteStage,
};
use unbrk_core::transport::{SerialTransport, TranscriptTransport, Transport};
use unbrk_core::uboot::DEFAULT_COMMAND_TIMEOUT;
use unbrk_core::xmodem::{
    XMODEM_DEFAULT_BLOCK_RETRY_LIMIT, XMODEM_DEFAULT_EOT_RETRY_LIMIT,
    XMODEM_DEFAULT_PACKET_TIMEOUT, XmodemConfig,
};

const EXIT_CODES_HELP: &str = "\
Exit codes:
  0  success
  1  I/O error
  2  timeout
  3  protocol error
  4  XMODEM failure
  5  U-Boot command failure
  6  verification mismatch
  7  bad input
  8  user abort";
const CONSOLE_HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[must_use]
pub fn run() -> ExitCode {
    let terminal_status = TerminalStatus::detect();
    // Do not hold process-wide stdio locks across the whole command. The fancy
    // progress renderer writes through indicatif's own stdout/stderr handles, so
    // long-lived locks suppress the progress UI entirely.
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    match try_run(
        std::env::args_os(),
        terminal_status,
        &mut stdout,
        &mut stderr,
    ) {
        Ok(()) => CliExitCode::Success.into(),
        Err(error) => {
            let _ignored = writeln!(stderr, "{}", error.styled());
            error.exit_code().into()
        }
    }
}

#[must_use]
pub fn cli_command() -> clap::Command {
    Cli::command()
}

fn try_run<I, T>(
    args: I,
    terminal_status: TerminalStatus,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), RunError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let command = parse_command(args, terminal_status)?;
    dispatch(command, stdout, stderr)
}

fn parse_command<I, T>(args: I, terminal_status: TerminalStatus) -> Result<CommandPlan, RunError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = cli_command()
        .try_get_matches_from(args)
        .map_err(RunError::from)?;
    let cli = Cli::from_arg_matches(&matches)
        .map_err(|error| RunError::Input(InputError::from_clap(&error)))?;

    match cli.command {
        Commands::Recover(args) => {
            let Some(recover_matches) = matches.subcommand_matches("recover") else {
                return Err(RunError::Input(InputError::new(
                    "internal CLI error: recover subcommand matches were not available",
                )));
            };

            validate_recover(*args, recover_matches, terminal_status)
        }
        Commands::Ports => Ok(CommandPlan::Ports),
        Commands::Completions { shell } => Ok(CommandPlan::Completions { shell }),
        Commands::Man => Ok(CommandPlan::Man),
        Commands::Doctor(args) => Ok(CommandPlan::Doctor(args)),
    }
}

fn validate_recover(
    args: RecoverArgs,
    matches: &clap::ArgMatches,
    terminal_status: TerminalStatus,
) -> Result<CommandPlan, RunError> {
    if args.json && matches.value_source("progress") == Some(ValueSource::CommandLine) {
        return Err(RunError::Input(InputError::new(
            "invalid flag combination: --progress cannot be used with --json",
        )));
    }

    if args.non_interactive && args.port.is_none() {
        return Err(RunError::Input(InputError::new(
            "invalid flag combination: --non-interactive requires an explicit --port",
        )));
    }

    if !args.resume_from_uboot && args.preloader.is_none() {
        return Err(RunError::Input(InputError::new(
            "missing required argument: --preloader is required unless --resume-from-uboot is set",
        )));
    }

    if !args.resume_from_uboot && args.fip.is_none() {
        return Err(RunError::Input(InputError::new(
            "missing required argument: --fip is required unless --resume-from-uboot is set",
        )));
    }

    if args.flash_persistent && args.preloader.is_none() {
        return Err(RunError::Input(InputError::new(
            "invalid flag combination: --flash-persistent requires --preloader",
        )));
    }

    if args.flash_persistent && args.fip.is_none() {
        return Err(RunError::Input(InputError::new(
            "invalid flag combination: --flash-persistent requires --fip",
        )));
    }

    let no_console = match args.no_console {
        Some(false) if args.json => {
            return Err(RunError::Input(InputError::new(
                "invalid flag combination: --json implies --no-console, so --no-console=false is not allowed",
            )));
        }
        Some(value) => value,
        None => args.json,
    };

    if let Some(pattern) = &args.uboot_prompt {
        Regex::new(pattern).map_err(|error| {
            RunError::Input(InputError::new(format!(
                "invalid value for --uboot-prompt: {error}"
            )))
        })?;
    }

    if args.has_flash_layout_overrides() && !args.flash_persistent {
        return Err(RunError::Input(InputError::new(
            "invalid flag combination: flash-layout overrides require --flash-persistent",
        )));
    }

    let progress_mode = if args.json {
        ResolvedProgressMode::Off
    } else {
        args.progress
            .unwrap_or(ProgressMode::Auto)
            .resolve(terminal_status.stdout_is_tty)
    };

    let console_handoff_allowed = !args.flash_persistent
        && !args.non_interactive
        && !no_console
        && terminal_status.stdin_is_tty
        && terminal_status.stdout_is_tty;

    Ok(CommandPlan::Recover(Box::new(RecoverPlan {
        args,
        progress_mode,
        console_handoff_allowed,
        terminal_status,
    })))
}

fn dispatch(
    command: CommandPlan,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), RunError> {
    match command {
        CommandPlan::Recover(plan) => run_recover(&plan, stdout, stderr),
        CommandPlan::Ports => run_ports(stdout),
        CommandPlan::Completions { shell } => {
            let mut command = cli_command();
            generate(shell, &mut command, "unbrk", stdout);
            Ok(())
        }
        CommandPlan::Man => {
            Man::new(cli_command())
                .render(stdout)
                .map_err(|source| stdout_io_error("rendering the manual page", &source))?;
            Ok(())
        }
        CommandPlan::Doctor(args) => run_doctor(&args, stdout),
    }
}

fn run_recover(
    plan: &RecoverPlan,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), RunError> {
    if plan.args.resume_from_uboot {
        writeln!(
            stderr,
            "warning: --resume-from-uboot is expert-only and assumes the current U-Boot prompt is safe.",
        )
        .map_err(|source| stderr_io_error("writing the resume-from-uboot warning", &source))?;
    }

    let port = recover_port(plan)?;
    let target = target_profile(&plan.args);
    let mut events = Vec::new();
    let mut reporter = ProgressReporter::new(plan, stdout);
    reporter.write_startup_banner(plan, port.as_str(), &target);
    reporter.output_result()?;
    record_local_event(
        &mut events,
        &mut reporter,
        EventPayload::SessionStarted {
            schema_version: EVENT_SCHEMA_VERSION,
            tool_version: String::from(env!("CARGO_PKG_VERSION")),
            target_profile: String::from(target.name),
            serial_port: Some(port.clone()),
        },
    );
    reporter.output_result()?;

    let mut transport = open_transport(port.as_str(), &plan.args)?;
    record_local_event(
        &mut events,
        &mut reporter,
        EventPayload::PortOpened {
            port: port.clone(),
            baud: plan.args.baud,
        },
    );
    reporter.output_result()?;

    let mut execution = execute_recover(plan, target, &mut transport, &mut events, &mut reporter);
    reporter.output_result()?;
    let mut console_handoff_completed = false;
    if matches!(execution, Ok(RecoverOutcome::Recovered)) && plan.console_handoff_allowed {
        reporter.finish();
        match handoff_console(&mut transport, reporter.writer()) {
            Ok(()) => {
                console_handoff_completed = true;
            }
            Err(error) => execution = Err(error),
        }
    }

    if let Err(error) = &execution {
        reporter.finish();
        record_local_event(
            &mut events,
            &mut reporter,
            EventPayload::Failure {
                class: error.failure_class(),
                message: error.to_string(),
            },
        );
        reporter.output_result()?;
        if !plan.args.json {
            if matches!(error, RunError::Timeout(_))
                && let Some(hint) = timeout_hint(&events)
            {
                writeln!(stderr, "{hint}").map_err(|source| {
                    stderr_io_error("writing the timeout remediation hint", &source)
                })?;
            }
            write_event_trace(stderr, &events[..events.len().saturating_sub(1)])?;
        }
    }

    if let Some(path) = plan.args.log_file.as_deref() {
        write_events_to_path(path, &events)?;
    }

    if plan.args.json {
        write_events_and_flush(reporter.writer(), &events)?;
    } else if let Ok(outcome) = &execution {
        write_recover_summary(
            reporter.writer(),
            plan,
            port.as_str(),
            outcome,
            console_handoff_completed,
        )?;
    }

    execution.map(|_| ())
}

type CliTransport = TranscriptTransport<SerialTransport, Box<dyn Write>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecoverOutcome {
    Recovered,
    FlashedAfterRecovery { reset_evidence: String },
    FlashedFromExistingPrompt { reset_evidence: String },
}

fn stdout_io_error(operation: &str, source: &io::Error) -> RunError {
    local_io_error("stdout", operation, source)
}

fn stderr_io_error(operation: &str, source: &io::Error) -> RunError {
    local_io_error("stderr", operation, source)
}

fn output_io_error(operation: &str, source: &io::Error) -> RunError {
    local_io_error("output", operation, source)
}

fn terminal_io_error(operation: &str, source: &io::Error) -> RunError {
    local_io_error("terminal", operation, source)
}

fn local_io_error(stream: &str, operation: &str, source: &io::Error) -> RunError {
    RunError::Io(io::Error::new(
        source.kind(),
        format!("{stream} I/O failed while {operation}: {source}"),
    ))
}

fn recover_port(plan: &RecoverPlan) -> Result<String, RunError> {
    if let Some(port) = &plan.args.port {
        return Ok(port.clone());
    }

    if !plan.terminal_status.stdin_is_tty || !plan.terminal_status.stdout_is_tty {
        return Err(RunError::Input(InputError::new(
            "automatic port selection is only available in interactive mode; pass --port explicitly",
        )));
    }

    let ports = discover_ports()?;
    select_recover_port(&ports)
}

fn run_ports(stdout: &mut dyn Write) -> Result<(), RunError> {
    let ports = discover_ports()?;

    if ports.is_empty() {
        writeln!(stdout, "No serial ports found.")
            .map_err(|source| stdout_io_error("writing the ports listing", &source))?;
        return Ok(());
    }

    for line in render_ports_listing(&ports) {
        writeln!(stdout, "{line}")
            .map_err(|source| stdout_io_error("writing the ports listing", &source))?;
    }

    Ok(())
}

fn run_doctor(args: &DoctorArgs, stdout: &mut dyn Write) -> Result<(), RunError> {
    let mut failures = 0_u32;

    write_doctor_line(
        stdout,
        "INFO",
        "os",
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    )?;

    match discover_ports() {
        Ok(ports) => write_doctor_line(
            stdout,
            "INFO",
            "ports",
            format!("discovered {} serial port(s)", ports.len()),
        )?,
        Err(error) => write_doctor_line(
            stdout,
            "INFO",
            "ports",
            format!("serial port enumeration unavailable: {error}"),
        )?,
    }

    if let Some(port) = args.port.as_deref() {
        match SerialTransport::open(port.to_owned(), args.baud, Duration::from_millis(250)) {
            Ok(_) => write_doctor_line(
                stdout,
                "PASS",
                "port",
                format!("{port} opened at {} baud", args.baud),
            )?,
            Err(error) => {
                failures += 1;
                write_doctor_line(stdout, "FAIL", "port", format!("{port}: {error}"))?;
            }
        }
    } else {
        write_doctor_line(
            stdout,
            "SKIP",
            "port",
            "not provided; pass --port to probe device access",
        )?;
    }

    failures += check_doctor_image(
        stdout,
        "preloader",
        args.preloader.as_deref(),
        AN7581.flash.preloader.byte_len(AN7581.flash.block_size),
    )?;
    failures += check_doctor_image(
        stdout,
        "fip",
        args.fip.as_deref(),
        AN7581.flash.fip.byte_len(AN7581.flash.block_size),
    )?;

    if failures == 0 {
        write_doctor_line(stdout, "PASS", "summary", "all requested checks passed")?;
        Ok(())
    } else {
        write_doctor_line(
            stdout,
            "FAIL",
            "summary",
            format!("{failures} requested check(s) failed"),
        )?;
        Err(RunError::Input(InputError::new(format!(
            "doctor found {failures} failing check(s)",
        ))))
    }
}

fn check_doctor_image(
    stdout: &mut dyn Write,
    label: &'static str,
    path: Option<&Path>,
    max_bytes: u64,
) -> Result<u32, RunError> {
    let Some(path) = path else {
        write_doctor_line(
            stdout,
            "SKIP",
            label,
            format!("not provided; pass --{label} to validate the image"),
        )?;
        return Ok(0);
    };

    match fs::read(path) {
        Ok(payload) => {
            let size = u64::try_from(payload.len()).unwrap_or(u64::MAX);
            if size == 0 {
                write_doctor_line(
                    stdout,
                    "FAIL",
                    label,
                    format!("{} is empty", path.display()),
                )?;
                Ok(1)
            } else if size > max_bytes {
                write_doctor_line(
                    stdout,
                    "FAIL",
                    label,
                    format!(
                        "{} is {size} bytes, exceeding the allocated flash window of {max_bytes} bytes",
                        path.display()
                    ),
                )?;
                Ok(1)
            } else {
                write_doctor_line(
                    stdout,
                    "PASS",
                    label,
                    format!(
                        "{} is readable and fits in {max_bytes} bytes",
                        path.display()
                    ),
                )?;
                Ok(0)
            }
        }
        Err(error) => {
            write_doctor_line(
                stdout,
                "FAIL",
                label,
                format!("{} could not be read: {error}", path.display()),
            )?;
            Ok(1)
        }
    }
}

fn write_doctor_line(
    stdout: &mut dyn Write,
    status: &str,
    label: &str,
    detail: impl std::fmt::Display,
) -> Result<(), RunError> {
    writeln!(stdout, "[{status}] {label}: {detail}")
        .map_err(|source| stdout_io_error("writing doctor output", &source))
}

fn discover_ports() -> Result<Vec<SerialPortInfo>, RunError> {
    let mut ports = available_ports().map_err(|error| {
        RunError::Serial(io::Error::other(format!(
            "failed to enumerate serial ports: {error}",
        )))
    })?;
    normalize_discovered_ports(&mut ports);
    ports.sort_by(|left, right| left.port_name.cmp(&right.port_name));
    Ok(ports)
}

fn select_recover_port(ports: &[SerialPortInfo]) -> Result<String, RunError> {
    let plausible = plausible_ports(ports);

    match plausible.as_slice() {
        [port] => Ok(port.port_name.clone()),
        [] => Err(RunError::Input(InputError::new(
            "automatic port selection found no plausible serial ports; run `unbrk ports` or pass --port explicitly",
        ))),
        _ => Err(RunError::Input(InputError::new(format!(
            "automatic port selection found multiple plausible serial ports ({}); pass --port explicitly",
            plausible
                .iter()
                .map(|port| port.port_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))),
    }
}

fn plausible_ports(ports: &[SerialPortInfo]) -> Vec<&SerialPortInfo> {
    ports
        .iter()
        .filter(|port| is_plausible_recovery_port(port))
        .collect()
}

fn normalize_discovered_ports(ports: &mut [SerialPortInfo]) {
    for port in ports {
        port.port_name = normalize_port_name(&port.port_name);
    }
}

fn normalize_port_name(port_name: &str) -> String {
    let path = Path::new(port_name);
    let Ok(relative_path) = path.strip_prefix("/sys/class/tty") else {
        return String::from(port_name);
    };
    let Some(file_name) = relative_path.file_name() else {
        return String::from(port_name);
    };
    format!("/dev/{}", file_name.to_string_lossy())
}

fn is_plausible_recovery_port(port: &SerialPortInfo) -> bool {
    if matches!(port.port_type, SerialPortType::BluetoothPort) {
        return false;
    }

    let name = Path::new(&port.port_name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(port.port_name.as_str())
        .to_ascii_lowercase();
    if name.contains("modem") || name.contains("rfcomm") || name.contains("bluetooth") {
        return false;
    }

    matches!(port.port_type, SerialPortType::UsbPort(_))
        || name.contains("ttyusb")
        || name.contains("ttyacm")
        || name.contains("tty.usb")
        || name.contains("cu.usb")
        || name.starts_with("com")
}

fn render_ports_listing(ports: &[SerialPortInfo]) -> Vec<String> {
    ports.iter().map(render_port_line).collect()
}

fn render_port_line(port: &SerialPortInfo) -> String {
    let plausibility = if is_plausible_recovery_port(port) {
        "plausible"
    } else {
        "ignored"
    };
    format!(
        "{} [{}] {}",
        port.port_name,
        plausibility,
        describe_port_type(&port.port_type)
    )
}

fn describe_port_type(port_type: &SerialPortType) -> String {
    match port_type {
        SerialPortType::UsbPort(info) => format!(
            "USB VID:{:04x} PID:{:04x}{}{}{}",
            info.vid,
            info.pid,
            info.manufacturer
                .as_deref()
                .map_or(String::new(), |value| format!(" manufacturer={value}")),
            info.product
                .as_deref()
                .map_or(String::new(), |value| format!(" product={value}")),
            info.serial_number
                .as_deref()
                .map_or(String::new(), |value| format!(" serial={value}")),
        ),
        SerialPortType::PciPort => String::from("PCI serial"),
        SerialPortType::BluetoothPort => String::from("Bluetooth serial"),
        SerialPortType::Unknown => String::from("Unknown transport"),
    }
}

fn handoff_console(transport: &mut impl Transport, stdout: &mut dyn Write) -> Result<(), RunError> {
    writeln!(
        stdout,
        "Entering interactive console handoff. Press Ctrl-C or Ctrl-D to exit."
    )
    .map_err(|source| stdout_io_error("writing the console handoff banner", &source))?;
    stdout
        .flush()
        .map_err(|source| stdout_io_error("flushing the console handoff banner", &source))?;

    let _raw_mode = RawModeGuard::enable()?;
    transport
        .set_timeout(CONSOLE_HANDOFF_POLL_INTERVAL)
        .map_err(|source| serial_run_error("setting the console handoff timeout", &source))?;
    relay_console_handoff(transport, stdout)?;

    writeln!(stdout, "\r\nLeft interactive console handoff.")
        .map_err(|source| stdout_io_error("writing the console handoff footer", &source))?;
    stdout
        .flush()
        .map_err(|source| stdout_io_error("flushing the console handoff footer", &source))
}

fn relay_console_handoff(
    transport: &mut impl Transport,
    stdout: &mut dyn Write,
) -> Result<(), RunError> {
    let mut serial_buffer = [0_u8; 256];

    loop {
        if event::poll(CONSOLE_HANDOFF_POLL_INTERVAL)
            .map_err(|source| terminal_run_error("polling terminal input", &source))?
        {
            match event::read()
                .map_err(|source| terminal_run_error("reading terminal input", &source))?
            {
                TerminalEvent::Key(event) => match console_action_for_key_event(event) {
                    ConsoleAction::Ignore => {}
                    ConsoleAction::Exit => break,
                    ConsoleAction::Send(bytes) => {
                        transport.write(&bytes).map_err(|source| {
                            serial_run_error("writing console input to the serial port", &source)
                        })?;
                        transport.flush().map_err(|source| {
                            serial_run_error("flushing console input to the serial port", &source)
                        })?;
                    }
                },
                TerminalEvent::Paste(contents) if !contents.is_empty() => {
                    transport.write(contents.as_bytes()).map_err(|source| {
                        serial_run_error("writing pasted console input to the serial port", &source)
                    })?;
                    transport.flush().map_err(|source| {
                        serial_run_error(
                            "flushing pasted console input to the serial port",
                            &source,
                        )
                    })?;
                }
                _ => {}
            }
        }

        match transport.read(&mut serial_buffer) {
            Ok(0) => break,
            Ok(read_len) => {
                stdout
                    .write_all(&serial_buffer[..read_len])
                    .map_err(|source| stdout_io_error("writing console output", &source))?;
                stdout
                    .flush()
                    .map_err(|source| stdout_io_error("flushing console output", &source))?;
            }
            Err(source) if source.kind() == io::ErrorKind::TimedOut => {}
            Err(source) => {
                return Err(serial_run_error(
                    "reading console output from the serial port",
                    &source,
                ));
            }
        }
    }

    Ok(())
}

fn console_action_for_key_event(event: KeyEvent) -> ConsoleAction {
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return ConsoleAction::Ignore;
    }

    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return match event.code {
            KeyCode::Char('c' | 'C' | 'd' | 'D') => ConsoleAction::Exit,
            KeyCode::Char(character) if character.is_ascii() => {
                ConsoleAction::Send(vec![character.to_ascii_lowercase() as u8 & 0x1f])
            }
            _ => ConsoleAction::Ignore,
        };
    }

    match event.code {
        KeyCode::Backspace => ConsoleAction::Send(vec![0x08]),
        KeyCode::Enter => ConsoleAction::Send(vec![b'\r']),
        KeyCode::Tab => ConsoleAction::Send(vec![b'\t']),
        KeyCode::Esc => ConsoleAction::Send(vec![0x1b]),
        KeyCode::Char(character) => ConsoleAction::Send(character.to_string().into_bytes()),
        KeyCode::Up => ConsoleAction::Send(b"\x1b[A".to_vec()),
        KeyCode::Down => ConsoleAction::Send(b"\x1b[B".to_vec()),
        KeyCode::Right => ConsoleAction::Send(b"\x1b[C".to_vec()),
        KeyCode::Left => ConsoleAction::Send(b"\x1b[D".to_vec()),
        KeyCode::Home => ConsoleAction::Send(b"\x1b[H".to_vec()),
        KeyCode::End => ConsoleAction::Send(b"\x1b[F".to_vec()),
        KeyCode::Delete => ConsoleAction::Send(b"\x1b[3~".to_vec()),
        _ => ConsoleAction::Ignore,
    }
}

fn serial_run_error(operation: &'static str, source: &io::Error) -> RunError {
    RunError::Serial(io::Error::new(
        source.kind(),
        format!("serial I/O failed while {operation}: {source}"),
    ))
}

fn terminal_run_error(operation: &'static str, source: &io::Error) -> RunError {
    terminal_io_error(operation, source)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConsoleAction {
    Ignore,
    Exit,
    Send(Vec<u8>),
}

#[derive(Debug)]
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self, RunError> {
        enable_raw_mode().map_err(|source| terminal_run_error("enabling raw mode", &source))?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ignored = disable_raw_mode();
    }
}

fn open_transport(port: &str, args: &RecoverArgs) -> Result<CliTransport, RunError> {
    let initial_timeout = duration_override(args.prompt_timeout, DEFAULT_PROMPT_TIMEOUT);
    let serial = SerialTransport::open(port.to_owned(), args.baud, initial_timeout)
        .map_err(RunError::Serial)?;
    let transcript: Box<dyn Write> = match args.transcript_file.as_deref() {
        Some(path) => Box::new(BufWriter::new(File::create(path).map_err(|error| {
            RunError::Input(InputError::new(format!(
                "failed to create transcript file {}: {error}",
                path.display()
            )))
        })?)),
        None => Box::new(io::sink()),
    };

    Ok(TranscriptTransport::new(serial, transcript))
}

fn execute_recover(
    plan: &RecoverPlan,
    target: TargetProfile,
    transport: &mut CliTransport,
    events: &mut Vec<Event>,
    reporter: &mut ProgressReporter<'_>,
) -> Result<RecoverOutcome, RunError> {
    let recovery_config = RecoveryConfig::new(
        duration_override(plan.args.prompt_timeout, DEFAULT_PROMPT_TIMEOUT),
        xmodem_config(&plan.args),
    );
    let flash_config = FlashConfig::new(
        duration_override(plan.args.command_timeout, DEFAULT_COMMAND_TIMEOUT),
        duration_override(plan.args.reset_timeout, DEFAULT_RESET_TIMEOUT),
        xmodem_config(&plan.args),
    );

    if plan.args.resume_from_uboot {
        if plan.args.flash_persistent {
            let flash_plan = build_flash_plan(&plan.args, &target)?;
            let flash_report =
                flash_from_uboot(transport, target, &flash_plan, flash_config, |event| {
                    record_core_event(events, reporter, event.clone());
                })
                .map_err(RunError::from)?;
            return Ok(RecoverOutcome::FlashedFromExistingPrompt {
                reset_evidence: flash_report.reset_evidence,
            });
        }

        let prompt = wait_for_uboot_prompt(
            transport,
            &target.prompts.uboot,
            duration_override(plan.args.command_timeout, DEFAULT_COMMAND_TIMEOUT),
        )
        .map_err(RunError::from)?;
        record_local_event(events, reporter, EventPayload::UBootPromptSeen { prompt });
        record_local_event(
            events,
            reporter,
            EventPayload::HandoffReady {
                interactive_console: plan.console_handoff_allowed,
            },
        );
        return Ok(RecoverOutcome::Recovered);
    }

    let preloader_path = required_image_path(plan.args.preloader.as_ref(), "--preloader")?;
    let fip_path = required_image_path(plan.args.fip.as_ref(), "--fip")?;
    let preloader = fs::read(preloader_path).map_err(|error| {
        RunError::Input(InputError::new(format!(
            "failed to read preloader image {}: {error}",
            preloader_path.display()
        )))
    })?;
    let fip = fs::read(fip_path).map_err(|error| {
        RunError::Input(InputError::new(format!(
            "failed to read FIP image {}: {error}",
            fip_path.display()
        )))
    })?;
    let _recovery_report = recover_to_uboot(
        transport,
        &target,
        RecoveryImages {
            preloader_name: file_name(preloader_path),
            preloader: &preloader,
            fip_name: file_name(fip_path),
            fip: &fip,
        },
        recovery_config,
        |event| record_core_event(events, reporter, event.clone()),
    )
    .map_err(RunError::from)?;

    if plan.args.flash_persistent {
        let flash_plan = build_flash_plan(&plan.args, &target)?;
        let flash_report =
            flash_from_uboot(transport, target, &flash_plan, flash_config, |event| {
                record_core_event(events, reporter, event.clone());
            })
            .map_err(RunError::from)?;
        Ok(RecoverOutcome::FlashedAfterRecovery {
            reset_evidence: flash_report.reset_evidence,
        })
    } else {
        record_local_event(
            events,
            reporter,
            EventPayload::HandoffReady {
                interactive_console: plan.console_handoff_allowed,
            },
        );
        Ok(RecoverOutcome::Recovered)
    }
}

fn xmodem_config(args: &RecoverArgs) -> XmodemConfig {
    XmodemConfig::new(
        duration_override(args.packet_timeout, XMODEM_DEFAULT_PACKET_TIMEOUT),
        args.xmodem_block_retry
            .unwrap_or(XMODEM_DEFAULT_BLOCK_RETRY_LIMIT),
        args.xmodem_eot_retry
            .unwrap_or(XMODEM_DEFAULT_EOT_RETRY_LIMIT),
    )
}

fn target_profile(args: &RecoverArgs) -> TargetProfile {
    let uboot_prompt = args.uboot_prompt.as_ref().map_or_else(
        || AN7581.prompts.uboot,
        |source| PromptPattern::from_owned(source.clone()),
    );

    TargetProfile {
        serial: unbrk_core::target::SerialSettings {
            baud_rate: args.baud,
            ..AN7581.serial
        },
        prompts: PromptPatterns {
            uboot: uboot_prompt,
            ..AN7581.prompts
        },
        ..AN7581
    }
}

fn duration_override(override_seconds: Option<u64>, default: Duration) -> Duration {
    override_seconds.map_or(default, Duration::from_secs)
}

fn required_image_path<'a>(
    path: Option<&'a PathBuf>,
    flag: &'static str,
) -> Result<&'a Path, RunError> {
    path.map(std::path::PathBuf::as_path).ok_or_else(|| {
        RunError::Input(InputError::new(format!(
            "missing required argument at execution time: {flag}",
        )))
    })
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image")
}

fn build_flash_plan(args: &RecoverArgs, target: &TargetProfile) -> Result<FlashPlan, RunError> {
    let preloader_path = required_image_path(args.preloader.as_ref(), "--preloader")?;
    let fip_path = required_image_path(args.fip.as_ref(), "--fip")?;
    let defaults = target.flash;
    let erase_start_block = defaults.erase_range.start_block;
    let erase_block_count = BlockCount::new(
        args.erase_block_count
            .map(u32::try_from)
            .transpose()
            .map_err(block_value_error("--erase-block-count"))?
            .unwrap_or_else(|| defaults.erase_range.block_count.get()),
    );
    let preloader_start_block = BlockOffset::new(
        args.preloader_start_block
            .map(u32::try_from)
            .transpose()
            .map_err(block_value_error("--preloader-start-block"))?
            .unwrap_or_else(|| defaults.preloader.start_block.get()),
    );
    let preloader_block_count = BlockCount::new(
        args.preloader_block_count
            .map(u32::try_from)
            .transpose()
            .map_err(block_value_error("--preloader-block-count"))?
            .unwrap_or_else(|| defaults.preloader.block_count.get()),
    );
    let fip_start_block = BlockOffset::new(
        args.fip_start_block
            .map(u32::try_from)
            .transpose()
            .map_err(block_value_error("--fip-start-block"))?
            .unwrap_or_else(|| defaults.fip.start_block.get()),
    );
    let fip_block_count = BlockCount::new(
        args.fip_block_count
            .map(u32::try_from)
            .transpose()
            .map_err(block_value_error("--fip-block-count"))?
            .unwrap_or_else(|| defaults.fip.block_count.get()),
    );

    validate_block_range("the erase range", erase_start_block, erase_block_count)?;
    validate_block_range(
        "the preloader write range",
        preloader_start_block,
        preloader_block_count,
    )?;
    validate_block_range("the FIP write range", fip_start_block, fip_block_count)?;

    Ok(FlashPlan {
        block_size: defaults.block_size,
        erase_ranges: vec![EraseRange::new(erase_start_block, erase_block_count)],
        write_stages: vec![
            WriteStage::new(
                ImageKind::Preloader,
                preloader_start_block,
                preloader_block_count,
                preloader_path.to_path_buf(),
            ),
            WriteStage::new(
                ImageKind::Fip,
                fip_start_block,
                fip_block_count,
                fip_path.to_path_buf(),
            ),
        ],
    })
}

fn block_value_error(flag: &'static str) -> impl FnOnce(std::num::TryFromIntError) -> RunError {
    move |_| {
        RunError::Input(InputError::new(format!(
            "{flag} does not fit in a 32-bit MMC block value",
        )))
    }
}

fn validate_block_range(
    description: &'static str,
    start_block: BlockOffset,
    block_count: BlockCount,
) -> Result<(), RunError> {
    if start_block.get().checked_add(block_count.get()).is_none() {
        return Err(RunError::Input(InputError::new(format!(
            "{description} exceeds the 32-bit MMC block address space",
        ))));
    }

    Ok(())
}

fn wait_for_uboot_prompt(
    transport: &mut impl Transport,
    pattern: &PromptPattern,
    timeout: Duration,
) -> Result<String, UnbrkError> {
    let regex = pattern.compile().map_err(|error| UnbrkError::Protocol {
        stage: RecoveryStage::UBoot,
        detail: format!("invalid prompt regex: {error}"),
        recent_console: ConsoleTail::empty(),
    })?;
    transport
        .set_timeout(timeout)
        .map_err(|source| UnbrkError::Serial {
            operation: "setting the U-Boot prompt timeout",
            source,
        })?;
    transport
        .write(b"\r")
        .map_err(|source| UnbrkError::Serial {
            operation: "writing carriage return to wake U-Boot",
            source,
        })?;
    transport.flush().map_err(|source| UnbrkError::Serial {
        operation: "flushing carriage return to wake U-Boot",
        source,
    })?;

    let mut console = Vec::new();
    let mut cursor = 0;
    let mut scratch = [0_u8; 256];

    loop {
        if let Some(prompt) =
            advance_to_prompt_allowing_trailing_space_with_regex(&regex, &console, &mut cursor)
        {
            return Ok(prompt.prompt);
        }

        match transport.read(&mut scratch) {
            Ok(0) => {
                return Err(UnbrkError::Timeout {
                    stage: RecoveryStage::UBoot,
                    operation: "an active U-Boot prompt",
                    timeout,
                    recent_console: ConsoleTail::new(console),
                });
            }
            Ok(read_len) => console.extend_from_slice(&scratch[..read_len]),
            Err(source) if source.kind() == io::ErrorKind::TimedOut => {
                return Err(UnbrkError::Timeout {
                    stage: RecoveryStage::UBoot,
                    operation: "an active U-Boot prompt",
                    timeout,
                    recent_console: ConsoleTail::new(console),
                });
            }
            Err(source) => {
                return Err(UnbrkError::Serial {
                    operation: "reading U-Boot prompt output",
                    source,
                });
            }
        }
    }
}

struct FancyProgressRenderer {
    bar: ProgressBar,
    flash_persistent: bool,
}

impl FancyProgressRenderer {
    fn new(plan: &RecoverPlan) -> Self {
        let bar = ProgressBar::new_spinner();
        bar.enable_steady_tick(Duration::from_millis(120));
        let renderer = Self {
            bar,
            flash_persistent: plan.args.flash_persistent,
        };
        renderer.set_waiting_message(if plan.args.resume_from_uboot {
            "Waiting for live U-Boot prompt"
        } else {
            "Waiting for recovery prompt (power-cycle the board into recovery mode)"
        });
        renderer
    }

    fn observe(&self, event: &Event) {
        match &event.payload {
            EventPayload::PromptWaiting {
                stage,
                elapsed_secs,
                timeout_secs,
            } => {
                // Alternate between the countdown and the power-cycle
                // instruction so the user sees both without either being
                // permanently hidden. Only the initial prompt stages need
                // the hint; later stages just show the countdown.
                let show_hint = matches!(
                    stage,
                    RecoveryStage::Bootrom | RecoveryStage::PreloaderPrompt
                ) && elapsed_secs % 4 >= 2;

                if show_hint {
                    self.set_waiting(
                        "Waiting for recovery prompt (power-cycle the board into recovery mode)",
                    );
                } else {
                    self.set_waiting(format!(
                        "Waiting for {}... ({elapsed_secs}s/{timeout_secs}s)",
                        prompt_waiting_label(*stage)
                    ));
                }
            }
            EventPayload::PromptSeen {
                stage: RecoveryStage::PreloaderPrompt,
                ..
            } => {
                self.set_waiting_message("Sending x for preloader");
            }
            EventPayload::PromptSeen {
                stage: RecoveryStage::FipPrompt,
                ..
            } => {
                self.set_waiting_message("Sending x for FIP");
            }
            EventPayload::InputSent {
                stage: RecoveryStage::PreloaderPrompt,
                ..
            } => {
                self.set_waiting_message("Waiting for preloader XMODEM");
            }
            EventPayload::InputSent {
                stage: RecoveryStage::FipPrompt,
                ..
            } => {
                self.set_waiting_message("Waiting for FIP XMODEM");
            }
            EventPayload::CrcReady { stage, .. } => {
                self.set_waiting_message(match stage {
                    TransferStage::Preloader => "Uploading preloader",
                    TransferStage::Fip => "Uploading FIP",
                    TransferStage::LoadxPreloader => "Uploading preloader for flash",
                    TransferStage::LoadxFip => "Uploading FIP for flash",
                });
            }
            EventPayload::XmodemStarted {
                stage, size_bytes, ..
            } => {
                self.start_transfer(*stage, *size_bytes);
            }
            EventPayload::XmodemProgress {
                stage,
                bytes_sent,
                total_bytes,
            } => {
                self.update_transfer(*stage, *bytes_sent, *total_bytes);
            }
            EventPayload::XmodemCompleted {
                stage,
                recovered_from_eot_quirk,
                ..
            } => {
                self.complete_transfer(*stage, *recovered_from_eot_quirk);
            }
            EventPayload::UBootPromptSeen { .. } => {
                if self.flash_persistent {
                    self.set_waiting_message("Preparing persistent flash");
                } else {
                    self.finish();
                }
            }
            EventPayload::UBootCommandStarted { command } => {
                self.observe_command_started(command.as_str());
            }
            EventPayload::ImageVerified { image, .. } => {
                self.set_waiting(format!("Verified {image}; writing image to flash"));
            }
            EventPayload::ResetSeen { .. } => {
                self.set_waiting_message("Reset observed");
            }
            EventPayload::HandoffReady { .. } | EventPayload::Failure { .. } => {
                self.finish();
            }
            EventPayload::SessionStarted { .. }
            | EventPayload::PortOpened { .. }
            | EventPayload::PromptSeen { .. }
            | EventPayload::InputSent { .. }
            | EventPayload::UBootCommandCompleted { .. } => {}
        }
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }

    fn println(&self, line: impl Into<String>) {
        self.bar.println(line.into());
    }

    fn complete_transfer(&self, stage: TransferStage, recovered_from_eot_quirk: bool) {
        if recovered_from_eot_quirk {
            self.bar.println(format!(
                "Recovered from an EOT quirk while uploading {}.",
                stage_label(stage)
            ));
        }

        match stage {
            TransferStage::Preloader => self.set_waiting_message("Waiting for stage 2 prompt"),
            TransferStage::Fip => self.set_waiting_message("Waiting for live U-Boot prompt"),
            TransferStage::LoadxPreloader => {
                self.set_waiting_message("Verifying preloader before flash write");
            }
            TransferStage::LoadxFip => self.set_waiting_message("Verifying FIP before flash write"),
        }
    }

    fn observe_command_started(&self, command: &str) {
        if command == "printenv loadaddr" {
            self.set_waiting_message("Reading U-Boot load address");
        } else if command.starts_with("mmc erase ") {
            self.set_waiting_message("Erasing persistent flash");
        } else if command.starts_with("loadx ") {
            self.set_waiting_message("Waiting for XMODEM after loadx");
        } else if command == "printenv filesize" {
            self.set_waiting_message("Verifying transferred image size");
        } else if command.starts_with("mmc write ") {
            self.set_waiting_message("Writing image to flash");
        } else if command == "reset" {
            self.set_waiting_message("Waiting for reset after flashing");
        } else {
            self.set_waiting(format!("Running `{command}`"));
        }
    }

    fn start_transfer(&self, stage: TransferStage, total_bytes: u64) {
        self.bar.set_style(fancy_transfer_style());
        self.bar.enable_steady_tick(Duration::from_millis(120));
        self.bar.set_length(total_bytes);
        self.bar.set_position(0);
        self.bar
            .set_message(format!("Uploading {}", transfer_stage_label(stage)));
        self.bar.tick();
    }

    fn update_transfer(&self, stage: TransferStage, bytes_sent: u64, total_bytes: u64) {
        self.bar.set_style(fancy_transfer_style());
        self.bar.set_length(total_bytes);
        self.bar.set_position(bytes_sent);
        self.bar
            .set_message(format!("Uploading {}", transfer_stage_label(stage)));
        self.bar.tick();
    }

    fn set_waiting_message(&self, message: &'static str) {
        self.set_waiting(message);
    }

    fn set_waiting(&self, message: impl Into<String>) {
        self.bar.set_style(fancy_spinner_style());
        self.bar.enable_steady_tick(Duration::from_millis(120));
        self.bar.set_length(0);
        self.bar.set_position(0);
        self.bar.set_message(message.into());
        self.bar.tick();
    }
}

fn fancy_spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn fancy_transfer_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {msg} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
}

const fn stage_label(stage: TransferStage) -> &'static str {
    match stage {
        TransferStage::Preloader | TransferStage::LoadxPreloader => "preloader",
        TransferStage::Fip | TransferStage::LoadxFip => "FIP",
    }
}

const fn prompt_waiting_label(stage: RecoveryStage) -> &'static str {
    match stage {
        RecoveryStage::Bootrom => "the bootrom prompt",
        RecoveryStage::PreloaderPrompt => "the recovery prompt",
        RecoveryStage::FipPrompt => "the FIP prompt",
        RecoveryStage::UBoot => "the U-Boot prompt",
        RecoveryStage::FlashPlan => "the flash plan prompt",
    }
}

#[cfg(test)]
fn transfer_message(
    stage: TransferStage,
    bytes_sent: u64,
    total_bytes: u64,
    elapsed: Duration,
) -> String {
    let elapsed_millis = elapsed.as_millis();
    let rate = if elapsed_millis == 0 {
        String::from("0 B/s")
    } else {
        let per_second = u128::from(bytes_sent).saturating_mul(1000) / elapsed_millis;
        let per_second = u64::try_from(per_second).unwrap_or(u64::MAX);
        HumanBytes(per_second).to_string() + "/s"
    };

    format!(
        "Uploading {} ({}/{}, {}, {})",
        stage_label(stage),
        HumanBytes(bytes_sent),
        HumanBytes(total_bytes),
        rate,
        HumanDuration(elapsed),
    )
}

struct ProgressReporter<'a> {
    writer: &'a mut dyn Write,
    mode: ProgressReporterMode,
    output_error: Option<RunError>,
}

enum ProgressReporterMode {
    Off,
    Fancy(FancyProgressRenderer),
    Plain(PlainProgressRenderer),
}

impl<'a> ProgressReporter<'a> {
    fn new(plan: &RecoverPlan, writer: &'a mut dyn Write) -> Self {
        let mode = match plan.progress_mode {
            ResolvedProgressMode::Off => ProgressReporterMode::Off,
            ResolvedProgressMode::Fancy => {
                ProgressReporterMode::Fancy(FancyProgressRenderer::new(plan))
            }
            ResolvedProgressMode::Plain => {
                ProgressReporterMode::Plain(PlainProgressRenderer::default())
            }
        };

        Self {
            writer,
            mode,
            output_error: None,
        }
    }

    fn write_startup_banner(&mut self, plan: &RecoverPlan, port: &str, target: &TargetProfile) {
        let fancy = matches!(self.mode, ProgressReporterMode::Fancy(_));
        let lines = recover_startup_banner(plan, port, target, fancy);
        match &self.mode {
            ProgressReporterMode::Off => {}
            ProgressReporterMode::Fancy(renderer) => {
                for line in lines {
                    renderer.println(line);
                }
            }
            ProgressReporterMode::Plain(_) => {
                for line in lines {
                    self.write_line(line);
                }
            }
        }
    }

    fn observe(&mut self, event: &Event) {
        let line = match &mut self.mode {
            ProgressReporterMode::Off => None,
            ProgressReporterMode::Fancy(renderer) => {
                renderer.observe(event);
                None
            }
            ProgressReporterMode::Plain(renderer) => renderer.render_event(event),
        };

        if let Some(line) = line {
            self.write_line(line);
        }
    }

    fn finish(&self) {
        if let ProgressReporterMode::Fancy(renderer) = &self.mode {
            renderer.finish();
        }
    }

    fn writer(&mut self) -> &mut dyn Write {
        self.writer
    }

    fn output_result(&mut self) -> Result<(), RunError> {
        self.output_error.take().map_or(Ok(()), Err)
    }

    fn write_line(&mut self, line: impl AsRef<str>) {
        if self.output_error.is_some() {
            return;
        }

        if let Err(source) = writeln!(self.writer, "{}", line.as_ref()) {
            self.output_error = Some(stdout_io_error("writing progress output", &source));
            return;
        }

        if let Err(source) = self.writer.flush() {
            self.output_error = Some(output_io_error("flushing progress output", &source));
        }
    }
}

#[derive(Debug, Default)]
struct PlainProgressRenderer {
    transfer_progress: Option<TransferProgressState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransferProgressState {
    stage: TransferStage,
    bucket: u64,
}

impl PlainProgressRenderer {
    fn render_event(&mut self, event: &Event) -> Option<String> {
        match &event.payload {
            EventPayload::SessionStarted { .. }
            | EventPayload::PortOpened { .. }
            | EventPayload::UBootCommandCompleted { .. } => None,
            EventPayload::PromptWaiting {
                stage,
                elapsed_secs,
                timeout_secs,
            } => Some(format!(
                "Still waiting for {}... ({elapsed_secs}s/{timeout_secs}s)",
                prompt_waiting_label(*stage)
            )),
            EventPayload::PromptSeen {
                stage: RecoveryStage::PreloaderPrompt,
                ..
            } => Some(String::from("Detected the preloader prompt; sending x.")),
            EventPayload::PromptSeen {
                stage: RecoveryStage::FipPrompt,
                ..
            } => Some(String::from("Detected the FIP prompt; sending x.")),
            EventPayload::InputSent {
                stage: RecoveryStage::PreloaderPrompt,
                ..
            } => Some(String::from(
                "Requested the preloader upload; waiting for the XMODEM receiver.",
            )),
            EventPayload::InputSent {
                stage: RecoveryStage::FipPrompt,
                ..
            } => Some(String::from(
                "Requested the FIP upload; waiting for the XMODEM receiver.",
            )),
            EventPayload::PromptSeen { .. } | EventPayload::InputSent { .. } => {
                Some(event.payload.to_string())
            }
            EventPayload::CrcReady { stage, .. } => Some(format!(
                "{} is ready to receive over XMODEM.",
                transfer_stage_label(*stage)
            )),
            EventPayload::XmodemStarted {
                stage, size_bytes, ..
            } => {
                self.transfer_progress = Some(TransferProgressState {
                    stage: *stage,
                    bucket: 0,
                });
                Some(format!(
                    "Uploading {} ({} total).",
                    stage_label(*stage),
                    HumanBytes(*size_bytes)
                ))
            }
            EventPayload::XmodemProgress {
                stage,
                bytes_sent,
                total_bytes,
            } => self.render_transfer_progress(*stage, *bytes_sent, *total_bytes),
            EventPayload::XmodemCompleted {
                stage,
                recovered_from_eot_quirk,
                ..
            } => {
                self.transfer_progress = None;
                Some(if *recovered_from_eot_quirk {
                    format!(
                        "Finished uploading {} after recovering from an EOT quirk.",
                        stage_label(*stage)
                    )
                } else {
                    format!("Finished uploading {}.", stage_label(*stage))
                })
            }
            EventPayload::UBootPromptSeen { .. } => {
                Some(String::from("Reached the RAM-resident U-Boot prompt."))
            }
            EventPayload::UBootCommandStarted { command } => Some(plain_command_message(command)),
            EventPayload::ImageVerified { image, .. } => {
                Some(format!("Verified {image}; writing it to persistent flash."))
            }
            EventPayload::ResetSeen { evidence } => {
                Some(format!("Observed target reset: {evidence}."))
            }
            EventPayload::HandoffReady {
                interactive_console,
            } => Some(if *interactive_console {
                String::from("Recovery complete; interactive console handoff is ready.")
            } else {
                String::from("Recovery complete; stopping at the machine-controlled handoff point.")
            }),
            EventPayload::Failure { class, message } => {
                Some(format!("Recovery failed ({class}): {message}"))
            }
        }
    }

    fn render_transfer_progress(
        &mut self,
        stage: TransferStage,
        bytes_sent: u64,
        total_bytes: u64,
    ) -> Option<String> {
        if total_bytes == 0 {
            return None;
        }

        let percent = bytes_sent.saturating_mul(100) / total_bytes;
        let bucket = percent / 10;
        let progress_state = self
            .transfer_progress
            .unwrap_or(TransferProgressState { stage, bucket: 0 });
        if progress_state.stage == stage
            && bucket <= progress_state.bucket
            && bytes_sent < total_bytes
        {
            return None;
        }

        self.transfer_progress = Some(TransferProgressState { stage, bucket });
        Some(format!(
            "Uploading {}: {percent}% ({}/{})",
            stage_label(stage),
            HumanBytes(bytes_sent),
            HumanBytes(total_bytes)
        ))
    }
}

// Emoji with graceful fallback on terminals that don't support Unicode.
static EMOJI_WRENCH: Emoji<'_, '_> = Emoji("🔧 ", "");
static EMOJI_PACKAGE: Emoji<'_, '_> = Emoji("📦 ", "");
static EMOJI_HOURGLASS: Emoji<'_, '_> = Emoji("⏳ ", "");
static EMOJI_TIMER: Emoji<'_, '_> = Emoji("⏱  ", "");
static EMOJI_MONITOR: Emoji<'_, '_> = Emoji("🖥  ", "");
static EMOJI_RESUME: Emoji<'_, '_> = Emoji("🔄 ", "");
const FANCY_UNBRK_LOGO_LINES: [&str; 5] = [
    "██  ██  ██  ██  █████   █████   ██  ██",
    "██  ██  ███ ██  ██  ██  ██  ██  ██ ██ ",
    "██  ██  ██████  █████   █████   ████  ",
    "██  ██  ██ ███  ██  ██  ██ ██   ██ ██ ",
    " ████   ██  ██  █████   ██  ██  ██  ██",
];
const FANCY_UNBRK_LOGO_GRADIENT: [u8; 5] = [19, 21, 27, 33, 39];

/// Build a styled label for the startup banner (bold cyan, right-aligned).
fn banner_label(text: &str) -> String {
    let label = Style::new().bold().cyan().for_stderr().apply_to(text);
    format!("{label:>12}")
}

fn banner_logo_lines() -> Vec<String> {
    FANCY_UNBRK_LOGO_LINES
        .iter()
        .zip(FANCY_UNBRK_LOGO_GRADIENT)
        .map(|(line, color)| {
            Style::new()
                .bold()
                .color256(color)
                .for_stderr()
                .apply_to(*line)
                .to_string()
        })
        .collect()
}

fn recover_startup_banner(
    plan: &RecoverPlan,
    port: &str,
    target: &TargetProfile,
    fancy: bool,
) -> Vec<String> {
    if fancy {
        fancy_startup_banner(plan, port, target)
    } else {
        plain_startup_banner(plan, port, target)
    }
}

fn fancy_startup_banner(plan: &RecoverPlan, port: &str, target: &TargetProfile) -> Vec<String> {
    let mut lines = banner_logo_lines();
    lines.push(String::new());
    lines.push(format!(
        "{}{} {port} \u{00b7} {} \u{00b7} {} baud",
        EMOJI_WRENCH,
        banner_label("Recovery"),
        target.name,
        plan.args.baud,
    ));

    if plan.args.resume_from_uboot {
        lines.push(format!(
            "{}{} resume from existing U-Boot prompt",
            EMOJI_RESUME,
            banner_label("Mode"),
        ));
    } else {
        lines.push(format!(
            "{}{} {}",
            EMOJI_PACKAGE,
            banner_label("Preloader"),
            display_optional_path(plan.args.preloader.as_deref()),
        ));
        lines.push(format!(
            "{}{} {}",
            EMOJI_PACKAGE,
            banner_label("FIP"),
            display_optional_path(plan.args.fip.as_deref()),
        ));
        let action_hint = Style::new()
            .yellow()
            .for_stderr()
            .apply_to("power-cycle board into recovery mode");
        lines.push(format!(
            "{}{} recovery prompt \u{2014} {action_hint}",
            EMOJI_HOURGLASS,
            banner_label("Waiting"),
        ));
    }

    lines.push(format!(
        "{}{} prompt {} \u{00b7} packet {} \u{00b7} command {} \u{00b7} reset {}",
        EMOJI_TIMER,
        banner_label("Timeouts"),
        HumanDuration(duration_override(
            plan.args.prompt_timeout,
            DEFAULT_PROMPT_TIMEOUT
        )),
        HumanDuration(duration_override(
            plan.args.packet_timeout,
            XMODEM_DEFAULT_PACKET_TIMEOUT
        )),
        HumanDuration(duration_override(
            plan.args.command_timeout,
            DEFAULT_COMMAND_TIMEOUT
        )),
        HumanDuration(duration_override(
            plan.args.reset_timeout,
            DEFAULT_RESET_TIMEOUT
        )),
    ));

    let handoff_value = if plan.console_handoff_allowed {
        "interactive after recovery"
    } else {
        "disabled; stop at U-Boot"
    };
    lines.push(format!(
        "{}{} {handoff_value}",
        EMOJI_MONITOR,
        banner_label("Console"),
    ));

    lines
}

fn plain_startup_banner(plan: &RecoverPlan, port: &str, target: &TargetProfile) -> Vec<String> {
    let mut lines = vec![format!(
        "Starting recovery on {port} at {} baud for target {}.",
        plan.args.baud, target.name
    )];

    if plan.args.resume_from_uboot {
        lines.push(String::from("Mode: resume from an existing U-Boot prompt."));
    } else {
        lines.push(format!(
            "Images: preloader={}, FIP={}.",
            display_optional_path(plan.args.preloader.as_deref()),
            display_optional_path(plan.args.fip.as_deref()),
        ));
        lines.push(String::from(
            "Waiting for the recovery prompt. If the board is not already in recovery mode,",
        ));
        lines.push(String::from(
            "power it off, hold the reset button, and power on while holding the button.",
        ));
    }

    lines.push(format!(
        "Timeouts: prompt {}, packet {}, command {}, reset {}.",
        HumanDuration(duration_override(
            plan.args.prompt_timeout,
            DEFAULT_PROMPT_TIMEOUT
        )),
        HumanDuration(duration_override(
            plan.args.packet_timeout,
            XMODEM_DEFAULT_PACKET_TIMEOUT
        )),
        HumanDuration(duration_override(
            plan.args.command_timeout,
            DEFAULT_COMMAND_TIMEOUT
        )),
        HumanDuration(duration_override(
            plan.args.reset_timeout,
            DEFAULT_RESET_TIMEOUT
        )),
    ));
    lines.push(format!(
        "Console handoff: {}.",
        if plan.console_handoff_allowed {
            "interactive after recovery"
        } else {
            "disabled; stop at U-Boot"
        }
    ));

    lines
}

fn display_optional_path(path: Option<&Path>) -> String {
    path.map_or_else(|| String::from("n/a"), |path| path.display().to_string())
}

const fn transfer_stage_label(stage: TransferStage) -> &'static str {
    match stage {
        TransferStage::Preloader => "Preloader",
        TransferStage::Fip => "FIP",
        TransferStage::LoadxPreloader => "Preloader flash staging image",
        TransferStage::LoadxFip => "FIP flash staging image",
    }
}

fn plain_command_message(command: &str) -> String {
    if command == "printenv loadaddr" {
        String::from("Reading the U-Boot load address.")
    } else if command.starts_with("mmc erase ") {
        String::from("Erasing the persistent flash region.")
    } else if command.starts_with("loadx ") {
        String::from("Waiting for XMODEM after the loadx command.")
    } else if command == "printenv filesize" {
        String::from("Verifying the transferred image size.")
    } else if command.starts_with("mmc write ") {
        String::from("Writing the image to persistent flash.")
    } else if command == "reset" {
        String::from("Waiting for the post-flash reset.")
    } else {
        format!("Running `{command}`.")
    }
}

fn record_local_event(
    events: &mut Vec<Event>,
    reporter: &mut ProgressReporter<'_>,
    payload: EventPayload,
) {
    push_event(events, payload);
    observe_last_event(events, reporter);
}

fn record_core_event(events: &mut Vec<Event>, reporter: &mut ProgressReporter<'_>, event: Event) {
    push_existing_event(events, event);
    observe_last_event(events, reporter);
}

fn observe_last_event(events: &[Event], reporter: &mut ProgressReporter<'_>) {
    if let Some(event) = events.last() {
        reporter.observe(event);
    }
}

fn timeout_hint(events: &[Event]) -> Option<&'static str> {
    match events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::PromptSeen {
            stage: RecoveryStage::PreloaderPrompt,
            ..
        }
        | EventPayload::InputSent {
            stage: RecoveryStage::PreloaderPrompt,
            ..
        } => Some("hint: confirm the board is in recovery mode and emitting the initial prompt before retrying"),
        EventPayload::CrcReady {
            stage: TransferStage::Preloader,
            ..
        }
        | EventPayload::XmodemStarted {
            stage: TransferStage::Preloader,
            ..
        }
        | EventPayload::XmodemProgress {
            stage: TransferStage::Preloader,
            ..
        }
        | EventPayload::XmodemCompleted {
            stage: TransferStage::Preloader,
            ..
        } => Some("hint: the preloader transfer started but stage 2 never appeared; check UART integrity and power-cycle back into recovery mode"),
        EventPayload::PromptSeen {
            stage: RecoveryStage::FipPrompt,
            ..
        }
        | EventPayload::InputSent {
            stage: RecoveryStage::FipPrompt,
            ..
        }
        | EventPayload::CrcReady {
            stage: TransferStage::Fip,
            ..
        }
        | EventPayload::XmodemStarted {
            stage: TransferStage::Fip,
            ..
        }
        | EventPayload::XmodemProgress {
            stage: TransferStage::Fip,
            ..
        }
        | EventPayload::XmodemCompleted {
            stage: TransferStage::Fip,
            ..
        } => Some("hint: the FIP path stalled before a live U-Boot prompt; verify the image pair matches the target and retry from a fresh power cycle"),
        EventPayload::UBootCommandStarted { .. }
        | EventPayload::UBootCommandCompleted { .. }
        | EventPayload::ImageVerified { .. } => Some("hint: the persistent flash phase timed out; keep the serial console attached and check storage access plus reset evidence"),
        _ => None,
    }) {
        Some(message) => Some(message),
        None if events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::PortOpened { .. })) =>
        {
            Some("hint: check UART access, board power, and whether the target is emitting any recovery prompt")
        }
        None => None,
    }
}

#[cfg(test)]
fn append_events(events: &mut Vec<Event>, appended: Vec<Event>) {
    for event in appended {
        push_existing_event(events, event);
    }
}

fn push_event(events: &mut Vec<Event>, payload: EventPayload) {
    let sequence = next_sequence(events);
    events.push(
        Event::now(sequence, payload.clone()).unwrap_or_else(|_| Event::new(sequence, 0, payload)),
    );
}

fn push_existing_event(events: &mut Vec<Event>, event: Event) {
    events.push(Event::new(
        next_sequence(events),
        event.timestamp_unix_ms,
        event.payload,
    ));
}

fn next_sequence(events: &[Event]) -> u64 {
    u64::try_from(events.len())
        .unwrap_or(u64::MAX.saturating_sub(1))
        .saturating_add(1)
}

fn write_events(writer: &mut dyn Write, events: &[Event]) -> Result<(), RunError> {
    for event in events {
        serde_json::to_writer(&mut *writer, event).map_err(|error| map_json_event_error(&error))?;
        writeln!(writer)
            .map_err(|source| output_io_error("writing the JSON event stream", &source))?;
    }

    Ok(())
}

fn map_json_event_error(error: &serde_json::Error) -> RunError {
    if error.is_io() {
        RunError::Io(io::Error::other(format!(
            "output I/O failed while writing the JSON event stream: {error}",
        )))
    } else {
        RunError::Protocol(format!("failed to serialize JSON event stream: {error}"))
    }
}

fn flush_event_writer(writer: &mut dyn Write) -> Result<(), RunError> {
    writer
        .flush()
        .map_err(|error| output_io_error("flushing the event stream", &error))
}

fn write_events_and_flush(writer: &mut dyn Write, events: &[Event]) -> Result<(), RunError> {
    write_events(writer, events)?;
    flush_event_writer(writer)
}

fn write_events_to_path(path: &Path, events: &[Event]) -> Result<(), RunError> {
    let file = File::create(path).map_err(|error| {
        RunError::Input(InputError::new(format!(
            "failed to create log file {}: {error}",
            path.display()
        )))
    })?;
    let mut writer = BufWriter::new(file);
    write_events_and_flush(&mut writer, events)
}

fn write_event_trace(writer: &mut dyn Write, events: &[Event]) -> Result<(), RunError> {
    for event in events {
        writeln!(writer, "{event}")
            .map_err(|source| output_io_error("writing the event trace", &source))?;
    }
    Ok(())
}

fn write_recover_summary(
    stdout: &mut dyn Write,
    plan: &RecoverPlan,
    port: &str,
    outcome: &RecoverOutcome,
    console_handoff_completed: bool,
) -> Result<(), RunError> {
    writeln!(
        stdout,
        "Recovery finished on {port} at {} baud.",
        plan.args.baud,
    )
    .map_err(|source| stdout_io_error("writing the recovery summary header", &source))?;

    match outcome {
        RecoverOutcome::Recovered => {
            if console_handoff_completed {
                writeln!(
                    stdout,
                    "Interactive console handoff ended at the RAM-resident U-Boot prompt."
                )
                .map_err(|source| {
                    stdout_io_error("writing the recovery summary outcome", &source)
                })?;
            } else {
                writeln!(stdout, "Reached the RAM-resident U-Boot prompt.").map_err(|source| {
                    stdout_io_error("writing the recovery summary outcome", &source)
                })?;
            }
        }
        RecoverOutcome::FlashedAfterRecovery { reset_evidence } => {
            writeln!(
                stdout,
                "Completed recovery and persistent flash. Observed reset evidence: {reset_evidence}"
            )
            .map_err(|source| stdout_io_error("writing the recovery summary outcome", &source))?;
        }
        RecoverOutcome::FlashedFromExistingPrompt { reset_evidence } => {
            writeln!(
                stdout,
                "Resumed from an existing U-Boot prompt and completed the persistent flash. Observed reset evidence: {reset_evidence}"
            )
            .map_err(|source| stdout_io_error("writing the recovery summary outcome", &source))?;
        }
    }

    Ok(())
}

#[derive(Debug, Parser)]
#[command(
    name = "unbrk",
    about = "UART recovery automation for supported Airoha targets",
    version,
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = EXIT_CODES_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Recover(Box<RecoverArgs>),
    Ports,
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
    Man,
    Doctor(Box<DoctorArgs>),
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "Independent CLI flags map directly to clap-derived fields."
)]
#[derive(Debug, Clone, Args)]
struct RecoverArgs {
    #[arg(long)]
    port: Option<String>,
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
    #[arg(long)]
    preloader: Option<PathBuf>,
    #[arg(long)]
    fip: Option<PathBuf>,
    #[arg(long, value_name = "SECONDS")]
    prompt_timeout: Option<u64>,
    #[arg(long, value_name = "SECONDS")]
    packet_timeout: Option<u64>,
    #[arg(long, value_name = "COUNT")]
    xmodem_block_retry: Option<u32>,
    #[arg(long, value_name = "COUNT")]
    xmodem_eot_retry: Option<u32>,
    #[arg(long, value_name = "SECONDS")]
    command_timeout: Option<u64>,
    #[arg(long, value_name = "SECONDS")]
    reset_timeout: Option<u64>,
    #[arg(long)]
    log_file: Option<PathBuf>,
    #[arg(long)]
    transcript_file: Option<PathBuf>,
    #[arg(long)]
    uboot_prompt: Option<String>,
    #[arg(long)]
    flash_persistent: bool,
    #[arg(long)]
    resume_from_uboot: bool,
    #[arg(long, value_enum)]
    progress: Option<ProgressMode>,
    #[arg(long)]
    non_interactive: bool,
    #[arg(long)]
    json: bool,
    #[arg(
        long,
        action = ArgAction::Set,
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true
    )]
    no_console: Option<bool>,
    #[arg(long, value_name = "BLOCK", value_parser = parse_u_boot_int)]
    erase_block_count: Option<u64>,
    #[arg(long, value_name = "BLOCK", value_parser = parse_u_boot_int)]
    preloader_start_block: Option<u64>,
    #[arg(long, value_name = "COUNT", value_parser = parse_u_boot_int)]
    preloader_block_count: Option<u64>,
    #[arg(long, value_name = "BLOCK", value_parser = parse_u_boot_int)]
    fip_start_block: Option<u64>,
    #[arg(long, value_name = "COUNT", value_parser = parse_u_boot_int)]
    fip_block_count: Option<u64>,
}

impl RecoverArgs {
    fn has_flash_layout_overrides(&self) -> bool {
        [
            self.erase_block_count,
            self.preloader_start_block,
            self.preloader_block_count,
            self.fip_start_block,
            self.fip_block_count,
        ]
        .into_iter()
        .any(|value| value.is_some())
    }
}

#[derive(Debug, Clone, Args)]
struct DoctorArgs {
    #[arg(long)]
    port: Option<String>,
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
    #[arg(long)]
    preloader: Option<PathBuf>,
    #[arg(long)]
    fip: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProgressMode {
    Auto,
    Plain,
    Fancy,
    Off,
}

impl ProgressMode {
    const fn resolve(self, stdout_is_tty: bool) -> ResolvedProgressMode {
        match self {
            Self::Auto if stdout_is_tty => ResolvedProgressMode::Fancy,
            Self::Auto | Self::Plain => ResolvedProgressMode::Plain,
            Self::Fancy => ResolvedProgressMode::Fancy,
            Self::Off => ResolvedProgressMode::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedProgressMode {
    Plain,
    Fancy,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalStatus {
    stdin_is_tty: bool,
    stdout_is_tty: bool,
}

impl TerminalStatus {
    fn detect() -> Self {
        Self {
            stdin_is_tty: io::stdin().is_terminal(),
            stdout_is_tty: io::stdout().is_terminal(),
        }
    }
}

#[derive(Debug)]
enum CommandPlan {
    Recover(Box<RecoverPlan>),
    Ports,
    Completions { shell: Shell },
    Man,
    Doctor(Box<DoctorArgs>),
}

#[derive(Debug)]
struct RecoverPlan {
    args: RecoverArgs,
    progress_mode: ResolvedProgressMode,
    console_handoff_allowed: bool,
    terminal_status: TerminalStatus,
}

#[derive(Debug)]
pub enum RunError {
    Input(InputError),
    Io(io::Error),
    Serial(io::Error),
    Timeout(String),
    Protocol(String),
    Xmodem(String),
    UBootCommand(String),
    VerificationMismatch(String),
    UserAbort(String),
}

impl RunError {
    #[must_use]
    pub const fn failure_class(&self) -> FailureClass {
        match self {
            Self::Input(_) => FailureClass::BadInput,
            Self::Io(_) => FailureClass::Io,
            Self::Serial(_) => FailureClass::Serial,
            Self::Timeout(_) => FailureClass::Timeout,
            Self::Protocol(_) => FailureClass::Protocol,
            Self::Xmodem(_) => FailureClass::Xmodem,
            Self::UBootCommand(_) => FailureClass::UBootCommand,
            Self::VerificationMismatch(_) => FailureClass::VerificationMismatch,
            Self::UserAbort(_) => FailureClass::UserAbort,
        }
    }

    #[must_use]
    pub const fn exit_code(&self) -> CliExitCode {
        match self {
            Self::Input(_) => CliExitCode::BadInput,
            Self::Io(_) | Self::Serial(_) => CliExitCode::IoError,
            Self::Timeout(_) => CliExitCode::Timeout,
            Self::Protocol(_) => CliExitCode::ProtocolError,
            Self::Xmodem(_) => CliExitCode::XmodemFailure,
            Self::UBootCommand(_) => CliExitCode::UBootCommandFailure,
            Self::VerificationMismatch(_) => CliExitCode::VerificationMismatch,
            Self::UserAbort(_) => CliExitCode::UserAbort,
        }
    }

    /// Return a terminal-styled version of this error for stderr output.
    ///
    /// Uses bold red for the error category label and an emoji prefix.
    /// Styling respects `CLICOLOR` / TTY detection via the `console` crate.
    fn styled(&self) -> String {
        static EMOJI_ERROR: Emoji<'_, '_> = Emoji("\u{274c} ", "");
        let label_style = Style::new().bold().red().for_stderr();
        let (label, detail) = match self {
            Self::Input(error) => return format!("{EMOJI_ERROR}{error}"),
            Self::Io(error) => ("I/O error:", error.to_string()),
            Self::Serial(error) => ("serial error:", error.to_string()),
            Self::Timeout(msg) => ("timeout:", msg.clone()),
            Self::Protocol(msg) => ("protocol error:", msg.clone()),
            Self::Xmodem(msg) => ("xmodem failure:", msg.clone()),
            Self::UBootCommand(msg) => ("U-Boot command failure:", msg.clone()),
            Self::VerificationMismatch(msg) => ("verification mismatch:", msg.clone()),
            Self::UserAbort(msg) => ("user abort:", msg.clone()),
        };
        format!("{EMOJI_ERROR}{} {detail}", label_style.apply_to(label))
    }
}

impl From<clap::Error> for RunError {
    fn from(error: clap::Error) -> Self {
        Self::Input(InputError::from_clap(&error))
    }
}

impl From<UnbrkError> for RunError {
    fn from(error: UnbrkError) -> Self {
        match error {
            UnbrkError::Serial { operation, source } => Self::Serial(io::Error::new(
                source.kind(),
                format!("serial I/O failed while {operation}: {source}"),
            )),
            UnbrkError::Timeout { .. } => Self::Timeout(error.to_string()),
            UnbrkError::PromptMismatch { .. } | UnbrkError::Protocol { .. } => {
                Self::Protocol(error.to_string())
            }
            UnbrkError::Xmodem { .. } => Self::Xmodem(error.to_string()),
            UnbrkError::UBootCommand { .. } => Self::UBootCommand(error.to_string()),
            UnbrkError::VerificationMismatch { .. } => {
                Self::VerificationMismatch(error.to_string())
            }
            UnbrkError::BadInput { .. } => Self::Input(InputError::new(error.to_string())),
            UnbrkError::UserAbort { .. } => Self::UserAbort(error.to_string()),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Serial(error) => write!(formatter, "serial error: {error}"),
            Self::Timeout(message) => write!(formatter, "timeout: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Xmodem(message) => write!(formatter, "xmodem failure: {message}"),
            Self::UBootCommand(message) => write!(formatter, "U-Boot command failure: {message}"),
            Self::VerificationMismatch(message) => {
                write!(formatter, "verification mismatch: {message}")
            }
            Self::UserAbort(message) => write!(formatter, "user abort: {message}"),
        }
    }
}

impl std::error::Error for RunError {}

#[derive(Debug)]
pub struct InputError {
    message: String,
}

impl InputError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn from_clap(error: &clap::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InputError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExitCode {
    Success = 0,
    IoError = 1,
    Timeout = 2,
    ProtocolError = 3,
    XmodemFailure = 4,
    UBootCommandFailure = 5,
    VerificationMismatch = 6,
    BadInput = 7,
    UserAbort = 8,
}

impl From<CliExitCode> for ExitCode {
    fn from(value: CliExitCode) -> Self {
        Self::from(value as u8)
    }
}

fn parse_u_boot_int(raw: &str) -> Result<u64, String> {
    let normalized = raw.trim();
    let hex = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"));

    hex.map_or_else(
        || normalized.parse(),
        |value| u64::from_str_radix(value, 16),
    )
    .map_err(|_| format!("invalid integer literal `{raw}`"))
}

#[cfg(test)]
mod tests {
    use super::{
        CliExitCode, CommandPlan, ConsoleAction, FANCY_UNBRK_LOGO_LINES, FancyProgressRenderer,
        PlainProgressRenderer, ProgressMode, RecoverOutcome, RecoverPlan, ResolvedProgressMode,
        RunError, TerminalStatus, append_events, build_flash_plan, console_action_for_key_event,
        fancy_startup_banner, flush_event_writer, is_plausible_recovery_port, map_json_event_error,
        normalize_port_name, parse_command, plain_startup_banner, render_port_line,
        select_recover_port, target_profile, timeout_hint, transfer_message, try_run,
        wait_for_uboot_prompt, write_event_trace, write_events, write_events_and_flush,
        write_recover_summary, xmodem_config,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};
    use std::fs;
    use std::fs::File;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use unbrk_core::error::UnbrkError;
    use unbrk_core::event::{Event, EventPayload, RecoveryStage, TransferStage};
    use unbrk_core::target::{
        AN7581, BlockCount, BlockOffset, BlockRange, FlashLayout, PromptPattern, TargetProfile,
    };
    use unbrk_core::xmodem::{XMODEM_DEFAULT_BLOCK_RETRY_LIMIT, XMODEM_DEFAULT_EOT_RETRY_LIMIT};
    use unbrk_core::{MockStep, MockTransport};

    const PORT: &str = "/dev/ttyUSB0";
    const PRELOADER: &str = "preloader.bin";
    const FIP: &str = "image.fip";
    static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tty_status(stdout_is_tty: bool) -> TerminalStatus {
        TerminalStatus {
            stdin_is_tty: stdout_is_tty,
            stdout_is_tty,
        }
    }

    fn parse_recover(args: &[&str], terminal_status: TerminalStatus) -> RecoverPlan {
        match parse_command(args, terminal_status).unwrap() {
            CommandPlan::Recover(plan) => *plan,
            command => panic!("expected recover command, got {command:?}"),
        }
    }

    fn render(args: &[&str], terminal_status: TerminalStatus) -> String {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        try_run(args, terminal_status, &mut stdout, &mut stderr).unwrap();
        assert!(stderr.is_empty());
        String::from_utf8(stdout).unwrap()
    }

    fn fixture_event(sequence: u64, timestamp_unix_ms: u64, payload: EventPayload) -> Event {
        Event::new(sequence, timestamp_unix_ms, payload)
    }

    #[test]
    fn recover_defaults_to_fancy_progress_on_tty() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(true),
        );

        assert_eq!(plan.progress_mode, ResolvedProgressMode::Fancy);
        assert!(plan.console_handoff_allowed);
    }

    #[test]
    fn recover_defaults_to_plain_progress_without_tty() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(false),
        );

        assert_eq!(plan.progress_mode, ResolvedProgressMode::Plain);
        assert!(!plan.console_handoff_allowed);
    }

    #[test]
    fn fancy_startup_banner_prepends_the_unbrk_logo() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(true),
        );

        let lines = fancy_startup_banner(&plan, PORT, &AN7581);

        assert_eq!(&lines[..5], &FANCY_UNBRK_LOGO_LINES);
        assert!(lines[5].is_empty());
        assert!(lines[6].contains("Recovery"));
    }

    #[test]
    fn plain_startup_banner_keeps_the_text_only_layout() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(false),
        );

        let lines = plain_startup_banner(&plan, PORT, &AN7581);

        assert!(lines[0].starts_with("Starting recovery on"));
        assert!(
            lines.iter().all(|line| !line.contains("██")),
            "plain banner should not render the logo: {lines:?}"
        );
    }

    #[test]
    fn xmodem_retry_flags_default_independently() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(true),
        );

        let config = xmodem_config(&plan.args);

        assert_eq!(config.block_retry_limit, XMODEM_DEFAULT_BLOCK_RETRY_LIMIT);
        assert_eq!(config.eot_retry_limit, XMODEM_DEFAULT_EOT_RETRY_LIMIT);
    }

    #[test]
    fn xmodem_block_retry_override_leaves_eot_default() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--xmodem-block-retry",
                "4",
            ],
            tty_status(true),
        );

        let config = xmodem_config(&plan.args);

        assert_eq!(config.block_retry_limit, 4);
        assert_eq!(config.eot_retry_limit, XMODEM_DEFAULT_EOT_RETRY_LIMIT);
    }

    #[test]
    fn xmodem_eot_retry_override_leaves_block_default() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--xmodem-eot-retry",
                "2",
            ],
            tty_status(true),
        );

        let config = xmodem_config(&plan.args);

        assert_eq!(config.block_retry_limit, XMODEM_DEFAULT_BLOCK_RETRY_LIMIT);
        assert_eq!(config.eot_retry_limit, 2);
    }

    #[test]
    fn removed_combined_xmodem_retry_flag_is_rejected() {
        let error = parse_command(
            [
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--xmodem-retry",
                "5",
            ],
            tty_status(true),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("--xmodem-retry"));
    }

    #[test]
    fn json_mode_forces_progress_off_and_console_off() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--json",
            ],
            tty_status(true),
        );

        assert_eq!(plan.progress_mode, ResolvedProgressMode::Off);
        assert!(!plan.console_handoff_allowed);
    }

    #[test]
    fn explicit_progress_conflicts_with_json() {
        let error = parse_command(
            [
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--json",
                "--progress",
                "plain",
            ],
            tty_status(true),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(
            error
                .to_string()
                .contains("--progress cannot be used with --json")
        );
    }

    #[test]
    fn non_interactive_requires_an_explicit_port() {
        let error =
            parse_command(["unbrk", "recover", "--non-interactive"], tty_status(true)).unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("--non-interactive requires"));
    }

    #[test]
    fn json_rejects_explicit_no_console_false() {
        let error = parse_command(
            [
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--json",
                "--no-console=false",
            ],
            tty_status(true),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("--no-console=false"));
    }

    #[test]
    fn invalid_uboot_prompt_is_bad_input() {
        let error = parse_command(
            [
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--uboot-prompt",
                "(",
            ],
            tty_status(true),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(
            error
                .to_string()
                .contains("invalid value for --uboot-prompt")
        );
    }

    #[test]
    fn flash_layout_overrides_require_flash_persistent() {
        let error = parse_command(
            [
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--preloader-start-block",
                "4",
            ],
            tty_status(true),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("require --flash-persistent"));
    }

    #[test]
    fn explicit_progress_off_is_respected() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--progress",
                "off",
            ],
            tty_status(true),
        );

        assert_eq!(plan.progress_mode, ResolvedProgressMode::Off);
    }

    #[test]
    fn bootrom_recovery_requires_images() {
        let error =
            parse_command(["unbrk", "recover", "--port", PORT], tty_status(true)).unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("--preloader is required"));
    }

    #[test]
    fn resume_from_uboot_can_skip_images_when_not_flashing() {
        let plan = parse_recover(
            &["unbrk", "recover", "--port", PORT, "--resume-from-uboot"],
            tty_status(true),
        );

        assert!(plan.args.resume_from_uboot);
        assert!(plan.console_handoff_allowed);
    }

    #[test]
    fn flash_persistent_disables_console_handoff() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--flash-persistent",
            ],
            tty_status(true),
        );

        assert!(!plan.console_handoff_allowed);
    }

    #[test]
    fn flash_layout_overrides_accept_hex_values() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--flash-persistent",
                "--erase-block-count",
                "0x800",
                "--preloader-start-block",
                "0x4",
                "--preloader-block-count",
                "0xfc",
                "--fip-start-block",
                "0x100",
                "--fip-block-count",
                "0x700",
            ],
            tty_status(true),
        );

        assert_eq!(plan.args.erase_block_count, Some(0x800));
        assert_eq!(plan.args.preloader_start_block, Some(0x4));
        assert_eq!(plan.args.preloader_block_count, Some(0xfc));
        assert_eq!(plan.args.fip_start_block, Some(0x100));
        assert_eq!(plan.args.fip_block_count, Some(0x700));
    }

    #[test]
    fn exit_code_mapping_matches_documented_values() {
        assert_eq!(
            RunError::Input(super::InputError::new("bad input")).exit_code(),
            CliExitCode::BadInput
        );
        assert_eq!(
            RunError::Io(std::io::Error::other("boom")).exit_code(),
            CliExitCode::IoError
        );
        assert_eq!(
            RunError::Serial(std::io::Error::other("boom")).exit_code(),
            CliExitCode::IoError
        );
        assert_eq!(
            RunError::Timeout(String::from("slow")).exit_code(),
            CliExitCode::Timeout
        );
        assert_eq!(
            RunError::Protocol(String::from("bad prompt")).exit_code(),
            CliExitCode::ProtocolError
        );
        assert_eq!(
            RunError::Xmodem(String::from("cancelled")).exit_code(),
            CliExitCode::XmodemFailure
        );
        assert_eq!(
            RunError::UBootCommand(String::from("mmc erase failed")).exit_code(),
            CliExitCode::UBootCommandFailure
        );
        assert_eq!(
            RunError::VerificationMismatch(String::from("filesize")).exit_code(),
            CliExitCode::VerificationMismatch
        );
        assert_eq!(
            RunError::UserAbort(String::from("ctrl-c")).exit_code(),
            CliExitCode::UserAbort
        );
    }

    #[test]
    fn serial_error_conversion_preserves_operation_context() {
        let error = RunError::from(UnbrkError::Serial {
            operation: "writing the loadx command",
            source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
        });

        match error {
            RunError::Serial(error) => {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                assert!(
                    error
                        .to_string()
                        .contains("serial I/O failed while writing the loadx command")
                );
                assert!(error.to_string().contains("permission denied"));
            }
            other => panic!("expected a serial run error, got {other:?}"),
        }
    }

    #[test]
    fn io_error_display_uses_a_non_serial_prefix() {
        let error = RunError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));

        assert_eq!(error.to_string(), "I/O error: broken pipe");
    }

    #[test]
    fn completions_command_parses_a_known_shell() {
        let command = parse_command(["unbrk", "completions", "bash"], tty_status(true)).unwrap();

        match command {
            CommandPlan::Completions { shell } => assert_eq!(shell.to_string(), "bash"),
            command => panic!("expected completions command, got {command:?}"),
        }
    }

    #[test]
    fn completions_command_renders_shell_output() {
        let rendered = render(&["unbrk", "completions", "bash"], tty_status(true));

        assert!(rendered.contains("_unbrk"));
        assert!(rendered.contains("complete"));
    }

    #[test]
    fn man_command_renders_roff_output() {
        let rendered = render(&["unbrk", "man"], tty_status(true));

        assert!(rendered.contains(".TH unbrk 1"));
        assert!(rendered.contains("UART recovery automation"));
    }

    #[test]
    fn progress_mode_auto_resolves_against_tty_state() {
        assert_eq!(
            ProgressMode::Auto.resolve(true),
            ResolvedProgressMode::Fancy
        );
        assert_eq!(
            ProgressMode::Auto.resolve(false),
            ResolvedProgressMode::Plain
        );
    }

    #[test]
    fn fancy_renderer_tracks_major_recovery_phases() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(true),
        );
        let renderer = FancyProgressRenderer::new(&plan);

        assert_eq!(
            renderer.bar.message(),
            "Waiting for recovery prompt (power-cycle the board into recovery mode)"
        );

        renderer.observe(&fixture_event(
            1,
            0,
            EventPayload::PromptSeen {
                stage: RecoveryStage::PreloaderPrompt,
                prompt: String::from("Press x"),
            },
        ));
        assert_eq!(renderer.bar.message(), "Sending x for preloader");

        renderer.observe(&fixture_event(
            2,
            0,
            EventPayload::XmodemStarted {
                stage: TransferStage::Preloader,
                file_name: String::from(PRELOADER),
                size_bytes: 1024,
            },
        ));
        assert!(renderer.bar.message().contains("Uploading Preloader"));

        renderer.observe(&fixture_event(
            3,
            0,
            EventPayload::XmodemProgress {
                stage: TransferStage::Preloader,
                bytes_sent: 512,
                total_bytes: 1024,
            },
        ));
        assert!(renderer.bar.message().contains("Uploading Preloader"));

        renderer.observe(&fixture_event(
            4,
            0,
            EventPayload::XmodemCompleted {
                stage: TransferStage::Preloader,
                bytes_sent: 1024,
                expected_bytes: 1024,
                recovered_from_eot_quirk: false,
            },
        ));
        assert_eq!(renderer.bar.message(), "Waiting for stage 2 prompt");
    }

    #[test]
    fn transfer_message_reports_bytes_rate_and_elapsed() {
        let message = transfer_message(TransferStage::Fip, 2048, 4096, Duration::from_secs(2));

        assert!(message.contains("Uploading FIP"));
        assert!(message.contains("/s"));
        assert!(message.contains("/4.00 KiB"));
    }

    #[test]
    fn plain_renderer_emits_phase_and_progress_lines() {
        let mut renderer = PlainProgressRenderer::default();

        assert_eq!(
            renderer.render_event(&fixture_event(
                1,
                0,
                EventPayload::PromptSeen {
                    stage: RecoveryStage::PreloaderPrompt,
                    prompt: String::from("Press x"),
                },
            )),
            Some(String::from("Detected the preloader prompt; sending x."))
        );

        assert_eq!(
            renderer.render_event(&fixture_event(
                2,
                0,
                EventPayload::XmodemStarted {
                    stage: TransferStage::Preloader,
                    file_name: String::from(PRELOADER),
                    size_bytes: 1024,
                },
            )),
            Some(String::from("Uploading preloader (1.00 KiB total)."))
        );

        assert_eq!(
            renderer.render_event(&fixture_event(
                3,
                0,
                EventPayload::XmodemProgress {
                    stage: TransferStage::Preloader,
                    bytes_sent: 512,
                    total_bytes: 1024,
                },
            )),
            Some(String::from("Uploading preloader: 50% (512 B/1.00 KiB)"))
        );

        assert_eq!(
            renderer.render_event(&fixture_event(
                4,
                0,
                EventPayload::XmodemCompleted {
                    stage: TransferStage::Preloader,
                    bytes_sent: 1024,
                    expected_bytes: 1024,
                    recovered_from_eot_quirk: false,
                },
            )),
            Some(String::from("Finished uploading preloader."))
        );
    }

    #[test]
    fn timeout_hint_points_to_the_stalled_phase() {
        let events = vec![fixture_event(
            1,
            0,
            EventPayload::XmodemStarted {
                stage: TransferStage::Fip,
                file_name: String::from(FIP),
                size_bytes: 4096,
            },
        )];

        assert!(
            timeout_hint(&events)
                .unwrap()
                .contains("FIP path stalled before a live U-Boot prompt")
        );
    }

    #[test]
    fn console_handoff_maps_common_keys_to_serial_bytes() {
        assert_eq!(
            console_action_for_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ConsoleAction::Send(vec![b'\r'])
        );
        assert_eq!(
            console_action_for_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            ConsoleAction::Send(b"\x1b[A".to_vec())
        );
        assert_eq!(
            console_action_for_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE,)),
            ConsoleAction::Send(vec![b'x'])
        );
    }

    #[test]
    fn console_handoff_uses_ctrl_c_and_ctrl_d_to_exit() {
        assert_eq!(
            console_action_for_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,)),
            ConsoleAction::Exit
        );
        assert_eq!(
            console_action_for_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL,)),
            ConsoleAction::Exit
        );
    }

    #[test]
    fn recover_summary_reports_completed_console_handoff() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(true),
        );
        let mut stdout = Vec::new();

        write_recover_summary(&mut stdout, &plan, PORT, &RecoverOutcome::Recovered, true).unwrap();

        let rendered = String::from_utf8(stdout).unwrap();
        assert!(rendered.contains("Recovery finished on"));
        assert!(rendered.contains("Interactive console handoff ended"));
        assert!(!rendered.contains("progress mode"));
    }

    #[test]
    fn recover_execution_requires_an_explicit_port_until_auto_detection_exists() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = try_run(
            ["unbrk", "recover", "--resume-from-uboot"],
            tty_status(false),
            &mut stdout,
            &mut stderr,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("pass --port explicitly"));
    }

    #[test]
    fn auto_selection_picks_the_only_plausible_port() {
        let selected = select_recover_port(&[
            bluetooth_port("/dev/rfcomm0"),
            usb_port("/dev/ttyUSB0", 0x0403, 0x6001, "FTDI", "FT232R"),
        ])
        .unwrap();

        assert_eq!(selected, "/dev/ttyUSB0");
    }

    #[test]
    fn auto_selection_rejects_multiple_plausible_ports() {
        let error = select_recover_port(&[
            usb_port("/dev/ttyUSB0", 0x0403, 0x6001, "FTDI", "FT232R"),
            unknown_port("/dev/ttyACM0"),
        ])
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("/dev/ttyUSB0, /dev/ttyACM0"));
    }

    #[test]
    fn auto_selection_ignores_generic_ttys() {
        let selected = select_recover_port(&[
            unknown_port("/dev/ttyS4"),
            usb_port("/dev/ttyUSB0", 0x0403, 0x6001, "FTDI", "FT232R"),
        ])
        .unwrap();

        assert_eq!(selected, "/dev/ttyUSB0");
    }

    #[test]
    fn port_rendering_marks_bluetooth_as_ignored() {
        let port = bluetooth_port("/dev/rfcomm0");
        let rendered = render_port_line(&port);

        assert!(!is_plausible_recovery_port(&port));
        assert!(rendered.contains("[ignored]"));
        assert!(rendered.contains("Bluetooth serial"));
    }

    #[test]
    fn port_rendering_includes_usb_metadata() {
        let port = usb_port("/dev/ttyUSB0", 0x1a86, 0x7523, "QinHeng", "USB Serial");
        let rendered = render_port_line(&port);

        assert!(is_plausible_recovery_port(&port));
        assert!(rendered.contains("[plausible]"));
        assert!(rendered.contains("VID:1a86 PID:7523"));
        assert!(rendered.contains("manufacturer=QinHeng"));
        assert!(rendered.contains("product=USB Serial"));
    }

    #[test]
    fn linux_sysfs_paths_are_normalized_to_dev_nodes() {
        assert_eq!(normalize_port_name("/sys/class/tty/ttyS4"), "/dev/ttyS4");
        assert_eq!(
            normalize_port_name("/sys/class/tty/ttyUSB0"),
            "/dev/ttyUSB0"
        );
    }

    #[test]
    fn flash_plan_builder_applies_cli_overrides() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--flash-persistent",
                "--erase-block-count",
                "0x900",
                "--preloader-start-block",
                "0x20",
                "--preloader-block-count",
                "0x40",
                "--fip-start-block",
                "0x120",
                "--fip-block-count",
                "0x710",
            ],
            tty_status(false),
        );

        let flash_plan = build_flash_plan(&plan.args, &target_profile(&plan.args)).unwrap();

        assert_eq!(flash_plan.block_size, AN7581.flash.block_size);
        assert_eq!(
            flash_plan.erase_ranges[0].start_block,
            AN7581.flash.erase_range.start_block
        );
        assert_eq!(
            flash_plan.erase_ranges[0].block_count,
            BlockCount::new(0x900)
        );
        assert_eq!(
            flash_plan.write_stages[0].start_block,
            BlockOffset::new(0x20)
        );
        assert_eq!(
            flash_plan.write_stages[0].block_count,
            BlockCount::new(0x40)
        );
        assert_eq!(
            flash_plan.write_stages[1].start_block,
            BlockOffset::new(0x120)
        );
        assert_eq!(
            flash_plan.write_stages[1].block_count,
            BlockCount::new(0x710)
        );
    }

    #[test]
    fn flash_plan_builder_rejects_overflowing_block_ranges() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--flash-persistent",
                "--preloader-start-block",
                "0xffffffff",
                "--preloader-block-count",
                "2",
            ],
            tty_status(false),
        );

        let error = build_flash_plan(&plan.args, &target_profile(&plan.args)).unwrap_err();

        match error {
            RunError::Input(error) => {
                assert!(error.to_string().contains(
                    "the preloader write range exceeds the 32-bit MMC block address space"
                ));
            }
            other => panic!("expected an input error, got {other:?}"),
        }
    }

    #[test]
    fn flash_plan_builder_uses_target_erase_start_block() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--flash-persistent",
                "--erase-block-count",
                "0x900",
            ],
            tty_status(false),
        );
        let target = TargetProfile {
            flash: FlashLayout {
                erase_range: BlockRange::new(BlockOffset::new(0x40), BlockCount::new(0x800)),
                ..AN7581.flash
            },
            ..AN7581
        };

        let flash_plan = build_flash_plan(&plan.args, &target).unwrap();

        assert_eq!(
            flash_plan.erase_ranges[0].start_block,
            BlockOffset::new(0x40)
        );
        assert_eq!(
            flash_plan.erase_ranges[0].block_count,
            BlockCount::new(0x900)
        );
    }

    #[test]
    fn target_profile_applies_runtime_baud_and_prompt_override() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
                "--baud",
                "57600",
                "--uboot-prompt",
                "VALYRIAN>",
            ],
            tty_status(false),
        );

        let target = target_profile(&plan.args);

        assert_eq!(target.serial.baud_rate, 57_600);
        assert_eq!(target.prompts.uboot.as_str(), "VALYRIAN>");
        assert_eq!(
            target.prompts.initial_recovery,
            AN7581.prompts.initial_recovery
        );
        assert_eq!(target.prompts.second_stage, AN7581.prompts.second_stage);
    }

    #[test]
    fn wait_for_uboot_prompt_sends_carriage_return_before_matching() {
        let timeout = Duration::from_secs(3);
        let mut transport = MockTransport::new([
            MockStep::SetTimeout(timeout),
            MockStep::Write(b"\r".to_vec()),
            MockStep::Flush,
            MockStep::Read(b"boot chatter\r\nVALYRIAN> \r\n".to_vec()),
        ]);

        let prompt =
            wait_for_uboot_prompt(&mut transport, &PromptPattern::new(r"VALYRIAN>"), timeout)
                .unwrap();

        assert_eq!(prompt, "VALYRIAN>");
        transport.assert_finished();
    }

    #[test]
    fn append_events_preserves_core_timestamps() {
        let mut events = vec![fixture_event(
            1,
            100,
            EventPayload::PortOpened {
                port: String::from(PORT),
                baud: 115_200,
            },
        )];

        append_events(
            &mut events,
            vec![fixture_event(
                9,
                7_777,
                EventPayload::PromptSeen {
                    stage: RecoveryStage::PreloaderPrompt,
                    prompt: String::from("Press x"),
                },
            )],
        );

        assert_eq!(events[1].sequence, 2);
        assert_eq!(events[1].timestamp_unix_ms, 7_777);
    }

    #[test]
    fn doctor_command_stdout_errors_are_not_reported_as_serial_errors() {
        let mut stdout = BrokenWriter;
        let mut stderr = Vec::new();
        let error = try_run(
            ["unbrk", "doctor"],
            tty_status(true),
            &mut stdout,
            &mut stderr,
        )
        .unwrap_err();

        match error {
            RunError::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert!(
                    error
                        .to_string()
                        .contains("stdout I/O failed while writing doctor output")
                );
                assert!(!format!("{}", RunError::Io(error)).contains("serial error"));
            }
            other => panic!("expected an I/O run error, got {other:?}"),
        }
    }

    #[test]
    fn doctor_command_reports_passing_image_checks() {
        let preloader = temp_file_with_size(4);
        let fip = temp_file_with_size(4);

        let rendered = render(
            &[
                "unbrk",
                "doctor",
                "--preloader",
                preloader.path.to_str().unwrap(),
                "--fip",
                fip.path.to_str().unwrap(),
            ],
            tty_status(true),
        );

        assert!(rendered.contains("[INFO] os:"));
        assert!(rendered.contains("[SKIP] port:"));
        assert!(rendered.contains("[PASS] preloader:"));
        assert!(rendered.contains("[PASS] fip:"));
        assert!(rendered.contains("[PASS] summary: all requested checks passed"));
    }

    #[test]
    fn doctor_command_fails_for_oversized_preloader_image() {
        let preloader = temp_file_with_size(129_025);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = try_run(
            [
                "unbrk",
                "doctor",
                "--preloader",
                preloader.path.to_str().unwrap(),
            ],
            tty_status(true),
            &mut stdout,
            &mut stderr,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), CliExitCode::BadInput);
        assert!(error.to_string().contains("doctor found 1 failing check"));

        let rendered = String::from_utf8(stdout).unwrap();
        assert!(rendered.contains("[FAIL] preloader:"));
        assert!(rendered.contains("allocated flash window"));
        assert!(rendered.contains("[FAIL] summary: 1 requested check(s) failed"));
    }

    #[test]
    fn man_command_stdout_errors_are_not_reported_as_serial_errors() {
        let mut stdout = BrokenWriter;
        let mut stderr = Vec::new();
        let error =
            try_run(["unbrk", "man"], tty_status(true), &mut stdout, &mut stderr).unwrap_err();

        match error {
            RunError::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert!(
                    error
                        .to_string()
                        .contains("stdout I/O failed while rendering the manual page")
                );
                assert!(!format!("{}", RunError::Io(error)).contains("serial error"));
            }
            other => panic!("expected an I/O run error, got {other:?}"),
        }
    }

    #[test]
    fn resume_from_uboot_warning_stderr_errors_are_not_reported_as_serial_errors() {
        let mut stdout = Vec::new();
        let mut stderr = BrokenWriter;
        let error = try_run(
            ["unbrk", "recover", "--port", PORT, "--resume-from-uboot"],
            tty_status(false),
            &mut stdout,
            &mut stderr,
        )
        .unwrap_err();

        match error {
            RunError::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert!(
                    error
                        .to_string()
                        .contains("stderr I/O failed while writing the resume-from-uboot warning")
                );
                assert!(!format!("{}", RunError::Io(error)).contains("serial error"));
            }
            other => panic!("expected an I/O run error, got {other:?}"),
        }
    }

    #[test]
    fn json_write_errors_are_reported_as_io_failures() {
        let events = [fixture_event(
            1,
            100,
            EventPayload::PortOpened {
                port: String::from(PORT),
                baud: 115_200,
            },
        )];
        let mut writer = BrokenWriter;

        let error = write_events(&mut writer, &events).unwrap_err();

        assert!(matches!(error, RunError::Io(_)));
        assert!(
            error
                .to_string()
                .contains("output I/O failed while writing the JSON event stream")
        );
    }

    #[test]
    fn non_io_json_errors_map_to_protocol_failures() {
        let error = serde_json::from_str::<serde_json::Value>("not-json").unwrap_err();
        let mapped = map_json_event_error(&error);

        assert!(matches!(mapped, RunError::Protocol(_)));
        assert!(
            mapped
                .to_string()
                .contains("failed to serialize JSON event stream")
        );
    }

    #[test]
    fn flush_event_writer_reports_flush_failures() {
        let mut writer = FlushFailWriter;
        let error = flush_event_writer(&mut writer).unwrap_err();

        assert!(matches!(error, RunError::Io(_)));
        assert!(
            error
                .to_string()
                .contains("output I/O failed while flushing the event stream")
        );
    }

    #[test]
    fn write_events_and_flush_flushes_the_writer() {
        let events = [fixture_event(
            1,
            100,
            EventPayload::PortOpened {
                port: String::from(PORT),
                baud: 115_200,
            },
        )];
        let mut writer = CountingWriter::default();

        write_events_and_flush(&mut writer, &events).unwrap();

        assert_eq!(writer.flush_count, 1);
        assert!(
            String::from_utf8(writer.output)
                .unwrap()
                .contains("\"kind\":\"port_opened\"")
        );
    }

    #[test]
    fn write_event_trace_renders_pre_failure_progress() {
        let events = [
            fixture_event(
                1,
                100,
                EventPayload::PortOpened {
                    port: String::from(PORT),
                    baud: 115_200,
                },
            ),
            fixture_event(
                2,
                200,
                EventPayload::PromptSeen {
                    stage: RecoveryStage::PreloaderPrompt,
                    prompt: String::from("Press x"),
                },
            ),
        ];
        let mut output = Vec::new();

        write_event_trace(&mut output, &events).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("opened serial port"));
        assert!(rendered.contains("prompt seen"));
    }

    #[test]
    fn write_event_trace_errors_are_not_reported_as_serial_errors() {
        let events = [fixture_event(
            1,
            100,
            EventPayload::PortOpened {
                port: String::from(PORT),
                baud: 115_200,
            },
        )];
        let mut output = BrokenWriter;
        let error = write_event_trace(&mut output, &events).unwrap_err();

        match error {
            RunError::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert!(
                    error
                        .to_string()
                        .contains("output I/O failed while writing the event trace")
                );
                assert!(!format!("{}", RunError::Io(error)).contains("serial error"));
            }
            other => panic!("expected an I/O run error, got {other:?}"),
        }
    }

    #[test]
    fn write_recover_summary_stdout_errors_are_not_reported_as_serial_errors() {
        let plan = parse_recover(
            &[
                "unbrk",
                "recover",
                "--port",
                PORT,
                "--preloader",
                PRELOADER,
                "--fip",
                FIP,
            ],
            tty_status(false),
        );
        let mut stdout = BrokenWriter;
        let error =
            write_recover_summary(&mut stdout, &plan, PORT, &RecoverOutcome::Recovered, false)
                .unwrap_err();

        match error {
            RunError::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert!(
                    error
                        .to_string()
                        .contains("stdout I/O failed while writing the recovery summary header")
                );
                assert!(!format!("{}", RunError::Io(error)).contains("serial error"));
            }
            other => panic!("expected an I/O run error, got {other:?}"),
        }
    }

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken sink"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FlushFailWriter;

    impl Write for FlushFailWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("cannot flush"))
        }
    }

    #[derive(Default)]
    struct CountingWriter {
        output: Vec<u8>,
        flush_count: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_count += 1;
            Ok(())
        }
    }

    fn usb_port(
        port_name: &str,
        vid: u16,
        pid: u16,
        manufacturer: &str,
        product: &str,
    ) -> SerialPortInfo {
        SerialPortInfo {
            port_name: String::from(port_name),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid,
                serial_number: Some(String::from("SER123")),
                manufacturer: Some(String::from(manufacturer)),
                product: Some(String::from(product)),
            }),
        }
    }

    fn bluetooth_port(port_name: &str) -> SerialPortInfo {
        SerialPortInfo {
            port_name: String::from(port_name),
            port_type: SerialPortType::BluetoothPort,
        }
    }

    fn temp_file_with_size(size: u64) -> TempFile {
        let path = unique_temp_path();
        let file = File::create(&path).unwrap();
        file.set_len(size).unwrap();
        TempFile { path }
    }

    fn unique_temp_path() -> PathBuf {
        let unique_id = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "unbrk-cli-tests-{}-{unique_id}.bin",
            std::process::id()
        ))
    }

    struct TempFile {
        path: PathBuf,
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ignored = fs::remove_file(&self.path);
        }
    }

    fn unknown_port(port_name: &str) -> SerialPortInfo {
        SerialPortInfo {
            port_name: String::from(port_name),
            port_type: SerialPortType::Unknown,
        }
    }
}
