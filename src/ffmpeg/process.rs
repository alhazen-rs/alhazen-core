//! One running `ffmpeg` decoding from stdin: a writer thread feeds Matroska bytes, a reader
//! thread turns stdout into outputs, and a third thread keeps the tail of stderr for errors.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, TrySendError};

use crate::{Error, Result};

/// How much of stderr is kept for error messages.
const STDERR_TAIL: usize = 2048;
/// Chunks of stdin bytes waiting for the writer thread.
const INPUT_QUEUE: usize = 32;
/// How long ffmpeg may stop reading its input (while producing nothing) before it counts as hung.
const INPUT_STALL: Duration = Duration::from_secs(5);

/// On Windows, run without flashing a console window.
pub(crate) fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

pub(crate) struct FfmpegProcess<T> {
    child: Child,
    input: Option<Sender<Vec<u8>>>,
    output: Option<Receiver<T>>,
    stderr: Arc<Mutex<String>>,
    threads: Vec<JoinHandle<()>>,
}

impl<T: Send + 'static> FfmpegProcess<T> {
    /// Starts `program args`, writes `header` to its stdin first, and runs `reader` on stdout.
    pub fn spawn(
        program: &std::path::Path,
        args: &[String],
        header: Vec<u8>,
        output_queue: usize,
        reader: impl FnOnce(ChildStdout, Sender<T>) + Send + 'static,
    ) -> Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        no_window(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| Error::Decode(format!("cannot start {}: {e}", program.display())))?;
        let (mut stdin, stdout, mut stderr) =
            (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let (in_tx, in_rx) = crossbeam_channel::bounded::<Vec<u8>>(INPUT_QUEUE);
        let (out_tx, out_rx) = crossbeam_channel::bounded(output_queue);
        let tail = Arc::new(Mutex::new(String::new()));
        let mut threads = Vec::new();
        threads.push(std::thread::Builder::new().name("ffmpeg-stdin".into()).spawn(move || {
            if stdin.write_all(&header).is_err() {
                return;
            }
            // Ends when the decoder drops its sender (EOF / flush) or ffmpeg stops reading.
            for chunk in in_rx {
                if stdin.write_all(&chunk).is_err() {
                    return;
                }
            }
        })?);
        threads.push(std::thread::Builder::new().name("ffmpeg-stdout".into()).spawn(move || reader(stdout, out_tx))?);
        let tail2 = tail.clone();
        threads.push(std::thread::Builder::new().name("ffmpeg-stderr".into()).spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut t = tail2.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&buf[..n]));
                if t.len() > STDERR_TAIL {
                    let cut = t.len() - STDERR_TAIL;
                    let cut = (cut..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(t.len());
                    t.drain(..cut);
                }
            }
        })?);
        Ok(Self { child, input: Some(in_tx), output: Some(out_rx), stderr: tail, threads })
    }

    /// Queues bytes for stdin. While the queue is full, outputs that are already decoded are
    /// moved into `ready`, so a caller that only drains outputs after writing cannot deadlock.
    pub fn write(&mut self, mut bytes: Vec<u8>, ready: &mut VecDeque<T>) -> Result<()> {
        let Some(input) = &self.input else {
            return Err(Error::Decode("ffmpeg input already closed".into()));
        };
        let mut deadline = std::time::Instant::now() + INPUT_STALL;
        loop {
            match input.try_send(bytes) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(self.failure("ffmpeg stopped reading its input")),
                Err(TrySendError::Full(b)) => bytes = b,
            }
            while let Some(out) = self.try_recv() {
                ready.push_back(out);
                // Still producing output: not hung, just busy.
                deadline = std::time::Instant::now() + INPUT_STALL;
            }
            if std::time::Instant::now() > deadline {
                return Err(self.failure("ffmpeg stalled: it stopped reading its input"));
            }
            match input.send_timeout(bytes, Duration::from_millis(10)) {
                Ok(()) => return Ok(()),
                Err(SendTimeoutError::Disconnected(_)) => return Err(self.failure("ffmpeg stopped reading its input")),
                Err(SendTimeoutError::Timeout(b)) => bytes = b,
            }
        }
    }

    /// Ends the input; ffmpeg then flushes its remaining outputs and exits.
    pub fn close_input(&mut self) {
        self.input = None;
    }

    pub fn try_recv(&self) -> Option<T> {
        self.output.as_ref()?.try_recv().ok()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> std::result::Result<T, RecvTimeoutError> {
        match &self.output {
            Some(rx) => rx.recv_timeout(timeout),
            None => Err(RecvTimeoutError::Disconnected),
        }
    }

    /// True once stdout has ended and every output was received.
    pub fn is_drained(&self) -> bool {
        // threads[1] is the stdout reader; once it has returned, nothing more will arrive.
        let reader_done = self.threads.get(1).is_none_or(|t| t.is_finished());
        reader_done && self.output.as_ref().is_none_or(|rx| rx.is_empty())
    }

    /// Whether ffmpeg exited unsuccessfully (waits briefly for the exit status).
    pub fn exit_failed(&mut self) -> bool {
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(status)) => return !status.success(),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return true,
            }
        }
        false
    }

    /// An error carrying ffmpeg's last stderr lines.
    pub fn failure(&self, what: &str) -> Error {
        let tail = self.stderr.lock().unwrap();
        let tail = tail.trim();
        Error::Decode(if tail.is_empty() { what.to_owned() } else { format!("{what}: {tail}") })
    }
}

impl<T> Drop for FfmpegProcess<T> {
    fn drop(&mut self) {
        self.input = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Unblocks a reader waiting for space in the output queue.
        self.output = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Reads exactly `buf.len()` bytes; `false` on a clean or partial end of stream.
pub(crate) fn read_full(r: &mut impl Read, buf: &mut [u8]) -> bool {
    r.read_exact(buf).is_ok()
}
