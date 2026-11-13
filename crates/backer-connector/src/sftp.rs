//! SFTP through libssh2. One SSH session per connector, opened on first use and reopened once if
//! the network drops mid-run. The server's host key must match the key pinned on first connect.
use crate::dest::{service_hint, Destination, Protocol};
use crate::remote::{Meta, RemoteFs};
use crate::{Access, DirEntry, Secret};
use backer_core::{Error, Result};
use ssh2::{ErrorCode, FileStat, RenameFlags, Session, Sftp};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for any one reply from the server before the connection counts as dropped.
const IO_TIMEOUT_MS: u32 = 60_000;
const BUF: usize = 256 * 1024;

// SFTP status codes (draft-ietf-secsh-filexfer-02).
const FX_NO_SUCH_FILE: i32 = 2;
const FX_PERMISSION_DENIED: i32 = 3;

struct Conn {
    _sess: Session,
    sftp: Sftp,
}

pub struct SftpFs {
    host: String,
    port: u16,
    user: String,
    access: Access,
    conn: Mutex<Option<Conn>>,
}

/// OpenSSH-style fingerprint: "SHA256:" + unpadded base64 of the SHA-256 of the host key.
pub fn fingerprint(sha256: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::from("SHA256:");
    for c in sha256.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    s
}

fn unreachable(host: &str, port: u16, e: impl std::fmt::Display) -> Error {
    Error::DestinationNotReady(format!("cannot reach {host}:{port} over SFTP ({e})"))
}

/// Open TCP and do the SSH handshake, without logging in. Returns the session and the server's key fingerprint.
fn handshake(host: &str, port: u16) -> Result<(Session, String)> {
    let addrs: Vec<_> = (host, port).to_socket_addrs().map_err(|e| unreachable(host, port, e))?.collect();
    let mut last = None;
    let mut tcp = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(t) => {
                tcp = Some(t);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let tcp = match (tcp, last) {
        (Some(t), _) => t,
        (None, Some(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return Err(Error::DestinationNotReady(format!(
                "{host} refused the connection on port {port}; the SFTP service appears to be off. {}",
                service_hint(Protocol::Sftp)
            )))
        }
        (None, Some(e)) => return Err(unreachable(host, port, e)),
        (None, None) => return Err(unreachable(host, port, "no address")),
    };
    let _ = tcp.set_nodelay(true);
    let mut sess = Session::new().map_err(|e| Error::Connector(e.to_string()))?;
    sess.set_tcp_stream(tcp);
    sess.set_timeout(IO_TIMEOUT_MS);
    sess.handshake().map_err(|e| unreachable(host, port, format!("SSH handshake failed: {e}")))?;
    let fp = sess
        .host_key_hash(ssh2::HashType::Sha256)
        .map(fingerprint)
        .ok_or_else(|| Error::Connector("the server sent no host key".into()))?;
    Ok((sess, fp))
}

/// For the setup wizard: the server's host key fingerprint, so the user can confirm it before it is pinned.
pub fn probe_host_key(host: &str, port: u16) -> Result<String> {
    handshake(host, port).map(|x| x.1)
}

fn is_transport(e: &ssh2::Error) -> bool {
    // Session-level (negative) codes are network or protocol failures; SFTP codes are per-file answers.
    matches!(e.code(), ErrorCode::Session(_))
}

fn sftp_code(e: &ssh2::Error) -> Option<i32> {
    match e.code() {
        ErrorCode::SFTP(c) => Some(c),
        _ => None,
    }
}

impl SftpFs {
    pub fn new(dest: Destination, access: Access) -> Result<Self> {
        match dest {
            Destination::Sftp { host, port, user, .. } => Ok(Self { host, port, user, access, conn: Mutex::new(None) }),
            _ => Err(Error::Invalid("not an SFTP destination".into())),
        }
    }

    fn connect(&self) -> Result<Conn> {
        let (sess, fp) = handshake(&self.host, self.port)?;
        match &self.access.host_key {
            None => {
                return Err(Error::HostKey(format!(
                    "{} has not been confirmed yet (its key is {fp}); confirm it in the job's connection settings",
                    self.host
                )))
            }
            Some(pinned) if *pinned != fp => {
                return Err(Error::HostKey(format!(
                    "{} now presents host key {fp}, not the pinned {pinned}. Backups to it are paused until you approve the new key",
                    self.host
                )))
            }
            Some(_) => {}
        }
        let auth = match &self.access.secret {
            None => return Err(Error::Credentials(format!("no password is saved for {}@{}", self.user, self.host))),
            Some(Secret::Password(p)) => sess.userauth_password(&self.user, p),
            Some(Secret::PrivateKey { pem, passphrase }) => sess.userauth_pubkey_memory(&self.user, None, pem, passphrase.as_deref()),
        };
        if let Err(e) = auth {
            if is_transport(&e) && !matches!(e.code(), ErrorCode::Session(-18) | ErrorCode::Session(-19)) {
                // -18 = authentication failed, -19 = public key unverified
                return Err(unreachable(&self.host, self.port, e));
            }
            return Err(Error::Credentials(format!("{} rejected the user name or password for {}", self.host, self.user)));
        }
        if !sess.authenticated() {
            return Err(Error::Credentials(format!("{} did not accept the login for {}", self.host, self.user)));
        }
        let sftp = sess.sftp().map_err(|e| {
            Error::DestinationNotReady(format!("{} accepted the login but has no SFTP service ({e}). {}", self.host, service_hint(Protocol::Sftp)))
        })?;
        Ok(Conn { _sess: sess, sftp })
    }

    /// Run `f` on a live connection. A dropped connection is reopened once; per-file errors are not retried.
    fn with<T>(&self, what: &str, path: &str, f: impl Fn(&Sftp) -> std::result::Result<T, ssh2::Error>) -> Result<T> {
        let mut g = self.conn.lock().unwrap();
        for attempt in 0..2 {
            if g.is_none() {
                *g = Some(self.connect()?);
            }
            match f(&g.as_ref().unwrap().sftp) {
                Ok(v) => return Ok(v),
                Err(e) if is_transport(&e) && attempt == 0 => {
                    *g = None;
                }
                Err(e) if is_transport(&e) => {
                    *g = None;
                    return Err(unreachable(&self.host, self.port, e));
                }
                Err(e) => return Err(self.file_err(what, path, &e)),
            }
        }
        unreachable!()
    }

    fn file_err(&self, what: &str, path: &str, e: &ssh2::Error) -> Error {
        match sftp_code(e) {
            Some(FX_PERMISSION_DENIED) => Error::Connector(format!("{what} {path}: permission denied on {}", self.host)),
            Some(FX_NO_SUCH_FILE) => Error::Connector(format!("{what} {path}: not found on {}", self.host)),
            _ => Error::Connector(format!("{what} {path}: {e}")),
        }
    }
}

fn meta(st: &FileStat) -> Meta {
    Meta { is_dir: st.is_dir(), size: st.size.unwrap_or(0), mtime: st.mtime.unwrap_or(0) as i64 }
}

/// Wraps an io error from a file read/write so `with` can tell network drops from file errors.
fn io(e: std::io::Error) -> ssh2::Error {
    match e.into_inner().map(|x| x.downcast::<ssh2::Error>()) {
        Some(Ok(se)) => *se,
        // -7 = LIBSSH2_ERROR_SOCKET_SEND: treated as a dropped connection.
        Some(Err(_)) | None => ssh2::Error::new(ErrorCode::Session(-7), "network I/O error"),
    }
}

impl RemoteFs for SftpFs {
    fn stat(&self, path: &str) -> Result<Option<Meta>> {
        self.with("stat", path, |s| match s.stat(Path::new(path)) {
            Ok(st) => Ok(Some(meta(&st))),
            Err(e) if sftp_code(&e) == Some(FX_NO_SUCH_FILE) => Ok(None),
            Err(e) => Err(e),
        })
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        self.with("create folder", path, |s| match s.mkdir(Path::new(path), 0o755) {
            Ok(()) => Ok(()),
            Err(e) if is_transport(&e) => Err(e),
            Err(e) => match s.stat(Path::new(path)) {
                Ok(st) if st.is_dir() => Ok(()),
                _ => Err(e),
            },
        })
    }

    fn write(&self, path: &str, src: &mut dyn Read) -> Result<u64> {
        // A retry after a dropped connection would need the source again, so a write is attempted once;
        // the engine retries the file on the next run.
        let mut g = self.conn.lock().unwrap();
        if g.is_none() {
            *g = Some(self.connect()?);
        }
        let sftp = &g.as_ref().unwrap().sftp;
        let r = (|| -> std::result::Result<u64, ssh2::Error> {
            let mut f = sftp.create(Path::new(path))?;
            let mut buf = vec![0u8; BUF];
            let mut n = 0u64;
            loop {
                let k = src.read(&mut buf).map_err(|_| ssh2::Error::new(ErrorCode::SFTP(4), "could not read the source file"))?;
                if k == 0 {
                    break;
                }
                f.write_all(&buf[..k]).map_err(io)?;
                n += k as u64;
            }
            // fsync@openssh.com where the server supports it (OpenSSH, Synology); harmless otherwise.
            let _ = f.fsync();
            f.close()?;
            Ok(n)
        })();
        match r {
            Ok(n) => Ok(n),
            Err(e) if is_transport(&e) => {
                *g = None;
                Err(unreachable(&self.host, self.port, e))
            }
            Err(e) => Err(self.file_err("write", path, &e)),
        }
    }

    fn read(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let mut g = self.conn.lock().unwrap();
        if g.is_none() {
            *g = Some(self.connect()?);
        }
        let sftp = &g.as_ref().unwrap().sftp;
        let r = (|| -> std::result::Result<u64, ssh2::Error> {
            let mut f = sftp.open(Path::new(path))?;
            let mut buf = vec![0u8; BUF];
            let mut n = 0u64;
            loop {
                let k = f.read(&mut buf).map_err(io)?;
                if k == 0 {
                    break;
                }
                out.write_all(&buf[..k]).map_err(|_| ssh2::Error::new(ErrorCode::SFTP(4), "could not write the local copy"))?;
                n += k as u64;
            }
            Ok(n)
        })();
        match r {
            Ok(n) => Ok(n),
            Err(e) if is_transport(&e) => {
                *g = None;
                Err(unreachable(&self.host, self.port, e))
            }
            Err(e) => Err(self.file_err("read", path, &e)),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.with("rename", to, |s| {
            let flags = Some(RenameFlags::OVERWRITE | RenameFlags::ATOMIC | RenameFlags::NATIVE);
            match s.rename(Path::new(from), Path::new(to), flags) {
                Ok(()) => Ok(()),
                Err(e) if is_transport(&e) => Err(e),
                Err(e) => {
                    // SFTP v3 servers (OpenSSH) refuse to rename over an existing file. The new copy is already
                    // verified under its temp name, so replace the old copy of this same file and rename again.
                    match s.stat(Path::new(to)) {
                        Ok(st) if !st.is_dir() => {
                            s.unlink(Path::new(to))?;
                            s.rename(Path::new(from), Path::new(to), None)
                        }
                        _ => Err(e),
                    }
                }
            }
        })
    }

    fn remove(&self, path: &str) -> Result<()> {
        self.with("remove", path, |s| s.unlink(Path::new(path)))
    }

    fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        self.with("list", path, |s| {
            Ok(s.readdir(Path::new(path))?
                .into_iter()
                .filter_map(|(p, st)| {
                    let name = p.file_name()?.to_string_lossy().to_string();
                    (name != "." && name != "..").then(|| {
                        let m = meta(&st);
                        DirEntry { name, is_dir: m.is_dir, size: if m.is_dir { 0 } else { m.size }, mtime: m.mtime }
                    })
                })
                .collect())
        })
    }

    fn describe(&self) -> String {
        format!("sftp://{}", self.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_openssh_format() {
        // `ssh-keygen -lf` prints unpadded base64 of the SHA-256 digest.
        let digest = [0u8; 32];
        assert_eq!(fingerprint(&digest), "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        let d: Vec<u8> = (0..32).collect();
        assert_eq!(fingerprint(&d), "SHA256:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8");
    }

    #[test]
    fn refused_port_says_service_is_off() {
        // Bind then drop to find a closed local port.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let e = probe_host_key("127.0.0.1", port).unwrap_err();
        assert!(matches!(e, Error::DestinationNotReady(_)), "{e}");
        assert!(e.to_string().contains("appears to be off"), "{e}");
    }
}
