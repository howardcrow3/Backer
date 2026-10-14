//! OS-specific helpers. macOS is the target; the Linux branches exist so the agent builds and tests in CI.
use std::os::unix::net::UnixStream;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Child;
#[cfg(target_os = "macos")]
use std::process::Command;

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

pub fn my_uid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Stable identity of the program on the other end of the socket, and a display name.
/// macOS: the code-signing designated requirement (changes if the program is not signed by the same developer).
/// Unsigned programs get an identity of "unsigned:<path>" and can still be approved by the user.
pub fn client_identity(pid: i32) -> (String, String) {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
        let path = if n > 0 { String::from_utf8_lossy(&buf[..n as usize]).to_string() } else { String::new() };
        let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "unknown".into());
        // `codesign -d -r-` prints the designated requirement, which pins the signer, not just the path.
        let out = Command::new("/usr/bin/codesign").args(["-d", "-r-", &path]).output().ok();
        let req = out.and_then(|o| {
            let t = String::from_utf8_lossy(&o.stderr).to_string() + &String::from_utf8_lossy(&o.stdout);
            t.lines().find(|l| l.contains("designated =>")).map(|l| l.trim().to_string())
        });
        (req.unwrap_or_else(|| format!("unsigned:{path}")), name)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let path = std::fs::read_link(format!("/proc/{pid}/exe")).map(|p| p.display().to_string()).unwrap_or_default();
        let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "unknown".into());
        (format!("unsigned:{path}"), name)
    }
}

/// Keep the Mac awake while a backup runs (macOS). Dropping the guard releases it.
pub struct AwakeGuard(Option<Child>);
impl AwakeGuard {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            // caffeinate -i prevents idle sleep; -w exits when this process does.
            let child = Command::new("/usr/bin/caffeinate").args(["-i", "-w", &std::process::id().to_string()]).spawn().ok();
            return Self(child);
        }
        #[allow(unreachable_code)]
        Self(None)
    }
}
impl Default for AwakeGuard {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for AwakeGuard {
    fn drop(&mut self) {
        if let Some(c) = &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
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
        let p = launch_agent_plist("com.example.nasbak", Path::new("/Applications/x/agent"), Path::new("/tmp"));
        assert!(p.contains("com.example.nasbak") && p.contains("/Applications/x/agent") && p.contains("KeepAlive"));
    }
}
