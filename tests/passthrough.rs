use std::{
    io::{Read, Write},
    process::{Command, Stdio},
};

#[test]
fn child_control_bytes_are_forwarded_exactly() -> Result<(), Box<dyn std::error::Error>> {
    // Phase Z §5: CSI, OSC, DCS and queries reach stdout without rewriting.
    let expected = "\x1b[31m\x1b]2;title\x07\x1b]9;4;3;0\x1b\\\x1bPdata\x1b\\\x1b[c";
    let output = Command::new(env!("CARGO_BIN_EXE_zor"))
        .args([
            "--title",
            "never",
            "--",
            "/bin/sh",
            "-c",
            "printf %s \"$1\"",
            "sh",
            expected,
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, expected.as_bytes());
    Ok(())
}

#[test]
fn terminal_response_reaches_child_stdin() -> Result<(), Box<dyn std::error::Error>> {
    // Phase Z §5: DA responses from the outer terminal pass to the child unchanged.
    let mut child = Command::new(env!("CARGO_BIN_EXE_zor"))
        .args([
            "--title",
            "never",
            "--",
            "/bin/sh",
            "-c",
            "stty -echo; printf '\\x1b[c'; IFS= read -r reply; printf %s \"$reply\"",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut query = [0_u8; 3];
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.as_mut() {
        stdout.read_exact(&mut query)?;
    }
    if let Some(mut input) = child.stdin.take() {
        input.write_all(b"\x1b[?1;2c\n")?;
    }
    if let Some(stdout) = child.stdout.as_mut() {
        stdout.read_to_end(&mut output)?;
    }
    let status = child.wait()?;
    assert!(status.success());
    assert_eq!(query, *b"\x1b[c");
    assert_eq!(output, b"\x1b[?1;2c");
    Ok(())
}

#[test]
fn split_control_string_is_never_interleaved() -> Result<(), Box<dyn std::error::Error>> {
    // Phase Z §5: a control string split across writes remains byte-identical.
    let output = Command::new(env!("CARGO_BIN_EXE_zor"))
        .args([
            "--title",
            "never",
            "--",
            "/bin/sh",
            "-c",
            "printf '\\x1bPfirst'; sleep 0.05; printf 'second\\x1b\\\\'",
        ])
        .output()?;
    assert_eq!(output.stdout, b"\x1bPfirstsecond\x1b\\");
    Ok(())
}

#[test]
fn child_exit_status_is_propagated() -> Result<(), Box<dyn std::error::Error>> {
    // Phase Z §5: ordinary and signal-style child statuses become zor's status.
    for code in [0, 1, 7] {
        let output = Command::new(env!("CARGO_BIN_EXE_zor"))
            .args(["--", "/bin/sh", "-c", &format!("exit {code}")])
            .output()?;
        assert_eq!(output.status.code(), Some(code));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_zor"))
        .args(["--", "/bin/sh", "-c", "kill -TERM $$"])
        .output()?;
    assert_eq!(output.status.code(), Some(143));
    Ok(())
}

#[test]
fn nested_wrapper_uses_transparent_execution() -> Result<(), Box<dyn std::error::Error>> {
    // Phase Z §5: ZOR_PID avoids a second PTY/emulator layer.
    let output = Command::new(env!("CARGO_BIN_EXE_zor"))
        .env("ZOR_PID", "1")
        .args(["--", "/bin/sh", "-c", "printf nested; exit 9"])
        .output()?;
    assert_eq!(output.stdout, b"nested");
    assert_eq!(output.status.code(), Some(9));
    Ok(())
}
