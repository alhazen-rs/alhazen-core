//! Windows: ffmpeg runs inside a kill-on-close Job Object, so stopping it also stops processes it
//! started — package-manager shims (Chocolatey, Scoop) launch the real `ffmpeg.exe` as a child,
//! and killing only the shim would leave the decoder running.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// A kill-on-close job; dropping it ends every process in it.
pub struct Job(HANDLE);

// SAFETY: a job handle is a kernel handle, usable from any thread.
unsafe impl Send for Job {}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once; closing ends the job's processes.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl Job {
    fn new() -> windows::core::Result<Job> {
        // SAFETY: FFI with valid arguments; the info struct outlives the call.
        unsafe {
            let job = Job(CreateJobObjectW(None, windows::core::PCWSTR::null())?);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(job)
        }
    }
}

/// Spawns `cmd` (without a console window) inside a new kill-on-close job: started suspended,
/// assigned to the job, then resumed, so even processes it starts at once belong to the job.
/// Falls back to a plain spawn (and no job) if job setup fails.
pub fn spawn(cmd: &mut Command) -> std::io::Result<(Child, Option<Job>)> {
    let Ok(job) = Job::new() else {
        cmd.creation_flags(CREATE_NO_WINDOW);
        return cmd.spawn().map(|c| (c, None));
    };
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = cmd.spawn()?;
    // SAFETY: the child's process handle is valid while `child` lives.
    let assigned = unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) }.is_ok();
    if !resume(child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("could not resume the suspended ffmpeg"));
    }
    Ok((child, assigned.then_some(job)))
}

/// Resumes every thread of process `pid` (a freshly created process has exactly one).
fn resume(pid: u32) -> bool {
    // SAFETY: the snapshot and thread handles are opened and closed here; the entry struct is
    // initialised with its size as the API requires.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return false };
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut resumed = false;
        let mut more = Thread32First(snap, &mut entry).is_ok();
        while more {
            if entry.th32OwnerProcessID == pid
                && let Ok(t) = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
            {
                resumed |= ResumeThread(t) != u32::MAX;
                let _ = CloseHandle(t);
            }
            more = Thread32Next(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        resumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Whether a process with `pid` is running (`tasklist` lists it).
    fn running(pid: u32) -> bool {
        let out = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }

    #[test]
    fn dropping_the_job_ends_grandchildren_too() {
        // `cmd` starts a long `ping` as its child, like a shim starting the real ffmpeg.
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping -n 60 127.0.0.1 > NUL"]);
        let (mut child, job) = spawn(&mut cmd).unwrap();
        assert!(job.is_some(), "job created");
        std::thread::sleep(Duration::from_millis(500));
        // Find the ping child of our cmd.
        let query = format!("(Get-CimInstance Win32_Process -Filter 'ParentProcessId={}').ProcessId", child.id());
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &query]).output();
        let grandchild: Option<u32> =
            out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).lines().find_map(|l| l.trim().parse().ok()));
        let _ = child.kill();
        drop(job);
        let _ = child.wait();
        let Some(pid) = grandchild else {
            eprintln!("skipped: could not list the child process");
            return;
        };
        let start = Instant::now();
        while running(pid) {
            assert!(start.elapsed() < Duration::from_secs(5), "grandchild {pid} survived the job");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
