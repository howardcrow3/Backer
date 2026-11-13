//! Integration tests: every protocol runs the same checks against a real server.
//!
//! NFS always runs, against an in-process NFSv3 server. SFTP, SMB and a kernel NFS server run when
//! their address is set (CI starts them in containers; see scripts/nas-test-servers.sh):
//!
//! - `BACKER_TEST_SFTP=sftp://user@127.0.0.1:2222/upload` and `BACKER_TEST_SFTP_PASSWORD`
//! - `BACKER_TEST_SMB=smb://user@127.0.0.1:1445/share` and `BACKER_TEST_SMB_PASSWORD`
//! - `BACKER_TEST_NFS=nfs://127.0.0.1/export?uid=0&gid=0`
//!
//! Set `BACKER_REQUIRE_SERVERS=sftp,smb,nfs` to fail instead of skipping when one of those is missing (CI does).
use backer_connector::{hostkeys, nfs, open, sftp, smb, Access, Destination, Nas, Secret};
use backer_core::{Error, DEST_MARKER};
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

static UNIQUE: AtomicU32 = AtomicU32::new(0);

/// A fresh folder name so runs against a shared server do not collide.
fn unique(tag: &str) -> String {
    format!("backer-it-{tag}-{}-{}", std::process::id(), UNIQUE.fetch_add(1, Ordering::SeqCst))
}

fn server(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            let tag = var.trim_start_matches("BACKER_TEST_").to_lowercase();
            if std::env::var("BACKER_REQUIRE_SERVERS").unwrap_or_default().split(',').any(|t| t.trim() == tag) {
                panic!("{var} is not set but BACKER_REQUIRE_SERVERS includes {tag}");
            }
            eprintln!("skipping: {var} is not set");
            None
        }
    }
}

fn src(bytes: &[u8]) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    fs::write(f.path(), bytes).unwrap();
    f
}

/// The checks every protocol must pass. `root` is a connector on the protocol root; the test works in a
/// new subfolder of it, so `dest` opens the subfolder.
fn exercise(root: &dyn Nas, dest: &Destination, access: &Access, sub: &str) {
    root.reachable().unwrap();
    root.check_writable().unwrap();
    // FR-2: create a new destination folder from the picker.
    root.create_dir(sub).unwrap();
    assert!(root.list_dir("").unwrap().iter().any(|e| e.name == sub && e.is_dir));

    let c = open(dest, access.clone());
    // No marker yet: runs are refused.
    assert!(matches!(c.check_ready(), Err(Error::DestinationNotReady(_))));
    c.init_marker().unwrap();
    c.check_ready().unwrap();

    // Copy, verify, stat and read back.
    let a = src(b"hello from backer");
    let put = c.put_file(a.path(), "Docs/2026/notes.txt", 1, 1).unwrap();
    assert_eq!(put.size, 17);
    assert_eq!(put.hash, blake3::hash(b"hello from backer").to_hex().to_string());
    assert_eq!(c.stat("Docs/2026/notes.txt").unwrap().unwrap().size, 17);
    assert_eq!(c.hash("Docs/2026/notes.txt").unwrap(), put.hash);
    let mut back = vec![];
    c.read_file("Docs/2026/notes.txt", &mut back).unwrap();
    assert_eq!(back, b"hello from backer");
    assert!(c.stat("Docs/missing.txt").unwrap().is_none());

    // Replacing an existing file (rename over it) works and leaves no temp files.
    let b = src(b"second version, longer");
    assert_eq!(c.put_file(b.path(), "Docs/2026/notes.txt", 2, 1).unwrap().size, 22);
    assert!(c.list_temp_files("Docs/2026").unwrap().is_empty());

    // A larger file crosses several protocol-level chunks.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let bf = src(&big);
    let p = c.put_file(bf.path(), "Docs/big.bin", 3, 1).unwrap();
    assert_eq!(p.size, big.len() as u64);
    assert_eq!(p.hash, blake3::hash(&big).to_hex().to_string());

    // Names with spaces and non-ASCII characters.
    let u = src(b"x");
    c.put_file(u.path(), "Fotos/Ålesund trip/Café 1.jpg", 4, 1).unwrap();
    assert!(c.stat("Fotos/Ålesund trip/Café 1.jpg").unwrap().is_some());

    // Browsing hides Backer's own files and lists folders first.
    let names: Vec<_> = c.list_dir("").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec!["Docs".to_string(), "Fotos".to_string()]);
    assert!(!names.iter().any(|n| n == DEST_MARKER));
    assert!(c.list_dir("nowhere").unwrap().is_empty());

    // Many small files in one folder (folder creation is cached, listing pages through).
    let s = src(b"small");
    for i in 0..150 {
        c.put_file(s.path(), &format!("Many/f{i:04}.txt"), 5, i).unwrap();
    }
    assert_eq!(c.list_dir("Many").unwrap().len(), 150);

    // Leftover temp files are found and removed; user files never are.
    assert!(c.remove_temp("Docs/big.bin").is_err());
    assert!(c.stat("Docs/big.bin").unwrap().is_some());

    // Escaping the destination is refused.
    assert!(c.stat("../x").is_err());
    assert!(c.stat("/etc/passwd").is_err());
}

/// Erase is only for backer-removal (and these tests): it requires the marker.
fn erase_needs_marker(dest: &Destination, access: &Access) {
    let c = open(dest, access.clone());
    c.erase("Docs/big.bin").unwrap();
    assert!(c.stat("Docs/big.bin").unwrap().is_none());
}

#[test]
fn nfs_in_process_server() {
    use nfs3_server::memfs::{MemFs, MemFsConfig};
    use nfs3_server::tcp::{NFSTcp, NFSTcpListener};
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut cfg = MemFsConfig::default();
    cfg.add_dir("/backups");
    let listener = rt.block_on(NFSTcpListener::bind("127.0.0.1:0", MemFs::new(cfg).unwrap())).unwrap();
    let port = listener.get_listen_port();
    rt.spawn(async move { listener.handle_forever().await });

    let url = format!("nfs://127.0.0.1:{port}/backups?export=/&uid=501&gid=20");
    let root = open(&Destination::parse(&url).unwrap(), Access::default());
    let dest = Destination::parse(&url).unwrap().with_path("/backups/run").unwrap();
    exercise(&*root, &dest, &Access::default(), "run");
    erase_needs_marker(&dest, &Access::default());
    assert!(nfs::list_exports("127.0.0.1", Some(port)).unwrap().contains(&"/".to_string()));
}

#[test]
fn nfs_kernel_server() {
    let Some(url) = server("BACKER_TEST_NFS") else { return };
    let base = Destination::parse(&url).unwrap();
    let root = open(&base, Access::default());
    let sub = unique("nfs");
    let dest = base.with_path(&format!("{}/{sub}", base.base_path().unwrap())).unwrap();
    exercise(&*root, &dest, &Access::default(), &sub);
    erase_needs_marker(&dest, &Access::default());
    let host = base.host().unwrap();
    let exports = nfs::list_exports(host, None).unwrap();
    assert!(!exports.is_empty(), "the server lists its exports");
}

fn sftp_access(dest: &Destination, password: &str) -> Access {
    let Destination::Sftp { host, port, .. } = dest else { unreachable!() };
    let fp = sftp::probe_host_key(host, *port).unwrap();
    assert!(fp.starts_with("SHA256:"));
    Access { secret: Some(Secret::Password(password.into())), host_key: Some(fp) }
}

#[test]
fn sftp_server() {
    let Some(url) = server("BACKER_TEST_SFTP") else { return };
    let pw = std::env::var("BACKER_TEST_SFTP_PASSWORD").unwrap_or_default();
    let base = Destination::parse(&url).unwrap();
    let access = sftp_access(&base, &pw);
    let root = open(&base, access.clone());
    let sub = unique("sftp");
    let dest = base.with_path(&format!("{}/{sub}", base.base_path().unwrap())).unwrap();
    exercise(&*root, &dest, &access, &sub);
    erase_needs_marker(&dest, &access);

    // A key pin store survives restarts.
    let d = tempfile::tempdir().unwrap();
    let keys = hostkeys::HostKeys::new(d.path().join("host-keys.json"));
    keys.pin(&base.host_key_id().unwrap(), access.host_key.as_deref().unwrap()).unwrap();
    assert_eq!(keys.get(&base.host_key_id().unwrap()), access.host_key);
}

#[test]
fn sftp_rejects_bad_password_without_retrying() {
    let Some(url) = server("BACKER_TEST_SFTP") else { return };
    let base = Destination::parse(&url).unwrap();
    let mut access = sftp_access(&base, "definitely-wrong");
    let c = open(&base, access.clone());
    assert!(matches!(c.reachable(), Err(Error::Credentials(_))), "bad password is a credentials error");
    access.secret = None;
    assert!(matches!(open(&base, access).reachable(), Err(Error::Credentials(_))));
}

#[test]
fn sftp_blocks_changed_or_unknown_host_key() {
    let Some(url) = server("BACKER_TEST_SFTP") else { return };
    let pw = std::env::var("BACKER_TEST_SFTP_PASSWORD").unwrap_or_default();
    let base = Destination::parse(&url).unwrap();
    let wrong = Access { secret: Some(Secret::Password(pw.clone())), host_key: Some("SHA256:notthekey".into()) };
    let e = open(&base, wrong).reachable().unwrap_err();
    assert!(matches!(e, Error::HostKey(_)), "{e}");
    let unknown = Access { secret: Some(Secret::Password(pw)), host_key: None };
    assert!(matches!(open(&base, unknown).reachable(), Err(Error::HostKey(_))));
}

#[test]
fn smb_server() {
    let Some(url) = server("BACKER_TEST_SMB") else { return };
    let pw = std::env::var("BACKER_TEST_SMB_PASSWORD").unwrap_or_default();
    let base = Destination::parse(&url).unwrap();
    let access = Access { secret: Some(Secret::Password(pw.clone())), host_key: None };
    let root = open(&base, access.clone());
    let sub = unique("smb");
    let dest = base.with_path(&format!("{}/{sub}", base.base_path().unwrap())).unwrap();
    exercise(&*root, &dest, &access, &sub);
    erase_needs_marker(&dest, &access);

    // Share browsing for the wizard.
    let Destination::Smb { host, port, user, share, .. } = &base else { unreachable!() };
    let shares = smb::list_shares(host, *port, user, &access).unwrap();
    assert!(shares.contains(share), "{shares:?}");
}

#[test]
fn smb_rejects_bad_password_and_missing_share() {
    let Some(url) = server("BACKER_TEST_SMB") else { return };
    let pw = std::env::var("BACKER_TEST_SMB_PASSWORD").unwrap_or_default();
    let base = Destination::parse(&url).unwrap();
    let bad = Access { secret: Some(Secret::Password("definitely-wrong".into())), host_key: None };
    let e = open(&base, bad).reachable().unwrap_err();
    assert!(matches!(e, Error::Credentials(_)), "{e}");
    let Destination::Smb { host, port, user, .. } = &base else { unreachable!() };
    let missing = Destination::parse(&format!("smb://{user}@{host}:{port}/no-such-share")).unwrap();
    let e = open(&missing, Access { secret: Some(Secret::Password(pw)), host_key: None }).reachable().unwrap_err();
    assert!(e.to_string().contains("not found"), "{e}");
}
