//! OS-specific helpers. macOS is the target; the Linux branches exist so the agent builds and tests in CI.
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
#[cfg(unix)]
use std::process::Child;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(unix)]
/// Peer's user id. Connections from other users are always refused.
pub fn peer_uid(s: &UnixStream) -> Option<u32> {
    #[cfg(target_os = "macos")]
    unsafe {
        let (mut uid, mut gid) = (0u32, 0u32);
        (libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) == 0).then_some(uid)
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let r = libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut _, &mut len);
        (r == 0).then_some(cred.uid)
    }
}

#[cfg(unix)]
pub fn peer_pid(s: &UnixStream) -> Option<i32> {
    #[cfg(target_os = "macos")]
    unsafe {
        const LOCAL_PEERPID: libc::c_int = 0x002;
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let r = libc::getsockopt(s.as_raw_fd(), 0 /* SOL_LOCAL */, LOCAL_PEERPID, &mut pid as *mut _ as *mut _, &mut len);
        (r == 0).then_some(pid)
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let r = libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut _, &mut len);
        (r == 0).then_some(cred.pid)
    }
}

#[cfg(unix)]
pub fn my_uid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Stable identity of the program on the other end of the connection, and a display name.
/// macOS: the code-signing designated requirement (pins the signer, not just the path).
/// Linux (development): the executable path. Windows, or when the OS cannot tell us the process:
/// one shared "unverified" identity, so approving it approves every local program that holds the
/// connection token. Authenticode-based identity for Windows is future work.
pub fn client_identity(pid: Option<i32>) -> (String, String) {
    let unknown = || ("unverified-client".to_string(), "unknown program".to_string());
    let Some(pid) = pid else { return unknown() };
    // Assistants reach us through our own `mcp-bridge` command. The program to approve is the one
    // that launched the bridge, not the bridge. The parent comes from the OS, so a program cannot claim
    // to be someone else; the worst it can do is launch the bridge itself and be asked about as itself.
    let pid = match (exe_of(pid), std::env::current_exe().ok()) {
        (Some(a), Some(b)) if a == b => parent_of(pid).filter(|p| *p > 1).unwrap_or(pid),
        _ => pid,
    };
    let Some(path) = exe_of(pid) else { return unknown() };
    let path = path.display().to_string();
    let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "unknown".into());
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/usr/bin/codesign").args(["-d", "-r-", &path]).output().ok();
        let req = out.and_then(|o| {
            let t = String::from_utf8_lossy(&o.stderr).to_string() + &String::from_utf8_lossy(&o.stdout);
            t.lines().find(|l| l.contains("designated =>")).map(|l| l.trim().to_string())
        });
        return (req.unwrap_or_else(|| format!("unsigned:{path}")), name);
    }
    #[allow(unreachable_code)]
    (format!("unsigned:{path}"), name)
}

/// Path of the program running as `pid` (unix only; on Windows the OS does not tell us who is on a TCP port).
fn exe_of(pid: i32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
        return (n > 0).then(|| std::path::PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).to_string()));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    }
    #[allow(unreachable_code)]
    {
        let _ = pid;
        None
    }
}

fn parent_of(pid: i32) -> Option<i32> {
    #[cfg(unix)]
    {
        let o = std::process::Command::new("/bin/ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().ok()?;
        return String::from_utf8_lossy(&o.stdout).trim().parse().ok();
    }
    #[allow(unreachable_code)]
    {
        let _ = pid;
        None
    }
}

/// Keep the computer awake while a backup runs. Dropping the guard releases it.
/// macOS: `caffeinate`. Windows: a thread holding ES_SYSTEM_REQUIRED. Elsewhere: nothing.
#[cfg(unix)]
pub struct AwakeGuard(Option<Child>);
#[cfg(windows)]
pub struct AwakeGuard(Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)>);

#[cfg(unix)]
impl AwakeGuard {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            let child = Command::new("/usr/bin/caffeinate").args(["-i", "-w", &std::process::id().to_string()]).spawn().ok();
            return Self(child);
        }
        #[allow(unreachable_code)]
        Self(None)
    }
}
#[cfg(unix)]
impl Drop for AwakeGuard {
    fn drop(&mut self) {
        if let Some(c) = &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(windows)]
impl AwakeGuard {
    pub fn new() -> Self {
        use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        // The execution state belongs to the calling thread, so a dedicated thread holds it until dropped.
        let h = std::thread::spawn(move || {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            let _ = rx.recv();
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        });
        Self(Some((tx, h)))
    }
}
#[cfg(windows)]
impl Drop for AwakeGuard {
    fn drop(&mut self) {
        if let Some((tx, h)) = self.0.take() {
            let _ = tx.send(());
            let _ = h.join();
        }
    }
}

impl Default for AwakeGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Ask macOS to mount a share (it shows the usual Keychain-backed sign-in), then wait for the folder to appear.
pub fn mount_share(url: &str, expect_path: &Path, wait_secs: u64) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("/usr/bin/open").arg(url).status().map_err(|e| e.to_string())?;
        for _ in 0..wait_secs {
            if expect_path.is_dir() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        return Err(format!("{} did not appear after mounting {url}", expect_path.display()));
    }
    #[allow(unreachable_code)]
    {
        let _ = (url, wait_secs);
        if expect_path.is_dir() { Ok(()) } else { Err("mounting is only implemented on macOS".into()) }
    }
}

/// LaunchAgent property list for the background agent.
pub fn launch_agent_plist(label: &str, exe: &Path, log_dir: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}/agent.log</string>
  <key>StandardErrorPath</key><string>{log}/agent.err</string>
</dict></plist>
"#,
        exe = exe.display(),
        log = log_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plist_mentions_label_and_exe() {
        let p = launch_agent_plist("com.example.backer", Path::new("/Applications/x/agent"), Path::new("/tmp"));
        assert!(p.contains("com.example.backer") && p.contains("/Applications/x/agent") && p.contains("KeepAlive"));
    }
}
