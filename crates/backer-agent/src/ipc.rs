//! Local connections between the agent, the app and assistants.
//! Unix (macOS): a Unix socket, mode 0600, same-user check.
//! Windows: a TCP port on 127.0.0.1 plus a random token. The port and token are written to a file in the
//! user's private profile folder, so only that user's programs can read it. The TCP variant compiles
//! everywhere so it is tested on every platform.
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub trait Duplex: Read + Write + Send {
    fn dup(&self) -> io::Result<Box<dyn Duplex>>;
    fn set_timeout(&self, d: Option<Duration>) -> io::Result<()>;
}

impl Duplex for TcpStream {
    fn dup(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(d)
    }
}

#[cfg(unix)]
impl Duplex for std::os::unix::net::UnixStream {
    fn dup(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(d)
    }
}

pub struct Conn {
    pub stream: Box<dyn Duplex>,
    /// Process id of the program on the other end, when the OS can tell us.
    pub peer_pid: Option<i32>,
    expect_token: Option<String>,
}

impl Conn {
    /// TCP connections must send the token as their first line. Unix connections were checked at accept.
    pub fn authenticate(&mut self) -> bool {
        let Some(want) = self.expect_token.take() else { return true };
        let _ = self.stream.set_timeout(Some(Duration::from_secs(5)));
        let got = read_line_unbuffered(&mut self.stream);
        let _ = self.stream.set_timeout(None);
        matches!(got, Ok(g) if constant_time_eq(g.trim().as_bytes(), want.as_bytes()))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Reads one line a byte at a time so nothing after the newline is swallowed.
fn read_line_unbuffered(r: &mut impl Read) -> io::Result<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    while out.len() < 512 {
        if r.read(&mut b)? == 0 {
            break;
        }
        if b[0] == b'\n' {
            return String::from_utf8(out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        out.push(b[0]);
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "no line"))
}

enum Inner {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
    Tcp { listener: TcpListener, token: String },
}

pub struct Listener(Inner);

fn endpoint_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.endpoint"))
}

impl Listener {
    /// The platform's normal listener.
    pub fn bind(dir: &Path, name: &str) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Self::bind_unix(dir, name)
        }
        #[cfg(not(unix))]
        {
            Self::bind_tcp(dir, name)
        }
    }

    #[cfg(unix)]
    pub fn bind_unix(dir: &Path, name: &str) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(format!("{name}.sock"));
        let _ = std::fs::remove_file(&path);
        let l = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self(Inner::Unix(l)))
    }

    pub fn bind_tcp(dir: &Path, name: &str) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let ep = endpoint_path(dir, name);
        let tmp = ep.with_extension("tmp");
        std::fs::write(&tmp, serde_json::json!({"port": port, "token": token}).to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &ep)?;
        Ok(Self(Inner::Tcp { listener, token }))
    }

    /// Ok(None) means the connection was refused (wrong user or not loopback); keep listening.
    pub fn accept(&self) -> io::Result<Option<Conn>> {
        match &self.0 {
            #[cfg(unix)]
            Inner::Unix(l) => {
                let (s, _) = l.accept()?;
                if crate::platform::peer_uid(&s) != Some(crate::platform::my_uid()) {
                    return Ok(None);
                }
                let pid = crate::platform::peer_pid(&s);
                Ok(Some(Conn { stream: Box::new(s), peer_pid: pid, expect_token: None }))
            }
            Inner::Tcp { listener, token } => {
                let (s, addr) = listener.accept()?;
                if !addr.ip().is_loopback() {
                    return Ok(None);
                }
                Ok(Some(Conn { stream: Box::new(s), peer_pid: None, expect_token: Some(token.clone()) }))
            }
        }
    }
}

pub fn connect(dir: &Path, name: &str) -> io::Result<Box<dyn Duplex>> {
    #[cfg(unix)]
    {
        Ok(Box::new(std::os::unix::net::UnixStream::connect(dir.join(format!("{name}.sock")))?))
    }
    #[cfg(not(unix))]
    {
        connect_tcp(dir, name)
    }
}

pub fn connect_tcp(dir: &Path, name: &str) -> io::Result<Box<dyn Duplex>> {
    let ep: serde_json::Value = serde_json::from_slice(&std::fs::read(endpoint_path(dir, name))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let port = ep["port"].as_u64().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "port"))? as u16;
    let token = ep["token"].as_str().unwrap_or("");
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
    writeln!(s, "{token}")?;
    Ok(Box::new(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    #[test]
    fn tcp_requires_the_token() {
        let d = tempfile::tempdir().unwrap();
        let l = Listener::bind_tcp(d.path(), "t").unwrap();
        let dir = d.path().to_path_buf();
        let t = std::thread::spawn(move || {
            // good client
            let mut good = connect_tcp(&dir, "t").unwrap();
            writeln!(good, "hello").unwrap();
            // bad client: right port, wrong token
            let ep: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("t.endpoint")).unwrap()).unwrap();
            let mut bad = TcpStream::connect((Ipv4Addr::LOCALHOST, ep["port"].as_u64().unwrap() as u16)).unwrap();
            writeln!(bad, "nope").unwrap();
            (good, bad)
        });
        let mut c1 = l.accept().unwrap().unwrap();
        assert!(c1.authenticate());
        let mut line = String::new();
        io::BufReader::new(&mut c1.stream).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "hello");
        let mut c2 = l.accept().unwrap().unwrap();
        assert!(!c2.authenticate());
        let _keep = t.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let _l = Listener::bind_unix(d.path(), "u").unwrap();
        assert_eq!(std::fs::metadata(d.path().join("u.sock")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
