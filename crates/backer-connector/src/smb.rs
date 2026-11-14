//! SMB 2/3 in pure Rust (the `smb` crate), so Backer needs no mounted share and no Finder step.
//! Signing and encryption are used whenever the NAS offers them (spec section 5). One SMB session per
//! connector, opened on first use and reopened once if the network drops mid-run.
use crate::dest::{service_hint, Destination, Protocol};
use crate::remote::{Meta, RemoteFs};
use crate::{Access, DirEntry, Secret};
use backer_core::{Error, Result};
use smb::{
    Client, ClientConfig, ConnectionConfig, CreateDisposition, CreateOptions, FileAccessMask, FileAttributes, FileBasicInformation,
    FileCreateArgs, FileDirectoryInformation, FileDispositionInformation, FileRenameInformation, FileStandardInformation, Resource,
    UncPath,
};
use std::io::{Read, Write};
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);
const BUF: usize = 1 << 20;

// NTSTATUS values (MS-ERREF 2.3.1).
const ACCESS_DENIED: u32 = 0xC000_0022;
const OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const OBJECT_NAME_COLLISION: u32 = 0xC000_0035;
const OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;
const NO_SUCH_USER: u32 = 0xC000_0064;
const WRONG_PASSWORD: u32 = 0xC000_006A;
const LOGON_FAILURE: u32 = 0xC000_006D;
const ACCOUNT_RESTRICTION: u32 = 0xC000_006E;
const PASSWORD_EXPIRED: u32 = 0xC000_0071;
const ACCOUNT_DISABLED: u32 = 0xC000_0072;
const BAD_NETWORK_NAME: u32 = 0xC000_00CC;
const ACCOUNT_LOCKED_OUT: u32 = 0xC000_0234;
const NOT_A_DIRECTORY: u32 = 0xC000_0103;
const FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;

pub struct SmbFs {
    host: String,
    port: u16,
    user: String,
    share: String,
    access: Access,
    conn: Mutex<Option<Client>>,
}

fn status(e: &smb::Error) -> Option<u32> {
    match e {
        smb::Error::UnexpectedMessageStatus(s) | smb::Error::ReceivedErrorMessage(s, _) => Some(*s),
        _ => None,
    }
}

/// A network or protocol failure (worth one reconnect), as opposed to an answer about one file.
fn is_transport(e: &smb::Error) -> bool {
    status(e).is_none() && !matches!(e, smb::Error::MissingPermissions(_))
}

fn is_auth(s: u32) -> bool {
    matches!(s, NO_SUCH_USER | WRONG_PASSWORD | LOGON_FAILURE | ACCOUNT_RESTRICTION | PASSWORD_EXPIRED | ACCOUNT_DISABLED | ACCOUNT_LOCKED_OUT)
}

fn not_found(e: &smb::Error) -> bool {
    matches!(status(e), Some(OBJECT_NAME_NOT_FOUND | OBJECT_PATH_NOT_FOUND))
}

fn config(port: u16) -> ClientConfig {
    ClientConfig {
        connection: ConnectionConfig {
            port: Some(port),
            timeout: Some(TIMEOUT),
            // Encrypt when the NAS supports it (SMB 3); it stays the NAS's choice so SMB 2 boxes still work.
            encryption_mode: smb::connection::EncryptionMode::Allowed,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn password(access: &Access, user: &str, host: &str) -> Result<String> {
    match &access.secret {
        Some(Secret::Password(p)) => Ok(p.clone()),
        Some(Secret::PrivateKey { .. }) => Err(Error::Credentials("SMB needs a password, not a key".into())),
        None => Err(Error::Credentials(format!("no password is saved for {user}@{host}"))),
    }
}

/// Map a connect-time error: refused port, unreachable host, bad login or missing share.
fn connect_err(host: &str, port: u16, share: Option<&str>, e: smb::Error) -> Error {
    match status(&e) {
        Some(s) if is_auth(s) => Error::Credentials(format!("{host} rejected the user name or password")),
        Some(BAD_NETWORK_NAME) => Error::DestinationNotReady(format!("the shared folder {} was not found on {host}", share.unwrap_or("?"))),
        Some(ACCESS_DENIED) => Error::Credentials(format!("{host} denied this account access to {}", share.unwrap_or("the server"))),
        _ => {
            let msg = e.to_string();
            if msg.to_lowercase().contains("refused") {
                Error::DestinationNotReady(format!(
                    "{host} refused the connection on port {port}; the SMB service appears to be off. {}",
                    service_hint(Protocol::Smb)
                ))
            } else {
                Error::DestinationNotReady(format!("cannot reach {host}:{port} over SMB ({msg})"))
            }
        }
    }
}

/// For the setup wizard: the disk shares this account can see (hidden `$` shares left out).
pub fn list_shares(host: &str, port: u16, user: &str, access: &Access) -> Result<Vec<String>> {
    let pw = password(access, user, host)?;
    let client = Client::new(config(port));
    client.ipc_connect(host, user, pw).map_err(|e| connect_err(host, port, None, e))?;
    let out = (|| -> Result<Vec<String>> {
        let pipe = client.open_pipe(host, "srvsvc").map_err(|e| connect_err(host, port, None, e))?;
        let call = |pdu: Vec<u8>| -> Result<Vec<u8>> {
            let r = pipe
                .fsctl_with_options(smb::PipeTransceiveRequest::from(smb::IoctlBuffer::from(pdu)), 64 * 1024)
                .map_err(|e| Error::Connector(format!("listing shares on {host}: {e}")))?;
            Ok(r.to_vec())
        };
        crate::srvsvc::check_bind_ack(&call(crate::srvsvc::bind_pdu())?).map_err(|e| Error::Connector(format!("listing shares on {host}: {e}")))?;
        let mut shares = crate::srvsvc::parse_share_enum(&call(crate::srvsvc::share_enum_pdu(host))?)
            .map_err(|e| Error::Connector(format!("listing shares on {host}: {e}")))?;
        shares.sort_by_key(|s| s.to_lowercase());
        let _ = pipe.close();
        Ok(shares)
    })();
    let _ = client.close();
    out
}

impl SmbFs {
    pub fn new(dest: Destination, access: Access) -> Result<Self> {
        match dest {
            Destination::Smb { host, port, user, share, .. } => Ok(Self { host, port, user, share, access, conn: Mutex::new(None) }),
            _ => Err(Error::Invalid("not an SMB destination".into())),
        }
    }

    fn share_unc(&self) -> Result<UncPath> {
        UncPath::from_str(&format!(r"\\{}\{}", self.host, self.share)).map_err(|e| Error::Invalid(format!("bad SMB address: {e}")))
    }

    fn unc(&self, path: &str) -> Result<UncPath> {
        let p = path.trim_matches('/').replace('/', "\\");
        let base = self.share_unc()?;
        Ok(if p.is_empty() { base } else { base.with_path(&p) })
    }

    fn connect(&self) -> Result<Client> {
        let pw = password(&self.access, &self.user, &self.host)?;
        let client = Client::new(config(self.port));
        client.share_connect(&self.share_unc()?, &self.user, pw).map_err(|e| connect_err(&self.host, self.port, Some(&self.share), e))?;
        Ok(client)
    }

    /// Run `f` on a live session. A dropped connection is reopened once; per-file errors are not retried.
    fn with<T>(&self, what: &str, path: &str, f: impl Fn(&Client) -> smb::Result<T>) -> Result<T> {
        let mut g = self.conn.lock().unwrap();
        for attempt in 0..2 {
            if g.is_none() {
                *g = Some(self.connect()?);
            }
            match f(g.as_ref().unwrap()) {
                Ok(v) => return Ok(v),
                Err(e) if is_transport(&e) => {
                    if let Some(c) = g.take() {
                        let _ = c.close();
                    }
                    if attempt == 1 {
                        return Err(Error::DestinationNotReady(format!("lost the SMB connection to {} ({e})", self.host)));
                    }
                }
                Err(e) => return Err(self.file_err(what, path, &e)),
            }
        }
        unreachable!()
    }

    fn file_err(&self, what: &str, path: &str, e: &smb::Error) -> Error {
        match status(e) {
            Some(ACCESS_DENIED) => Error::Connector(format!("{what} {path}: permission denied on {}", self.host)),
            Some(OBJECT_NAME_NOT_FOUND | OBJECT_PATH_NOT_FOUND) => Error::Connector(format!("{what} {path}: not found on {}", self.host)),
            _ => Error::Connector(format!("{what} {path}: {e}")),
        }
    }

    fn open(c: &Client, unc: &UncPath, access: FileAccessMask) -> smb::Result<Resource> {
        c.create_file(unc, &FileCreateArgs::make_open_existing(access))
    }
}

fn close(r: &Resource) {
    let _ = match r {
        Resource::File(f) => f.close(),
        Resource::Directory(d) => d.close(),
        Resource::Pipe(p) => p.close(),
    };
}

/// FILETIME counts from 1601; the Unix epoch is 11,644,473,600 seconds later.
fn unix_secs(since_1601: Duration) -> i64 {
    since_1601.as_secs() as i64 - 11_644_473_600
}

fn handle(r: &Resource) -> &smb::ResourceHandle {
    match r {
        Resource::File(f) => f.handle(),
        Resource::Directory(d) => d.handle(),
        Resource::Pipe(p) => p.handle(),
    }
}

fn io_err(e: std::io::Error) -> smb::Error {
    smb::Error::IoError(e)
}

impl RemoteFs for SmbFs {
    fn stat(&self, path: &str) -> Result<Option<Meta>> {
        let unc = self.unc(path)?;
        self.with("stat", path, |c| {
            let r = match Self::open(c, &unc, FileAccessMask::new().with_file_read_attributes(true)) {
                Ok(r) => r,
                Err(e) if not_found(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
            let out = (|| {
                let std: FileStandardInformation = handle(&r).query_info()?;
                let basic: FileBasicInformation = handle(&r).query_info()?;
                Ok(Meta { is_dir: bool::from(std.directory), size: std.end_of_file, mtime: unix_secs(basic.last_write_time.since_epoch()) })
            })();
            close(&r);
            out.map(Some)
        })
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        let unc = self.unc(path)?;
        self.with("create folder", path, |c| {
            let args = FileCreateArgs {
                disposition: CreateDisposition::Create,
                attributes: FileAttributes::new().with_directory(true),
                options: CreateOptions::new().with_directory_file(true),
                desired_access: FileAccessMask::new().with_generic_read(true),
            };
            match c.create_file(&unc, &args) {
                Ok(r) => {
                    close(&r);
                    Ok(())
                }
                Err(e) if status(&e) == Some(OBJECT_NAME_COLLISION) => match Self::open(c, &unc, FileAccessMask::new().with_file_read_attributes(true)) {
                    Ok(r) => {
                        let is_dir = r.is_dir();
                        close(&r);
                        if is_dir { Ok(()) } else { Err(smb::Error::UnexpectedMessageStatus(NOT_A_DIRECTORY)) }
                    }
                    Err(e2) => Err(e2),
                },
                Err(e) => Err(e),
            }
        })
    }

    fn write(&self, path: &str, src: &mut dyn Read) -> Result<u64> {
        // Attempted once: a retry would need the source again. The engine retries the file next run.
        let unc = self.unc(path)?;
        let mut g = self.conn.lock().unwrap();
        if g.is_none() {
            *g = Some(self.connect()?);
        }
        let c = g.as_ref().unwrap();
        let r = (|| -> smb::Result<u64> {
            let args = FileCreateArgs::make_overwrite(FileAttributes::new(), CreateOptions::new().with_non_directory_file(true));
            let res = c.create_file(&unc, &args)?;
            let Resource::File(mut f) = res else {
                close(&res);
                return Err(smb::Error::UnexpectedMessageStatus(FILE_IS_A_DIRECTORY));
            };
            let out = (|| {
                let mut buf = vec![0u8; BUF];
                let mut n = 0u64;
                loop {
                    let k = src.read(&mut buf).map_err(io_err)?;
                    if k == 0 {
                        break;
                    }
                    f.write_all(&buf[..k]).map_err(io_err)?;
                    n += k as u64;
                }
                f.flush().map_err(io_err)?;
                Ok(n)
            })();
            let _ = f.close();
            out
        })();
        match r {
            Ok(n) => Ok(n),
            Err(e) if is_transport(&e) && !matches!(e, smb::Error::IoError(_)) => {
                *g = None;
                Err(Error::DestinationNotReady(format!("lost the SMB connection to {} ({e})", self.host)))
            }
            Err(e) => Err(self.file_err("write", path, &e)),
        }
    }

    fn read(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let unc = self.unc(path)?;
        let mut g = self.conn.lock().unwrap();
        if g.is_none() {
            *g = Some(self.connect()?);
        }
        let c = g.as_ref().unwrap();
        let r = (|| -> smb::Result<u64> {
            let res = Self::open(c, &unc, FileAccessMask::new().with_generic_read(true))?;
            let Resource::File(mut f) = res else {
                close(&res);
                return Err(smb::Error::UnexpectedMessageStatus(FILE_IS_A_DIRECTORY));
            };
            let r = (|| {
                let mut buf = vec![0u8; BUF];
                let mut n = 0u64;
                loop {
                    let k = f.read(&mut buf).map_err(io_err)?;
                    if k == 0 {
                        break;
                    }
                    out.write_all(&buf[..k]).map_err(io_err)?;
                    n += k as u64;
                }
                Ok(n)
            })();
            let _ = f.close();
            r
        })();
        match r {
            Ok(n) => Ok(n),
            Err(e) if is_transport(&e) && !matches!(e, smb::Error::IoError(_)) => {
                *g = None;
                Err(Error::DestinationNotReady(format!("lost the SMB connection to {} ({e})", self.host)))
            }
            Err(e) => Err(self.file_err("read", path, &e)),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let unc = self.unc(from)?;
        // SMB2 rename takes the new name relative to the share root.
        let target = to.trim_matches('/').replace('/', "\\");
        self.with("rename", to, |c| {
            let r = Self::open(c, &unc, FileAccessMask::new().with_delete(true).with_file_read_attributes(true))?;
            let out = handle(&r).set_info(FileRenameInformation { replace_if_exists: true.into(), root_directory: 0, file_name: target.as_str().into() });
            close(&r);
            out
        })
    }

    fn remove(&self, path: &str) -> Result<()> {
        let unc = self.unc(path)?;
        self.with("remove", path, |c| {
            let r = Self::open(c, &unc, FileAccessMask::new().with_delete(true))?;
            let out = handle(&r).set_info(FileDispositionInformation::default());
            close(&r);
            out
        })
    }

    fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        let unc = self.unc(path)?;
        self.with("list", path, |c| {
            let args = FileCreateArgs {
                disposition: CreateDisposition::Open,
                attributes: FileAttributes::new(),
                options: CreateOptions::new().with_directory_file(true),
                desired_access: FileAccessMask::new().with_generic_read(true),
            };
            let r = c.create_file(&unc, &args)?;
            let Resource::Directory(d) = &r else {
                close(&r);
                return Err(smb::Error::UnexpectedMessageStatus(NOT_A_DIRECTORY));
            };
            let mut out = vec![];
            let res = (|| {
                for item in d.query::<FileDirectoryInformation>("*")? {
                    let i = item?;
                    let name = i.file_name.to_string();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let is_dir = i.file_attributes.directory();
                    out.push(DirEntry { name, is_dir, size: if is_dir { 0 } else { i.end_of_file }, mtime: unix_secs(i.last_write_time.since_epoch()) });
                }
                Ok(())
            })();
            close(&r);
            res.map(|_| out)
        })
    }

    fn describe(&self) -> String {
        format!("smb://{}/{}", self.host, self.share)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_port_says_service_is_off() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let d = Destination::parse(&format!("smb://u@127.0.0.1:{port}/share")).unwrap();
        let fs = SmbFs::new(d, Access { secret: Some(Secret::Password("p".into())), host_key: None }).unwrap();
        let e = fs.stat("/").unwrap_err();
        assert!(matches!(e, Error::DestinationNotReady(_)), "{e}");
    }

    #[test]
    fn no_password_is_a_credentials_error() {
        let d = Destination::parse("smb://u@127.0.0.1:1/share").unwrap();
        let fs = SmbFs::new(d, Access::default()).unwrap();
        assert!(matches!(fs.stat("/").unwrap_err(), Error::Credentials(_)));
    }

    #[test]
    fn filetime_converts_to_unix() {
        // 2020-01-01T00:00:00Z
        assert_eq!(unix_secs(Duration::from_secs(13_222_310_400)), 1_577_836_800);
    }
}
