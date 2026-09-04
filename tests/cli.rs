#![cfg(feature = "cli")]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

fn temp_dir(label: &str) -> Result<std::path::PathBuf, std::io::Error> {
    let path = std::env::temp_dir().join(format!("zor-{label}-{}", std::process::id()));
    fs::create_dir_all(&path)?;
    Ok(path)
}

#[test]
fn check_evaluates_fixture_expectation_and_rule() -> Result<(), Box<dyn std::error::Error>> {
    // Phase Z §8: check validates both expected state and matched rule id.
    let root = temp_dir("check")?;
    let rules = root.join("rules");
    fs::create_dir_all(&rules)?;
    fs::write(
        rules.join("agent.toml"),
        "id='agent'\nprompt_marker='>'\nblock_markers=[]\n[[rules]]\nid='working'\nstate='working'\nregion='whole'\ncontains=['busy']\n",
    )?;
    let fixture = root.join("fixture.txt");
    fs::write(
        &fixture,
        "# agent: agent\n# title: test\n# progress: 3:0\n# expect: working\n# matched: working\nbusy\n",
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_zor"))
        .args([
            "--rules",
            rules.to_string_lossy().as_ref(),
            "check",
            fixture.to_string_lossy().as_ref(),
        ])
        .output()?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"working working\n");
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn sigusr1_writes_the_detection_window_fixture() -> Result<(), Box<dyn std::error::Error>> {
    // Phase Z §7: SIGUSR1 writes the exact observed window to TMPDIR.
    let root = temp_dir("signal")?;
    let child = Command::new(env!("CARGO_BIN_EXE_zor"))
        .env("TMPDIR", &root)
        .args([
            "--title",
            "never",
            "--",
            "/bin/sh",
            "-c",
            "printf observed; sleep 0.3",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    thread::sleep(Duration::from_millis(100));
    let status = Command::new("/bin/kill")
        .args(["-USR1", &child.id().to_string()])
        .status()?;
    assert!(status.success());
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    let path = stderr
        .lines()
        .find_map(|line| line.strip_prefix("zor: fixture written to "))
        .map(std::path::PathBuf::from);
    assert!(path.as_ref().is_some_and(|path| path.exists()));
    assert_eq!(
        path.as_ref()
            .map(fs::metadata)
            .transpose()?
            .map(|metadata| metadata.permissions().mode() & 0o777),
        Some(0o600)
    );
    let contents = path
        .map(fs::read_to_string)
        .transpose()?
        .unwrap_or_default();
    assert!(contents.contains("observed"));
    fs::remove_dir_all(root)?;
    Ok(())
}
