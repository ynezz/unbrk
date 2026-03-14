#[cfg(unix)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ScriptFlavor {
        UtilLinux,
        Bsd,
    }

    fn shell_quote(path: &Path) -> String {
        let path = path.display().to_string().replace('\'', r"'\''");
        format!("'{path}'")
    }

    fn detect_script_flavor() -> Option<ScriptFlavor> {
        let output = Command::new("script").arg("--version").output().ok()?;
        if output.status.success() {
            Some(ScriptFlavor::UtilLinux)
        } else {
            Some(ScriptFlavor::Bsd)
        }
    }

    fn transcript_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "unbrk-fancy-progress-tty-{}-{nonce}.log",
            std::process::id()
        ))
    }

    fn run_in_script(
        flavor: ScriptFlavor,
        command: &str,
        transcript_path: &Path,
    ) -> std::io::Result<Output> {
        match flavor {
            ScriptFlavor::UtilLinux => Command::new("script")
                .arg("-qec")
                .arg(command)
                .arg(transcript_path)
                .output(),
            ScriptFlavor::Bsd => Command::new("script")
                .arg("-eq")
                .arg(transcript_path)
                .arg("sh")
                .arg("-c")
                .arg(command)
                .output(),
        }
    }

    #[test]
    fn fancy_progress_renders_banner_in_a_real_tty() {
        let Some(script_flavor) = detect_script_flavor() else {
            return;
        };

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let preloader =
            manifest_dir.join("../../tests/fixtures/an7581/happy-path-stage1-prompt.bin");
        let fip = manifest_dir.join("../../tests/fixtures/an7581/happy-path-stage2-prompt.bin");
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_unbrk"));
        let transcript_path = transcript_path();
        let command = format!(
            "env -u NO_COLOR TERM=xterm-256color {} recover --port /dev/ttyFAKE --preloader {} --fip {} --progress fancy",
            shell_quote(&binary),
            shell_quote(&preloader),
            shell_quote(&fip),
        );

        let output = run_in_script(script_flavor, &command, &transcript_path)
            .expect("spawn script for pseudo-terminal");

        assert!(
            !output.status.success(),
            "expected fake serial port to fail: {output:?}"
        );

        let transcript = fs::read_to_string(&transcript_path).unwrap_or_default();
        let rendered = format!(
            "{transcript}{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let _ignored = fs::remove_file(&transcript_path);
        assert!(
            rendered.contains("▐███    ███▌ ▐██▖    ██▌"),
            "expected the ANSI logo in PTY output:\n{rendered}"
        );
        assert!(
            rendered.contains("happy-path-stage1-prompt.bin"),
            "expected fancy startup banner in PTY output:\n{rendered}"
        );
        assert!(
            rendered.contains("Recovery"),
            "expected banner metadata in PTY output:\n{rendered}"
        );
        assert!(
            rendered.contains("serial error:"),
            "expected the fake port failure in PTY output:\n{rendered}"
        );
    }
}
