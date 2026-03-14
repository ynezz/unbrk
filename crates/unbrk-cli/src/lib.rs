use clap::{
    ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    parser::ValueSource,
};
use clap_complete::Shell;
use clap_complete::generate;
use clap_mangen::Man;
use is_terminal::IsTerminal;
use regex::Regex;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use unbrk_core::error::{ConsoleTail, UnbrkError};
use unbrk_core::event::{
    EVENT_SCHEMA_VERSION, Event, EventPayload, FailureClass, ImageKind, RecoveryStage,
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
  1  serial error
  2  timeout
  3  protocol error
  4  XMODEM failure
  5  U-Boot command failure
  6  verification mismatch
  7  bad input
  8  user abort";

#[must_use]
pub fn run() -> ExitCode {
    let terminal_status = TerminalStatus::detect();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();

    match try_run(
        std::env::args_os(),
        terminal_status,
        &mut stdout,
        &mut stderr,
    ) {
        Ok(()) => CliExitCode::Success.into(),
        Err(error) => {
            let _ignored = writeln!(stderr, "{error}");
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
        Commands::Doctor => Ok(CommandPlan::Doctor),
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
        no_console,
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
        CommandPlan::Ports => {
            writeln!(
                stdout,
                "ports command scaffold: port enumeration is not implemented yet."
            )
            .map_err(RunError::Serial)?;
            Ok(())
        }
        CommandPlan::Completions { shell } => {
            let mut command = cli_command();
            generate(shell, &mut command, "unbrk", stdout);
            Ok(())
        }
        CommandPlan::Man => {
            Man::new(cli_command())
                .render(stdout)
                .map_err(RunError::Serial)?;
            Ok(())
        }
        CommandPlan::Doctor => {
            writeln!(
                stdout,
                "doctor command scaffold: diagnostics are not implemented yet."
            )
            .map_err(RunError::Serial)?;
            Ok(())
        }
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
        .map_err(RunError::Serial)?;
    }

    let port = recover_port(&plan.args)?;
    let target = target_profile(&plan.args);
    let mut events = Vec::new();
    push_event(
        &mut events,
        EventPayload::SessionStarted {
            schema_version: EVENT_SCHEMA_VERSION,
            tool_version: String::from(env!("CARGO_PKG_VERSION")),
            target_profile: String::from(target.name),
            serial_port: Some(port.clone()),
        },
    );

    let mut transport = open_transport(port.as_str(), &plan.args)?;
    push_event(
        &mut events,
        EventPayload::PortOpened {
            port: port.clone(),
            baud: plan.args.baud,
        },
    );

    let execution = execute_recover(plan, target, &mut transport, &mut events);
    if let Err(error) = &execution {
        push_event(
            &mut events,
            EventPayload::Failure {
                class: error.failure_class(),
                message: error.to_string(),
            },
        );
        if !plan.args.json {
            write_event_trace(stderr, &events[..events.len().saturating_sub(1)])?;
        }
    }

    if let Some(path) = plan.args.log_file.as_deref() {
        write_events_to_path(path, &events)?;
    }

    if plan.args.json {
        write_events(stdout, &events)?;
    } else if let Ok(outcome) = &execution {
        write_recover_summary(stdout, plan, port.as_str(), outcome)?;
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

fn recover_port(args: &RecoverArgs) -> Result<String, RunError> {
    args.port.clone().ok_or_else(|| {
        RunError::Input(InputError::new(
            "automatic port selection is not implemented yet; pass --port explicitly",
        ))
    })
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
            let flash_plan = build_flash_plan(&plan.args, target)?;
            let flash_report = flash_from_uboot(transport, target, &flash_plan, flash_config)
                .map_err(RunError::from)?;
            append_events(events, flash_report.events);
            return Ok(RecoverOutcome::FlashedFromExistingPrompt {
                reset_evidence: flash_report.reset_evidence,
            });
        }

        let prompt = wait_for_uboot_prompt(
            transport,
            target.prompts.uboot,
            duration_override(plan.args.command_timeout, DEFAULT_COMMAND_TIMEOUT),
        )
        .map_err(RunError::from)?;
        push_event(events, EventPayload::UBootPromptSeen { prompt });
        push_event(
            events,
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
    let recovery_report = recover_to_uboot(
        transport,
        target,
        RecoveryImages {
            preloader_name: file_name(preloader_path),
            preloader: &preloader,
            fip_name: file_name(fip_path),
            fip: &fip,
        },
        recovery_config,
    )
    .map_err(RunError::from)?;
    append_events(events, recovery_report.events);

    if plan.args.flash_persistent {
        let flash_plan = build_flash_plan(&plan.args, target)?;
        let flash_report = flash_from_uboot(transport, target, &flash_plan, flash_config)
            .map_err(RunError::from)?;
        append_events(events, flash_report.events);
        Ok(RecoverOutcome::FlashedAfterRecovery {
            reset_evidence: flash_report.reset_evidence,
        })
    } else {
        push_event(
            events,
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
    let prompt_source = args
        .uboot_prompt
        .as_deref()
        .map_or_else(|| AN7581.prompts.uboot.as_str(), leak_string);

    TargetProfile {
        serial: unbrk_core::target::SerialSettings {
            baud_rate: args.baud,
            ..AN7581.serial
        },
        prompts: PromptPatterns {
            uboot: PromptPattern::new(prompt_source),
            ..AN7581.prompts
        },
        ..AN7581
    }
}

fn leak_string(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
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

fn build_flash_plan(args: &RecoverArgs, target: TargetProfile) -> Result<FlashPlan, RunError> {
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
    pattern: PromptPattern,
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
        writeln!(writer).map_err(RunError::Serial)?;
    }

    Ok(())
}

fn map_json_event_error(error: &serde_json::Error) -> RunError {
    if error.is_io() {
        RunError::Serial(io::Error::other(format!(
            "failed to write JSON event stream: {error}",
        )))
    } else {
        RunError::Protocol(format!("failed to serialize JSON event stream: {error}"))
    }
}

fn flush_event_writer(writer: &mut dyn Write) -> Result<(), RunError> {
    writer.flush().map_err(|error| {
        RunError::Serial(io::Error::new(
            error.kind(),
            format!("failed to flush event stream: {error}"),
        ))
    })
}

fn write_events_to_path(path: &Path, events: &[Event]) -> Result<(), RunError> {
    let file = File::create(path).map_err(|error| {
        RunError::Input(InputError::new(format!(
            "failed to create log file {}: {error}",
            path.display()
        )))
    })?;
    let mut writer = BufWriter::new(file);
    write_events(&mut writer, events)?;
    flush_event_writer(&mut writer)
}

fn write_event_trace(writer: &mut dyn Write, events: &[Event]) -> Result<(), RunError> {
    for event in events {
        writeln!(writer, "{event}").map_err(RunError::Serial)?;
    }
    Ok(())
}

fn write_recover_summary(
    stdout: &mut dyn Write,
    plan: &RecoverPlan,
    port: &str,
    outcome: &RecoverOutcome,
) -> Result<(), RunError> {
    writeln!(
        stdout,
        "recovering on {port} | progress mode: {} | no-console: {} | stdout tty: {}",
        plan.progress_mode.as_str(),
        plan.no_console,
        plan.terminal_status.stdout_is_tty,
    )
    .map_err(RunError::Serial)?;

    match outcome {
        RecoverOutcome::Recovered => {
            writeln!(stdout, "stopped at the RAM-resident U-Boot prompt.")
                .map_err(RunError::Serial)?;
            if plan.console_handoff_allowed {
                writeln!(
                    stdout,
                    "interactive console handoff is not implemented yet; staying at the stop point."
                )
                .map_err(RunError::Serial)?;
            }
        }
        RecoverOutcome::FlashedAfterRecovery { reset_evidence } => {
            writeln!(
                stdout,
                "completed recovery and persistent flash; observed reset evidence: {reset_evidence}"
            )
            .map_err(RunError::Serial)?;
        }
        RecoverOutcome::FlashedFromExistingPrompt { reset_evidence } => {
            writeln!(
                stdout,
                "resumed from an existing U-Boot prompt and completed the persistent flash; observed reset evidence: {reset_evidence}"
            )
            .map_err(RunError::Serial)?;
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
    Doctor,
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

impl ResolvedProgressMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Fancy => "fancy",
            Self::Off => "off",
        }
    }
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
    Doctor,
}

#[derive(Debug)]
struct RecoverPlan {
    args: RecoverArgs,
    progress_mode: ResolvedProgressMode,
    no_console: bool,
    console_handoff_allowed: bool,
    terminal_status: TerminalStatus,
}

#[derive(Debug)]
pub enum RunError {
    Input(InputError),
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
            Self::Serial(_) => CliExitCode::SerialError,
            Self::Timeout(_) => CliExitCode::Timeout,
            Self::Protocol(_) => CliExitCode::ProtocolError,
            Self::Xmodem(_) => CliExitCode::XmodemFailure,
            Self::UBootCommand(_) => CliExitCode::UBootCommandFailure,
            Self::VerificationMismatch(_) => CliExitCode::VerificationMismatch,
            Self::UserAbort(_) => CliExitCode::UserAbort,
        }
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
            UnbrkError::Serial { source, .. } => Self::Serial(source),
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
    SerialError = 1,
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
        CliExitCode, CommandPlan, ProgressMode, RecoverPlan, ResolvedProgressMode, RunError,
        TerminalStatus, append_events, build_flash_plan, flush_event_writer, map_json_event_error,
        parse_command, target_profile, try_run, wait_for_uboot_prompt, write_event_trace,
        write_events, xmodem_config,
    };
    use std::io::{self, Write};
    use std::time::Duration;
    use unbrk_core::event::{Event, EventPayload, RecoveryStage};
    use unbrk_core::target::{
        AN7581, BlockCount, BlockOffset, BlockRange, FlashLayout, PromptPattern, TargetProfile,
    };
    use unbrk_core::xmodem::{XMODEM_DEFAULT_BLOCK_RETRY_LIMIT, XMODEM_DEFAULT_EOT_RETRY_LIMIT};
    use unbrk_core::{MockStep, MockTransport};

    const PORT: &str = "/dev/ttyUSB0";
    const PRELOADER: &str = "preloader.bin";
    const FIP: &str = "image.fip";

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
        assert!(!plan.no_console);
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
        assert!(plan.no_console);
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
            RunError::Serial(std::io::Error::other("boom")).exit_code(),
            CliExitCode::SerialError
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

        let flash_plan = build_flash_plan(&plan.args, target_profile(&plan.args)).unwrap();

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

        let error = build_flash_plan(&plan.args, target_profile(&plan.args)).unwrap_err();

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

        let flash_plan = build_flash_plan(&plan.args, target).unwrap();

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
            wait_for_uboot_prompt(&mut transport, PromptPattern::new(r"VALYRIAN>"), timeout)
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
    fn json_write_errors_stay_serial_failures() {
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

        assert!(matches!(error, RunError::Serial(_)));
        assert!(
            error
                .to_string()
                .contains("failed to write JSON event stream")
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

        assert!(matches!(error, RunError::Serial(_)));
        assert!(error.to_string().contains("failed to flush event stream"));
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
}
