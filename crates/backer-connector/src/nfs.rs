//! NFSv3 in pure Rust (the `nfs3_client` crate). No OS mount, no admin rights: Backer talks to the
//! NAS's mount and NFS services itself, as the Unix user id set on the destination (AUTH_SYS).
//! One connection per connector, opened on first use and reopened once if the network drops.
use crate::dest::{service_hint, Destination, Protocol};
use crate::remote::{Meta, RemoteFs};
use crate::DirEntry;
use backer_core::{Error, Result};
use nfs3_client::nfs3_types::nfs3::{
    self, createhow3, diropargs3, fattr3, filename3, ftype3, nfs_fh3, nfsstat3, sattr3, stable_how, Nfs3Option, Nfs3Result,
};
use nfs3_client::nfs3_types::rpc::{auth_unix, opaque_auth};
use nfs3_client::nfs3_types::xdr_codec::Opaque;
use nfs3_client::tokio::TokioIo;
use nfs3_client::{ConnectError, MountClient, MountError, Nfs3Connection, Nfs3ConnectionBuilder, PortmapperClient, RpcError};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

type Conn = Nfs3Connection<TokioIo<tokio::net::TcpStream>>;

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_IO: u32 = 1 << 20;

struct State {
    conn: Conn,
    root: nfs_fh3,
    /// Folder handles by path, so each file costs one LOOKUP rather than one per path segment.
    dirs: HashMap<String, nfs_fh3>,
    rsize: u32,
    wsize: u32,
}

pub struct NfsFs {
    host: String,
    /// Fixed NFS port (skips the portmapper; mount then uses the same port, as NFS-over-one-port servers do).
    port: Option<u16>,
    export: String,
    uid: u32,
    gid: u32,
    rt: tokio::runtime::Runtime,
    state: Mutex<Option<State>>,
}

/// What went wrong, before it becomes a Backer error.
enum E {
    /// The NFS server answered with an error for this file.
    Nfs(nfsstat3),
    /// Network or protocol failure: worth one reconnect.
    Net(String),
    /// Other (the server sent something unusable).
    Other(String),
}

impl From<RpcError> for E {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::Io(io) => E::Net(io.to_string()),
            other => E::Other(other.to_string()),
        }
    }
}

fn st<T, F>(r: Nfs3Result<T, F>) -> std::result::Result<T, E> {
    match r {
        Nfs3Result::Ok(v) => Ok(v),
        Nfs3Result::Err((s, _)) => Err(E::Nfs(s)),
    }
}

fn name(n: &str) -> filename3<'static> {
    filename3(Opaque::owned(n.as_bytes().to_vec()))
}

fn meta(a: &fattr3) -> Meta {
    Meta { is_dir: matches!(a.type_, ftype3::NF3DIR), size: a.size, mtime: a.mtime.seconds as i64 }
}

fn split(path: &str) -> (String, String) {
    let p = path.trim_end_matches('/');
    match p.rsplit_once('/') {
        Some(("", n)) => ("/".into(), n.into()),
        Some((d, n)) => (d.into(), n.into()),
        None => ("/".into(), p.into()),
    }
}

fn resolve(host: &str) -> Result<IpAddr> {
    (host, 0)
        .to_socket_addrs()
        .map_err(|e| Error::DestinationNotReady(format!("cannot find {host} on the network ({e})")))?
        .map(|a| a.ip())
        .next()
        .ok_or_else(|| Error::DestinationNotReady(format!("cannot find {host} on the network")))
}

fn credential(uid: u32, gid: u32) -> opaque_auth<'static> {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "backer".into());
    opaque_auth::auth_unix(&auth_unix { stamp: 0, machinename: Opaque::owned(host.into_bytes()), uid, gid, gids: vec![gid] })
}

fn connect_err(host: &str, export: Option<&str>, e: ConnectError) -> Error {
    match e {
        ConnectError::Mount(MountError::Denied(s)) => {
            let s = format!("{s:?}");
            if s.contains("NOENT") {
                Error::DestinationNotReady(format!("{host} does not export {}", export.unwrap_or("that folder")))
            } else {
                // MNT3ERR_ACCES / PERM: the NFS permission rules do not include this computer.
                Error::Credentials(format!("{host} refused to share {} with this computer ({s}). {}", export.unwrap_or("the folder"), service_hint(Protocol::Nfs)))
            }
        }
        ConnectError::Mount(MountError::Rpc(RpcError::Auth(a))) => Error::Credentials(format!("{host} rejected this computer's NFS credentials ({a:?})")),
        ConnectError::Portmap(_) => Error::DestinationNotReady(format!(
            "{host} did not answer as an NFS server; the NFS service appears to be off. {}",
            service_hint(Protocol::Nfs)
        )),
        ConnectError::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => Error::DestinationNotReady(format!(
            "{host} refused the connection; the NFS service appears to be off. {}",
            service_hint(Protocol::Nfs)
        )),
        other => Error::DestinationNotReady(format!("cannot reach {host} over NFS ({other})")),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| Error::Connector(e.to_string()))
}

/// For the setup wizard: the folders the NAS exports over NFS.
pub fn list_exports(host: &str, port: Option<u16>) -> Result<Vec<String>> {
    let ip = resolve(host)?;
    let rt = runtime()?;
    rt.block_on(async {
        let out = tokio::time::timeout(TIMEOUT, async {
            let mount_port = match port {
                Some(p) => p,
                None => {
                    let io = Tcp.connect_io(SocketAddr::new(ip, nfs3_client::nfs3_types::portmap::PMAP_PORT)).await.map_err(ConnectError::Io)?;
                    let mut pm = PortmapperClient::new(io);
                    pm.getport(nfs3_client::nfs3_types::mount::PROGRAM, nfs3_client::nfs3_types::mount::VERSION, nfs3_client::nfs3_types::portmap::IPPROTO_TCP)
                        .await
                        .map_err(ConnectError::Portmap)?
                }
            };
            let io = Tcp.connect_io(SocketAddr::new(ip, mount_port)).await.map_err(ConnectError::Io)?;
            let mut m = MountClient::new(io);
            let ex = m.export().await.map_err(|e| ConnectError::Mount(MountError::Rpc(e)))?;
            Ok::<_, ConnectError>(ex.into_inner().into_iter().map(|n| String::from_utf8_lossy(&n.ex_dir.0.into_owned()).to_string()).collect::<Vec<_>>())
        })
        .await
        .map_err(|_| Error::DestinationNotReady(format!("{host} did not answer over NFS")))?;
        out.map_err(|e| connect_err(host, None, e))
    })
}

/// Tokio TCP with TCP_NODELAY: NFS is request/response, so Nagle's algorithm would add ~40 ms per call.
struct Tcp;
impl nfs3_client::net::Connector for Tcp {
    type Connection = TokioIo<tokio::net::TcpStream>;
    async fn connect(&self, addr: SocketAddr) -> std::io::Result<Self::Connection> {
        let s = tokio::net::TcpStream::connect(addr).await?;
        s.set_nodelay(true)?;
        Ok(TokioIo::new(s))
    }
    async fn connect_with_port(&self, addr: SocketAddr, local_port: u16) -> std::io::Result<Self::Connection> {
        let sock = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
        let local: SocketAddr = if addr.is_ipv4() { ([0, 0, 0, 0], local_port).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, local_port).into() };
        sock.bind(local)?;
        let s = sock.connect(addr).await?;
        s.set_nodelay(true)?;
        Ok(TokioIo::new(s))
    }
}

impl Tcp {
    async fn connect_io(&self, a: SocketAddr) -> std::io::Result<TokioIo<tokio::net::TcpStream>> {
        nfs3_client::net::Connector::connect(self, a).await
    }
}

impl NfsFs {
    pub fn new(dest: Destination) -> Result<Self> {
        match dest {
            Destination::Nfs { host, port, export, uid, gid, .. } => Ok(Self { host, port, export, uid, gid, rt: runtime()?, state: Mutex::new(None) }),
            _ => Err(Error::Invalid("not an NFS destination".into())),
        }
    }

    async fn mount(&self, ip: IpAddr, privileged: bool) -> std::result::Result<Conn, ConnectError> {
        let mut b = Nfs3ConnectionBuilder::new(Tcp, ip.to_string(), &self.export)
            .credential(credential(self.uid, self.gid))
            .connect_from_privileged_port(privileged);
        if let Some(p) = self.port {
            b = b.mount_port(p).nfs3_port(p);
        }
        b.mount().await
    }

    fn connect(&self) -> Result<State> {
        let ip = resolve(&self.host)?;
        self.rt.block_on(async {
            // Many NAS (Synology by default) only accept clients on ports below 1024. macOS lets any user
            // bind those; elsewhere fall back to a normal port, which works when the NAS allows it.
            let conn = match tokio::time::timeout(TIMEOUT, self.mount(ip, true)).await {
                Ok(Ok(c)) => c,
                Ok(Err(ConnectError::Io(e))) if matches!(e.kind(), std::io::ErrorKind::PermissionDenied) || e.to_string().contains("privileged port") => {
                    tokio::time::timeout(TIMEOUT, self.mount(ip, false)).await.map_err(|_| self.timeout())?.map_err(|e| connect_err(&self.host, Some(&self.export), e))?
                }
                Ok(Err(e)) => return Err(connect_err(&self.host, Some(&self.export), e)),
                Err(_) => return Err(self.timeout()),
            };
            let mut conn = conn;
            let root = conn.root_nfs_fh3();
            let (mut rsize, mut wsize) = (64 * 1024, 64 * 1024);
            if let Ok(Ok(Nfs3Result::Ok(fi))) = tokio::time::timeout(TIMEOUT, conn.fsinfo(&nfs3::FSINFO3args { fsroot: root.clone() })).await {
                rsize = fi.rtpref.max(fi.rtmax.min(MAX_IO)).clamp(4096, MAX_IO);
                wsize = fi.wtpref.max(fi.wtmax.min(MAX_IO)).clamp(4096, MAX_IO);
            }
            Ok(State { conn, root, dirs: HashMap::new(), rsize, wsize })
        })
    }

    fn timeout(&self) -> Error {
        Error::DestinationNotReady(format!("{} did not answer over NFS", self.host))
    }

    /// Run an operation on a live connection. A dropped connection is reopened once.
    fn with<T>(&self, what: &str, path: &str, retry: bool, f: impl for<'a> Fn(&'a mut State) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<T, E>> + 'a>>) -> Result<T> {
        let mut g = self.state.lock().unwrap();
        for attempt in 0..2 {
            if g.is_none() {
                *g = Some(self.connect()?);
            }
            let s = g.as_mut().unwrap();
            let r = self.rt.block_on(async { tokio::time::timeout(TIMEOUT, f(s)).await.unwrap_or_else(|_| Err(E::Net("timed out".into()))) });
            match r {
                Ok(v) => return Ok(v),
                Err(E::Net(m)) => {
                    *g = None;
                    if attempt == 1 || !retry {
                        return Err(Error::DestinationNotReady(format!("lost the NFS connection to {} ({m})", self.host)));
                    }
                }
                Err(E::Nfs(s)) => return Err(self.file_err(what, path, s)),
                Err(E::Other(m)) => return Err(Error::Connector(format!("{what} {path}: {m}"))),
            }
        }
        unreachable!()
    }

    fn file_err(&self, what: &str, path: &str, s: nfsstat3) -> Error {
        match s {
            nfsstat3::NFS3ERR_ACCES | nfsstat3::NFS3ERR_PERM | nfsstat3::NFS3ERR_ROFS => {
                Error::Connector(format!("{what} {path}: permission denied on {} (user id {})", self.host, self.uid))
            }
            nfsstat3::NFS3ERR_NOENT => Error::Connector(format!("{what} {path}: not found on {}", self.host)),
            nfsstat3::NFS3ERR_NOSPC | nfsstat3::NFS3ERR_DQUOT => Error::Connector(format!("{what} {path}: the NAS is out of space")),
            nfsstat3::NFS3ERR_STALE => Error::DestinationNotReady(format!("{what} {path}: the NAS folder changed under Backer (stale handle)")),
            other => Error::Connector(format!("{what} {path}: {other:?}")),
        }
    }
}

impl State {
    async fn lookup(&mut self, dir: &nfs_fh3, n: &str) -> std::result::Result<Option<(nfs_fh3, Option<fattr3>)>, E> {
        let r = self.conn.lookup(&nfs3::LOOKUP3args { what: diropargs3 { dir: dir.clone(), name: name(n) } }).await?;
        match r {
            Nfs3Result::Ok(ok) => Ok(Some((ok.object, match ok.obj_attributes { Nfs3Option::Some(a) => Some(a), Nfs3Option::None => None }))),
            Nfs3Result::Err((nfsstat3::NFS3ERR_NOENT, _)) => Ok(None),
            Nfs3Result::Err((s, _)) => Err(E::Nfs(s)),
        }
    }

    async fn getattr(&mut self, fh: &nfs_fh3) -> std::result::Result<fattr3, E> {
        Ok(st(self.conn.getattr(&nfs3::GETATTR3args { object: fh.clone() }).await?)?.obj_attributes)
    }

    /// Handle of a folder, `None` if it is missing.
    async fn dir(&mut self, path: &str) -> std::result::Result<Option<nfs_fh3>, E> {
        let path = if path.is_empty() { "/" } else { path };
        if path == "/" {
            return Ok(Some(self.root.clone()));
        }
        if let Some(h) = self.dirs.get(path) {
            return Ok(Some(h.clone()));
        }
        let (parent, n) = split(path);
        let Some(p) = Box::pin(self.dir(&parent)).await? else { return Ok(None) };
        match self.lookup(&p, &n).await? {
            Some((h, a)) => {
                let a = match a {
                    Some(a) => a,
                    None => self.getattr(&h).await?,
                };
                if !matches!(a.type_, ftype3::NF3DIR) {
                    return Err(E::Nfs(nfsstat3::NFS3ERR_NOTDIR));
                }
                self.dirs.insert(path.to_string(), h.clone());
                Ok(Some(h))
            }
            None => Ok(None),
        }
    }

    async fn parent(&mut self, path: &str) -> std::result::Result<(nfs_fh3, String), E> {
        let (d, n) = split(path);
        let h = self.dir(&d).await?.ok_or(E::Nfs(nfsstat3::NFS3ERR_NOENT))?;
        Ok((h, n))
    }

    async fn file(&mut self, path: &str) -> std::result::Result<nfs_fh3, E> {
        let (d, n) = self.parent(path).await?;
        Ok(self.lookup(&d, &n).await?.ok_or(E::Nfs(nfsstat3::NFS3ERR_NOENT))?.0)
    }
}

fn sattr(mode: u32) -> sattr3 {
    sattr3 { mode: Nfs3Option::Some(mode), ..Default::default() }
}

impl RemoteFs for NfsFs {
    fn stat(&self, path: &str) -> Result<Option<Meta>> {
        let path = path.to_string();
        self.with("stat", &path.clone(), true, move |s| {
            let path = path.clone();
            Box::pin(async move {
                if path == "/" {
                    let root = s.root.clone();
                    return Ok(Some(meta(&s.getattr(&root).await?)));
                }
                let (d, n) = split(&path);
                let Some(dh) = s.dir(&d).await? else { return Ok(None) };
                match s.lookup(&dh, &n).await? {
                    None => Ok(None),
                    Some((_, Some(a))) => Ok(Some(meta(&a))),
                    Some((h, None)) => Ok(Some(meta(&s.getattr(&h).await?))),
                }
            })
        })
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        let path = path.to_string();
        self.with("create folder", &path.clone(), true, move |s| {
            let path = path.clone();
            Box::pin(async move {
                let (d, n) = s.parent(&path).await?;
                let r = s.conn.mkdir(&nfs3::MKDIR3args { where_: diropargs3 { dir: d.clone(), name: name(&n) }, attributes: sattr(0o755) }).await?;
                match r {
                    Nfs3Result::Ok(_) => Ok(()),
                    Nfs3Result::Err((nfsstat3::NFS3ERR_EXIST, _)) => match s.lookup(&d, &n).await? {
                        Some((_, Some(a))) if matches!(a.type_, ftype3::NF3DIR) => Ok(()),
                        _ => Err(E::Nfs(nfsstat3::NFS3ERR_EXIST)),
                    },
                    Nfs3Result::Err((e, _)) => Err(E::Nfs(e)),
                }
            })
        })
    }

    fn write(&self, path: &str, src: &mut dyn Read) -> Result<u64> {
        // Attempted once (no reconnect): a retry would need the source again; the engine retries next run.
        let mut g = self.state.lock().unwrap();
        if g.is_none() {
            *g = Some(self.connect()?);
        }
        let s = g.as_mut().unwrap();
        let r: std::result::Result<u64, E> = self.rt.block_on(async {
            let (d, n) = s.parent(path).await?;
            let how = createhow3::UNCHECKED(sattr3 { mode: Nfs3Option::Some(0o644), size: Nfs3Option::Some(0), ..Default::default() });
            let created = st(s.conn.create(&nfs3::CREATE3args { where_: diropargs3 { dir: d.clone(), name: name(&n) }, how }).await?)?;
            let fh = match created.obj {
                Nfs3Option::Some(h) => h,
                Nfs3Option::None => s.lookup(&d, &n).await?.ok_or(E::Nfs(nfsstat3::NFS3ERR_NOENT))?.0,
            };
            let mut buf = vec![0u8; s.wsize as usize];
            let mut off = 0u64;
            loop {
                // Fill a whole chunk so each WRITE is full size.
                let mut k = 0;
                while k < buf.len() {
                    let r = src.read(&mut buf[k..]).map_err(|e| E::Other(format!("reading source: {e}")))?;
                    if r == 0 {
                        break;
                    }
                    k += r;
                }
                if k == 0 {
                    break;
                }
                let mut done = 0;
                while done < k {
                    let w = st(tokio::time::timeout(TIMEOUT, s.conn.write(&nfs3::WRITE3args {
                        file: fh.clone(),
                        offset: off + done as u64,
                        count: (k - done) as u32,
                        stable: stable_how::UNSTABLE,
                        data: Opaque::borrowed(&buf[done..k]),
                    }))
                    .await
                    .map_err(|_| E::Net("timed out".into()))??)?;
                    if w.count == 0 {
                        return Err(E::Other("the NAS accepted no data".into()));
                    }
                    done += w.count as usize;
                }
                off += k as u64;
            }
            // Make the data durable before Backer reads it back and renames it into place.
            st(s.conn.commit(&nfs3::COMMIT3args { file: fh, offset: 0, count: 0 }).await?)?;
            Ok(off)
        });
        match r {
            Ok(n) => Ok(n),
            Err(E::Net(m)) => {
                *g = None;
                Err(Error::DestinationNotReady(format!("lost the NFS connection to {} ({m})", self.host)))
            }
            Err(E::Nfs(st)) => Err(self.file_err("write", path, st)),
            Err(E::Other(m)) => Err(Error::Connector(format!("write {path}: {m}"))),
        }
    }

    fn read(&self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let mut g = self.state.lock().unwrap();
        if g.is_none() {
            *g = Some(self.connect()?);
        }
        let s = g.as_mut().unwrap();
        let r: std::result::Result<u64, E> = self.rt.block_on(async {
            let fh = s.file(path).await?;
            let mut off = 0u64;
            loop {
                let r = st(tokio::time::timeout(TIMEOUT, s.conn.read(&nfs3::READ3args { file: fh.clone(), offset: off, count: s.rsize }))
                    .await
                    .map_err(|_| E::Net("timed out".into()))??)?;
                let data = r.data.as_ref();
                out.write_all(data).map_err(|e| E::Other(e.to_string()))?;
                off += data.len() as u64;
                if r.eof || data.is_empty() {
                    break;
                }
            }
            Ok(off)
        });
        match r {
            Ok(n) => Ok(n),
            Err(E::Net(m)) => {
                *g = None;
                Err(Error::DestinationNotReady(format!("lost the NFS connection to {} ({m})", self.host)))
            }
            Err(E::Nfs(st)) => Err(self.file_err("read", path, st)),
            Err(E::Other(m)) => Err(Error::Connector(format!("read {path}: {m}"))),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (from.to_string(), to.to_string());
        self.with("rename", &to.clone(), true, move |s| {
            let (from, to) = (from.clone(), to.clone());
            Box::pin(async move {
                let (fd, fname) = s.parent(&from).await?;
                let (td, tname) = s.parent(&to).await?;
                // NFS RENAME replaces an existing target atomically.
                st(s.conn.rename(&nfs3::RENAME3args { from: diropargs3 { dir: fd, name: name(&fname) }, to: diropargs3 { dir: td, name: name(&tname) } }).await?)?;
                Ok(())
            })
        })
    }

    fn remove(&self, path: &str) -> Result<()> {
        let path = path.to_string();
        self.with("remove", &path.clone(), true, move |s| {
            let path = path.clone();
            Box::pin(async move {
                let (d, n) = s.parent(&path).await?;
                st(s.conn.remove(&nfs3::REMOVE3args { object: diropargs3 { dir: d, name: name(&n) } }).await?)?;
                Ok(())
            })
        })
    }

    fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        let path = path.to_string();
        self.with("list", &path.clone(), true, move |s| {
            let path = path.clone();
            Box::pin(async move {
                let dh = s.dir(&path).await?.ok_or(E::Nfs(nfsstat3::NFS3ERR_NOENT))?;
                let mut out = vec![];
                let mut cookie = 0;
                let mut verf = nfs3::cookieverf3::default();
                loop {
                    let r = st(s.conn.readdirplus(&nfs3::READDIRPLUS3args { dir: dh.clone(), cookie, cookieverf: verf, dircount: 64 * 1024, maxcount: 256 * 1024 }).await?)?;
                    verf = r.cookieverf;
                    let eof = r.reply.eof;
                    for e in r.reply.entries.into_inner() {
                        cookie = e.cookie;
                        let n = String::from_utf8_lossy(e.name.0.as_ref()).to_string();
                        if n == "." || n == ".." {
                            continue;
                        }
                        let m = match e.name_attributes {
                            Nfs3Option::Some(a) => meta(&a),
                            Nfs3Option::None => Meta { is_dir: false, size: 0, mtime: 0 },
                        };
                        out.push(DirEntry { name: n, is_dir: m.is_dir, size: if m.is_dir { 0 } else { m.size }, mtime: m.mtime });
                    }
                    if eof {
                        break;
                    }
                }
                Ok(out)
            })
        })
    }

    fn describe(&self) -> String {
        format!("nfs://{}{}", self.host, self.export)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_paths() {
        assert_eq!(split("/a"), ("/".into(), "a".into()));
        assert_eq!(split("/a/b/c"), ("/a/b".into(), "c".into()));
    }

    #[test]
    fn refused_port_says_service_is_off() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let fs = NfsFs::new(Destination::parse(&format!("nfs://127.0.0.1:{port}/export")).unwrap()).unwrap();
        let e = fs.stat("/").unwrap_err();
        assert!(matches!(e, Error::DestinationNotReady(_)), "{e}");
    }
}
