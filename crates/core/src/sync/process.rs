//! Running a child process with a timeout and an output cap, without a shell.

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

/// How much of stderr is kept for error messages.
pub const STDERR_TAIL: usize = 4096;

pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr_tail: String,
    pub status: ExitStatus,
}

/// Runs `cmd` to completion. Fails when it outlives `timeout` or writes more
/// than `max_bytes` to stdout; the child is killed in both cases.
pub fn run_captured(mut cmd: Command, stdin: Option<Vec<u8>>, timeout: Duration, max_bytes: u64) -> Result<Captured> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().with_context(|| format!("starting {program}"))?;

    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin is piped");
        // Small inputs (a curl config); write before reading so the child can start.
        thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let (tx, rx) = mpsc::channel();
    let out_thread = thread::spawn(move || {
        let r = read_capped(stdout, max_bytes);
        let _ = tx.send(matches!(r, Ok(None)));
        r
    });
    let err_thread = thread::spawn(move || read_tail(stderr, STDERR_TAIL));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        // Stdout hitting the cap ends the reader early.
        if rx.try_recv() == Ok(true) {
            kill(&mut child);
            bail!("{program} wrote more than {max_bytes} bytes; raise max_bytes in the recipe if that is expected");
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            bail!("{program} did not finish within {} s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = out_thread.join().map_err(|_| anyhow!("stdout reader panicked"))??;
    let stderr_tail = err_thread.join().unwrap_or_default();
    match stdout {
        Some(stdout) => Ok(Captured { stdout, stderr_tail, status }),
        None => bail!("{program} wrote more than {max_bytes} bytes; raise max_bytes in the recipe if that is expected"),
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads to EOF; `None` when there was more than `max` bytes.
pub fn read_capped(mut r: impl Read, max: u64) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let n = (&mut r).take(max + 1).read_to_end(&mut buf)?;
    if n as u64 > max {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// The last `keep` bytes of a stream, lossily decoded.
pub fn read_tail(mut r: impl Read, keep: usize) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(n) = r.read(&mut chunk) {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > keep {
            tail.drain(..tail.len() - keep);
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// Text from outside, made safe to keep in a message: control characters
/// removed and length capped.
pub fn clean_text(s: &str, max_chars: usize) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .take(max_chars)
        .collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out.trim().to_string()
}
