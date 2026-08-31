//! Worker-owned child supervision. Neither cancellation nor UI polling owns a process handle.

use std::{
    collections::VecDeque,
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const LINE_CAPACITY: usize = 8;
const MAX_LINE_BYTES: usize = 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Handles the exact child on cancellation, polling/pipe errors, spawn failure or unwind.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) struct Output {
    pub status: ExitStatus,
    pub stderr: Vec<u8>,
}

/// At most one child and two scoped pipe readers per running proxy job. Readers always drain
/// their pipes but retain bounded data; excess progress is disposable, never a growing backlog.
pub(crate) fn run(
    mut command: Command,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&str),
    started: impl FnOnce(),
) -> Result<Output, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawned = command
        .spawn()
        .map_err(|error| format!("could not start media tool: {error}"))?;
    thread::scope(move |scope| {
        let mut child = OwnedChild(spawned);
        let stdout = child.0.stdout.take().ok_or("missing media-tool stdout")?;
        let stderr = child.0.stderr.take().ok_or("missing media-tool stderr")?;
        let (line_tx, lines) = mpsc::sync_channel(LINE_CAPACITY);
        let mut stdout_join = Some(
            thread::Builder::new()
                .name("proxy-progress".into())
                .spawn_scoped(scope, move || read_lines(stdout, line_tx))
                .map_err(|error| format!("could not start progress reader: {error}"))?,
        );
        let stderr_join = thread::Builder::new()
            .name("proxy-errors".into())
            .spawn_scoped(scope, move || read_error_tail(stderr))
            .map_err(|error| format!("could not start diagnostic reader: {error}"))?;
        started();
        let status = (|| {
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err("cancelled".to_owned());
                }
                for line in lines.try_iter().take(LINE_CAPACITY) {
                    progress(&line);
                }
                if stdout_join.as_ref().is_some_and(|join| join.is_finished()) {
                    stdout_join
                        .take()
                        .unwrap()
                        .join()
                        .map_err(|_| "progress reader panicked".to_owned())??;
                }
                if let Some(status) = child
                    .0
                    .try_wait()
                    .map_err(|error| format!("could not poll media tool: {error}"))?
                {
                    return Ok(status);
                }
                thread::sleep(Duration::from_millis(10));
            }
        })();
        // Close the child before joining readers, including when it stopped producing progress.
        drop(child);
        if let Some(join) = stdout_join {
            join.join()
                .map_err(|_| "progress reader panicked".to_owned())??;
        }
        let stderr = stderr_join
            .join()
            .map_err(|_| "diagnostic reader panicked".to_owned())?;
        for line in lines.try_iter().take(LINE_CAPACITY) {
            progress(&line);
        }
        Ok(Output {
            status: status?,
            stderr,
        })
    })
}

fn read_lines(mut reader: impl Read, lines: mpsc::SyncSender<String>) -> Result<(), String> {
    let mut buffer = [0_u8; 4096];
    let mut line = Vec::with_capacity(MAX_LINE_BYTES);
    let mut oversized = false;
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("could not read media-tool progress: {error}")),
        };
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                if !oversized {
                    let _ = lines.try_send(String::from_utf8_lossy(&line).trim().to_owned());
                }
                line.clear();
                oversized = false;
            } else if line.len() < MAX_LINE_BYTES {
                line.push(byte);
            } else {
                oversized = true;
            }
        }
    }
    if !oversized && !line.is_empty() {
        let _ = lines.try_send(String::from_utf8_lossy(&line).trim().to_owned());
    }
    Ok(())
}

fn read_error_tail(mut reader: impl Read) -> Vec<u8> {
    let mut tail = VecDeque::with_capacity(MAX_ERROR_BYTES);
    let mut buffer = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let excess = (tail.len() + count).saturating_sub(MAX_ERROR_BYTES);
        tail.drain(..excess);
        tail.extend(&buffer[..count]);
    }
    tail.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_process_pipe_buffers_are_bounded_and_drain_excess() {
        let (tx, rx) = mpsc::sync_channel(LINE_CAPACITY);
        let mut input = vec![b'x'; MAX_LINE_BYTES + 100];
        input.extend_from_slice(b"\n");
        for _ in 0..10000 {
            input.extend_from_slice(b"out_time_us=1\n");
        }
        read_lines(input.as_slice(), tx).unwrap();
        let lines = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(lines.len(), LINE_CAPACITY);
        assert!(lines.iter().all(|line| line == "out_time_us=1"));
        let mut errors = vec![b'x'; MAX_ERROR_BYTES * 4];
        errors.extend_from_slice(b"last diagnostic");
        let tail = read_error_tail(errors.as_slice());
        assert_eq!(tail.len(), MAX_ERROR_BYTES);
        assert!(tail.ends_with(b"last diagnostic"));
    }
}
