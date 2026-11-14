//! Where a job's files go: a mounted folder, or a NAS reached directly over SFTP, SMB or NFS.
//!
//! A destination is stored in `Job::dest_root` as a plain path (mounted) or a URL:
//!
//! - `sftp://user@nas[:22]/volume1/backups`
//! - `smb://user@nas[:445]/share/optional/folder`
//! - `nfs://nas[:2049]/volume1/backups/optional/folder?export=/volume1/backups&uid=1026&gid=100`
//!
//! The URL never holds a password. Secrets are looked up in the OS credential store by
//! [`Destination::credential_key`].
use backer_core::{Error, Result};
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A folder the OS already mounted (or a local disk).
    Mounted(PathBuf),
    Sftp { host: String, port: u16, user: String, path: String },
    Smb { host: String, port: u16, user: String, share: String, path: String },
    /// NFSv3. `export` is what the server exports (e.g. /volume1/backups); `path` is a folder inside it.
    Nfs { host: String, port: Option<u16>, export: String, path: String, uid: u32, gid: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Mounted,
    Sftp,
    Smb,
    Nfs,
}

pub const SFTP_PORT: u16 = 22;
pub const SMB_PORT: u16 = 445;

/// What a Synology (and most NAS) calls the service, for "this service appears to be off" messages.
pub fn service_hint(p: Protocol) -> &'static str {
    match p {
        Protocol::Sftp => "Check that SFTP is turned on in the NAS settings (on a Synology: Control Panel > File Services > FTP > SFTP).",
        Protocol::Smb => "Check that SMB is turned on in the NAS settings (on a Synology: Control Panel > File Services > SMB).",
        Protocol::Nfs => "Check that NFS is turned on and this folder has an NFS permission rule for this computer (on a Synology: Control Panel > File Services > NFS, and Shared Folder > Edit > NFS Permissions with \"Allow connections from non-privileged ports\").",
        Protocol::Mounted => "Check that the NAS share is mounted.",
    }
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

/// Turn a URL path ("/a/b%20c/") into clean segments, rejecting `.` and `..`.
fn segments(u: &Url) -> Result<Vec<String>> {
    let mut out = vec![];
    for s in u.path().split('/') {
        if s.is_empty() {
            continue;
        }
        let d = percent_decode(s)?;
        if d == "." || d == ".." || d.contains('/') || d.contains('\\') || d.contains('\0') {
            return Err(bad(format!("unsafe path segment: {d}")));
        }
        out.push(d);
    }
    Ok(out)
}

fn percent_decode(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
            match h {
                Some(v) => {
                    out.push(v);
                    i += 3;
                    continue;
                }
                None => return Err(bad("bad percent escape")),
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| bad("path is not UTF-8"))
}

fn user_of(u: &Url) -> Result<String> {
    if u.password().is_some() {
        return Err(bad("put the password in the password field, not in the address"));
    }
    let user = percent_decode(u.username())?;
    if user.is_empty() {
        return Err(bad("a user name is required"));
    }
    Ok(user)
}

fn host_of(u: &Url) -> Result<String> {
    let h = u.host_str().filter(|h| !h.is_empty()).ok_or_else(|| bad("a host name or IP address is required"))?;
    // IPv6 hosts come back bracketed; keep them bare for connecting.
    Ok(h.trim_start_matches('[').trim_end_matches(']').to_string())
}

fn join(segs: &[String]) -> String {
    format!("/{}", segs.join("/"))
}

impl Destination {
    /// Parse `Job::dest_root`. Anything that is not an sftp/smb/nfs URL is a mounted path.
    pub fn parse(s: &str) -> Result<Self> {
        let scheme = s.split_once("://").map(|x| x.0.to_ascii_lowercase());
        let remote = matches!(scheme.as_deref(), Some("sftp" | "smb" | "nfs"));
        if !remote {
            return Ok(Destination::Mounted(PathBuf::from(s)));
        }
        // The URL parser silently resolves "." and ".." (also when escaped), so refuse them up front.
        let raw_path = s.split_once("://").map(|x| x.1).unwrap_or("").split(['?', '#']).next().unwrap_or("");
        for seg in raw_path.split('/').skip(1) {
            if matches!(percent_decode(seg)?.as_str(), "." | "..") {
                return Err(bad(format!("unsafe path in NAS address: {s}")));
            }
        }
        let u = Url::parse(s).map_err(|e| bad(format!("bad NAS address {s}: {e}")))?;
        if u.fragment().is_some() {
            return Err(bad("unexpected # in NAS address"));
        }
        let segs = segments(&u)?;
        match u.scheme() {
            "sftp" => Ok(Destination::Sftp { host: host_of(&u)?, port: u.port().unwrap_or(SFTP_PORT), user: user_of(&u)?, path: join(&segs) }),
            "smb" => {
                let (share, rest) = segs.split_first().ok_or_else(|| bad("choose a shared folder"))?;
                Ok(Destination::Smb { host: host_of(&u)?, port: u.port().unwrap_or(SMB_PORT), user: user_of(&u)?, share: share.clone(), path: join(rest) })
            }
            "nfs" => {
                let full = join(&segs);
                let mut export = None;
                let (mut uid, mut gid) = (None, None);
                for (k, v) in u.query_pairs() {
                    match &*k {
                        "export" => export = Some(v.to_string()),
                        "uid" => uid = Some(v.parse::<u32>().map_err(|_| bad("uid must be a number"))?),
                        "gid" => gid = Some(v.parse::<u32>().map_err(|_| bad("gid must be a number"))?),
                        _ => return Err(bad(format!("unknown NFS option {k}"))),
                    }
                }
                let export = export.unwrap_or_else(|| full.clone());
                let export = format!("/{}", export.trim_matches('/'));
                let rest = if export == "/" {
                    full.clone()
                } else if full == export {
                    "/".to_string()
                } else if let Some(r) = full.strip_prefix(&format!("{export}/")) {
                    format!("/{r}")
                } else {
                    return Err(bad(format!("the folder {full} is not inside the export {export}")));
                };
                Ok(Destination::Nfs { host: host_of(&u)?, port: u.port(), export, path: rest, uid: uid.unwrap_or(default_uid()), gid: gid.unwrap_or(default_gid()) })
            }
            _ => unreachable!(),
        }
    }

    pub fn protocol(&self) -> Protocol {
        match self {
            Destination::Mounted(_) => Protocol::Mounted,
            Destination::Sftp { .. } => Protocol::Sftp,
            Destination::Smb { .. } => Protocol::Smb,
            Destination::Nfs { .. } => Protocol::Nfs,
        }
    }

    pub fn host(&self) -> Option<&str> {
        match self {
            Destination::Mounted(_) => None,
            Destination::Sftp { host, .. } | Destination::Smb { host, .. } | Destination::Nfs { host, .. } => Some(host),
        }
    }

    /// The account in the OS credential store that holds this destination's password or key:
    /// one entry per protocol, user, host and port, so jobs on the same NAS account share it.
    pub fn credential_key(&self) -> Option<String> {
        match self {
            Destination::Sftp { host, port, user, .. } => Some(format!("sftp://{user}@{host}:{port}")),
            Destination::Smb { host, port, user, .. } => Some(format!("smb://{user}@{host}:{port}")),
            _ => None,
        }
    }

    /// Key for the pinned SFTP host key.
    pub fn host_key_id(&self) -> Option<String> {
        match self {
            Destination::Sftp { host, port, .. } => Some(format!("{host}:{port}")),
            _ => None,
        }
    }

    /// The URL (or path) form stored in `Job::dest_root`.
    pub fn to_url(&self) -> String {
        fn enc(s: &str) -> String {
            let mut o = String::new();
            for b in s.bytes() {
                if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
                    o.push(b as char);
                } else {
                    o.push_str(&format!("%{b:02X}"));
                }
            }
            o
        }
        fn h(host: &str) -> String {
            if host.contains(':') { format!("[{host}]") } else { host.to_string() }
        }
        match self {
            Destination::Mounted(p) => p.to_string_lossy().into(),
            Destination::Sftp { host, port, user, path } => {
                let port = if *port == SFTP_PORT { String::new() } else { format!(":{port}") };
                format!("sftp://{}@{}{port}{}", enc(user).replace('/', "%2F"), h(host), enc(path))
            }
            Destination::Smb { host, port, user, share, path } => {
                let port = if *port == SMB_PORT { String::new() } else { format!(":{port}") };
                let p = if path == "/" { String::new() } else { enc(path) };
                format!("smb://{}@{}{port}/{}{p}", enc(user).replace('/', "%2F"), h(host), enc(share).replace('/', "%2F"))
            }
            Destination::Nfs { host, port, export, path, uid, gid } => {
                let port = port.map(|p| format!(":{p}")).unwrap_or_default();
                let full = if path == "/" { export.clone() } else if export == "/" { path.clone() } else { format!("{export}{path}") };
                format!("nfs://{}{port}{}?export={}&uid={uid}&gid={gid}", h(host), enc(&full), enc(export))
            }
        }
    }

    /// The same destination with a different folder inside it (browsing in the wizard).
    pub fn with_path(&self, p: &str) -> Result<Self> {
        let clean = clean_path(p)?;
        Ok(match self.clone() {
            Destination::Mounted(_) => return Err(bad("browse mounted folders with the folder picker")),
            Destination::Sftp { host, port, user, .. } => Destination::Sftp { host, port, user, path: clean },
            Destination::Smb { host, port, user, share, .. } => Destination::Smb { host, port, user, share, path: clean },
            Destination::Nfs { host, port, export, uid, gid, .. } => Destination::Nfs { host, port, export, path: clean, uid, gid },
        })
    }

    /// Folder inside the protocol's root (share, export or SFTP root) that the job writes under.
    pub fn base_path(&self) -> Option<&str> {
        match self {
            Destination::Mounted(_) => None,
            Destination::Sftp { path, .. } | Destination::Smb { path, .. } | Destination::Nfs { path, .. } => Some(path),
        }
    }
}

/// "/a/b" with no empty, `.` or `..` parts.
pub fn clean_path(p: &str) -> Result<String> {
    let mut out = vec![];
    for s in p.split('/') {
        if s.is_empty() {
            continue;
        }
        if s == "." || s == ".." || s.contains('\\') || s.contains('\0') {
            return Err(bad(format!("unsafe path: {p}")));
        }
        out.push(s.to_string());
    }
    Ok(join(&out))
}

#[cfg(unix)]
fn default_uid() -> u32 {
    // SAFETY: getuid/getgid cannot fail.
    unsafe { libc_getuid() }
}
#[cfg(unix)]
fn default_gid() -> u32 {
    unsafe { libc_getgid() }
}
#[cfg(unix)]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
    #[link_name = "getgid"]
    fn libc_getgid() -> u32;
}
/// Windows has no Unix ids; NAS NFS rules usually squash to a guest account anyway.
#[cfg(not(unix))]
fn default_uid() -> u32 {
    65534
}
#[cfg(not(unix))]
fn default_gid() -> u32 {
    65534
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_are_mounted() {
        assert_eq!(Destination::parse("/Volumes/backups").unwrap(), Destination::Mounted("/Volumes/backups".into()));
        assert_eq!(Destination::parse(r"Z:\backups").unwrap().protocol(), Protocol::Mounted);
        assert_eq!(Destination::parse(r"\\nas\share").unwrap().protocol(), Protocol::Mounted);
    }

    #[test]
    fn sftp_url() {
        let d = Destination::parse("sftp://howard@nas.local/volume1/backups").unwrap();
        assert_eq!(d, Destination::Sftp { host: "nas.local".into(), port: 22, user: "howard".into(), path: "/volume1/backups".into() });
        assert_eq!(d.credential_key().unwrap(), "sftp://howard@nas.local:22");
        assert_eq!(d.host_key_id().unwrap(), "nas.local:22");
        assert_eq!(Destination::parse(&d.to_url()).unwrap(), d);
        let p = Destination::parse("sftp://h@10.0.0.2:2222/").unwrap();
        assert_eq!(p, Destination::Sftp { host: "10.0.0.2".into(), port: 2222, user: "h".into(), path: "/".into() });
        assert_eq!(p.to_url(), "sftp://h@10.0.0.2:2222/");
    }

    #[test]
    fn smb_url() {
        let d = Destination::parse("smb://me@nas/Backups/Mac%20mini").unwrap();
        assert_eq!(d, Destination::Smb { host: "nas".into(), port: 445, user: "me".into(), share: "Backups".into(), path: "/Mac mini".into() });
        assert_eq!(d.to_url(), "smb://me@nas/Backups/Mac%20mini");
        let root = Destination::parse("smb://me@nas:1445/Backups").unwrap();
        assert_eq!(root.base_path(), Some("/"));
        assert_eq!(Destination::parse(&root.to_url()).unwrap(), root);
        assert!(Destination::parse("smb://me@nas/").is_err(), "share required");
    }

    #[test]
    fn nfs_url() {
        let d = Destination::parse("nfs://nas/volume1/backups/mac?export=/volume1/backups&uid=1026&gid=100").unwrap();
        assert_eq!(d, Destination::Nfs { host: "nas".into(), port: None, export: "/volume1/backups".into(), path: "/mac".into(), uid: 1026, gid: 100 });
        assert_eq!(Destination::parse(&d.to_url()).unwrap(), d);
        let whole = Destination::parse("nfs://nas/volume1/backups?uid=1&gid=2").unwrap();
        assert_eq!(whole.base_path(), Some("/"));
        assert_eq!(Destination::parse(&whole.to_url()).unwrap(), whole);
        assert!(Destination::parse("nfs://nas/other?export=/volume1/backups").is_err());
        assert!(Destination::parse("nfs://nas/x?bogus=1").is_err());
        assert!(d.credential_key().is_none());
    }

    #[test]
    fn rejects_unsafe_or_secret_urls() {
        assert!(Destination::parse("sftp://u:secret@nas/x").is_err(), "no passwords in URLs");
        assert!(Destination::parse("sftp://nas/x").is_err(), "user required");
        assert!(Destination::parse("sftp://u@nas/a/%2E%2E/b").is_err());
        assert!(Destination::parse("sftp://u@nas/a/..%2Fb").is_err());
        assert!(Destination::parse("smb://u@nas/s/a%5Cb").is_err());
        assert!(clean_path("/a/../b").is_err());
        assert_eq!(clean_path("a//b/").unwrap(), "/a/b");
    }

    #[test]
    fn with_path_keeps_connection() {
        let d = Destination::parse("smb://me@nas/Backups").unwrap();
        let e = d.with_path("/Macs/air").unwrap();
        assert_eq!(e.to_url(), "smb://me@nas/Backups/Macs/air");
        assert_eq!(e.credential_key(), d.credential_key());
    }
}
