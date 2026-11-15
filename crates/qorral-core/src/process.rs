//! Process helpers: run a tool with a hard timeout, check whether a recorded
//! PID is still our QEMU, and end only that PID.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Runs `cmd` to completion or kills it at `timeout`. Every tool launch goes
/// through here: nothing Qorral starts may hang a worker forever.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    no_console(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {:?}: {e}", cmd.get_program()))?;
    let end = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|e| e.to_string()),
            Ok(None) if Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{:?} did not finish in {} s", cmd.get_program(), timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Windows: no console window flashes up for helper tools.
pub fn no_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// The executable file name of a live process, or `None` if it has exited.
#[cfg(windows)]
pub fn image_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    // SAFETY: plain Win32 calls on a handle we open and close here; the
    // buffer outlives the call and its length is passed in.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        if !alive || !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned())
    }
}

#[cfg(unix)]
pub fn image_name(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    match exe {
        Some(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
        None => {
            // macOS has no /proc: ask ps (bounded).
            let mut c = Command::new("ps");
            c.args(["-p", &pid.to_string(), "-o", "comm="]);
            let out = run_with_timeout(c, Duration::from_secs(5)).ok()?;
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() { None } else { Path::new(&s).file_name().map(|n| n.to_string_lossy().into_owned()) }
        }
    }
}

/// True only while `pid` is alive *and* is a QEMU system emulator, so a
/// recycled PID never counts as our VM.
pub fn is_our_qemu(pid: u32) -> bool {
    pid != 0 && image_name(pid).is_some_and(|n| n.to_ascii_lowercase().starts_with("qemu-system"))
}

/// Waits up to `timeout` until `pid` has fully exited. On Windows the exit
/// code is set before the process has released its memory and files (a
/// large VM takes a moment), so "not alive" is not yet "disk unlocked".
/// True when the process is gone.
pub fn wait_gone(pid: u32, timeout: Duration) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        // SAFETY: handle opened and closed here; waiting has no side effects.
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return true; // no such process any more
            }
            let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
            let done = WaitForSingleObject(h, ms) == WAIT_OBJECT_0;
            CloseHandle(h);
            done
        }
    }
    #[cfg(unix)]
    {
        let end = std::time::Instant::now() + timeout;
        // Not /proc/<pid>: an unreaped child keeps it; its exe link is gone.
        while image_name(pid).is_some() {
            if std::time::Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

/// Ends `pid`, but only after checking it is still a QEMU system emulator.
pub fn kill_qemu(pid: u32) -> bool {
    if !is_our_qemu(pid) {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        // SAFETY: handle opened and closed here; TerminateProcess on our own child.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return false;
            }
            let ok = TerminateProcess(h, 1) != 0;
            CloseHandle(h);
            ok
        }
    }
    #[cfg(unix)]
    {
        let mut c = Command::new("kill");
        c.args(["-9", &pid.to_string()]);
        run_with_timeout(c, Duration::from_secs(5)).is_ok_and(|o| o.status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_gone_waits_for_the_process_object() {
        assert!(!wait_gone(std::process::id(), Duration::from_millis(50)), "a live process is not gone");
        let mut c = if cfg!(windows) { Command::new("cmd") } else { Command::new("true") };
        if cfg!(windows) {
            c.args(["/c", "exit", "0"]);
        }
        let mut child = c.spawn().expect("spawn a short-lived child");
        let pid = child.id();
        child.wait().expect("child exits");
        drop(child); // releases our handle, so the process object can go
        assert!(wait_gone(pid, Duration::from_secs(5)));
    }

    #[test]
    fn our_own_process_is_not_qemu_and_a_dead_pid_is_not_alive() {
        let me = std::process::id();
        assert!(image_name(me).is_some(), "a live process has an image name");
        assert!(!is_our_qemu(me), "the test runner must never count as a VM");
        assert!(!kill_qemu(me), "kill refuses anything that is not QEMU");
        assert!(!is_our_qemu(0));
    }

    #[test]
    fn timeout_kills_a_hanging_tool() {
        let mut c = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        c.env("QORRAL_TEST", "1");
        let t = Instant::now();
        let r = run_with_timeout(c, Duration::from_millis(1500));
        assert!(r.is_err());
        assert!(t.elapsed() < Duration::from_secs(10));
    }
}
