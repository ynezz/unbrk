use clap::{
    ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    parser::ValueSource,
};
use clap_complete::Shell;
use is_terminal::IsTerminal;
use regex::Regex;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

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
    let matches = Cli::command()
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
            writeln!(
                stdout,
                "completions command scaffold: generation is not implemented yet for {shell}.",
            )
            .map_err(RunError::Serial)?;
            Ok(())
        }
        CommandPlan::Man => {
            writeln!(
                stdout,
                "man command scaffold: generation is not implemented yet."
            )
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

    if plan.args.json {
        writeln!(
            stdout,
            concat!(
                "{{",
                "\"schema_version\":1,",
                "\"event\":\"cli_scaffold\",",
                "\"command\":\"recover\",",
                "\"progress_mode\":\"{}\",",
                "\"no_console\":{},",
                "\"console_handoff_allowed\":{}",
                "}}"
            ),
            plan.progress_mode.as_str(),
            plan.no_console,
            plan.console_handoff_allowed,
        )
        .map_err(RunError::Serial)?;
    } else {
        writeln!(
            stdout,
            "recover command scaffold: orchestration is not implemented yet.",
        )
        .map_err(RunError::Serial)?;
        writeln!(
            stdout,
            "resolved progress mode: {} | console handoff: {} | stdout tty: {}",
            plan.progress_mode.as_str(),
            if plan.console_handoff_allowed {
                "enabled"
            } else {
                "disabled"
            },
            plan.terminal_status.stdout_is_tty,
        )
        .map_err(RunError::Serial)?;
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
    xmodem_retry: Option<u32>,
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
    stderr_is_tty: bool,
}

impl TerminalStatus {
    fn detect() -> Self {
        Self {
            stdin_is_tty: io::stdin().is_terminal(),
            stdout_is_tty: io::stdout().is_terminal(),
            stderr_is_tty: io::stderr().is_terminal(),
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
        TerminalStatus, parse_command,
    };

    const PORT: &str = "/dev/ttyUSB0";
    const PRELOADER: &str = "preloader.bin";
    const FIP: &str = "image.fip";

    fn tty_status(stdout_is_tty: bool) -> TerminalStatus {
        TerminalStatus {
            stdin_is_tty: stdout_is_tty,
            stdout_is_tty,
            stderr_is_tty: stdout_is_tty,
        }
    }

    fn parse_recover(args: &[&str], terminal_status: TerminalStatus) -> RecoverPlan {
        match parse_command(args, terminal_status).unwrap() {
            CommandPlan::Recover(plan) => *plan,
            command => panic!("expected recover command, got {command:?}"),
        }
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
}
