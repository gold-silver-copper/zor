use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::{
    io::{self, Read, Write},
    sync::mpsc,
    thread,
};

use crate::screen::Screen;

pub fn run(command: &str, argv: &[String]) -> Result<u8> {
    let system = NativePtySystem::default();
    let pair = system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open pty")?;
    let mut builder = CommandBuilder::new(command);
    builder.args(argv);
    builder.env("ZOR_PID", std::process::id().to_string());
    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("spawn wrapped command")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let mut writer = pair.master.take_writer().context("take pty writer")?;
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let Some(chunk) = buffer.get(..count) else {
                        break;
                    };
                    if output_tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut writer);
    });

    // The main loop is the sole output owner: child bytes are flushed before parsing.
    let mut screen = Screen::new(24, 80);
    let mut stdout = io::stdout().lock();
    for chunk in output_rx {
        stdout.write_all(&chunk).context("write child output")?;
        stdout.flush().context("flush child output")?;
        let _ground = screen.process(&chunk);
    }
    let status = child.wait().context("wait for wrapped command")?;
    let _ = reader_thread.join();
    Ok(u8::try_from(status.exit_code()).unwrap_or(u8::MAX))
}

pub fn run_transparent(command: &str, argv: &[String]) -> Result<u8> {
    let status = std::process::Command::new(command)
        .args(argv)
        .status()
        .context("run nested wrapped command")?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}
