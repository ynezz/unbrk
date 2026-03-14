#[cfg(unix)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn shell_quote(path: &Path) -> String {
        let path = path.display().to_string().replace('\'', r"'\''");
        format!("'{path}'")
    }

    #[test]
    fn fancy_progress_renders_banner_in_a_real_tty() {
        if Command::new("script").arg("--version").output().is_err() {
            return;
        }

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let preloader =
            manifest_dir.join("../../tests/fixtures/an7581/happy-path-stage1-prompt.bin");
        let fip = manifest_dir.join("../../tests/fixtures/an7581/happy-path-stage2-prompt.bin");
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_unbrk"));
        let command = format!(
            "env -u NO_COLOR {} recover --port /dev/ttyFAKE --preloader {} --fip {} --progress fancy",
            shell_quote(&binary),
            shell_quote(&preloader),
            shell_quote(&fip),
        );

        let output = Command::new("script")
            .arg("-qfec")
            .arg(&command)
            .arg("/dev/null")
            .output()
            .expect("spawn script for pseudo-terminal");

        assert!(
            !output.status.success(),
            "expected fake serial port to fail: {output:?}"
        );

        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            rendered.contains("happy-path-stage1-prompt.bin"),
            "expected fancy startup banner in PTY output:\n{rendered}"
        );
        assert!(
            rendered.contains("serial error:"),
            "expected the fake port failure in PTY output:\n{rendered}"
        );
    }
}
