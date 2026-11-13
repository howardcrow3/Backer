//! Share listing for the SMB setup wizard: NetrShareEnum (MS-SRVS 3.1.4.8) over the `srvsvc` pipe,
//! encoded by hand in NDR (32-bit). The `smb` crate's own `list_shares` only offers NDR64, which
//! Samba (and so nearly every NAS) rejects.
//!
//! Only what the wizard needs: one bind, one request, level 1 (name, type, remark), one fragment.

const SRVSVC_UUID: [u8; 16] = uuid_le(0x4b324fc8, 0x1670, 0x01d3, [0x12, 0x78, 0x5a, 0x47, 0xbf, 0x6e, 0xe1, 0x88]);
const SRVSVC_VERSION: u32 = 3; // major 3, minor 0
const NDR32_UUID: [u8; 16] = uuid_le(0x8a885d04, 0x1ceb, 0x11c9, [0x9f, 0xe8, 0x08, 0x00, 0x2b, 0x10, 0x48, 0x60]);
const NDR32_VERSION: u32 = 2;
const FRAG: u16 = 16 * 1024;
const OPNUM_NETR_SHARE_ENUM: u16 = 15;
const STYPE_DISKTREE: u32 = 0;

const fn uuid_le(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

fn header(ptype: u8, body_len: usize, call_id: u32) -> Vec<u8> {
    let mut h = vec![5, 0, ptype, 0x03, 0x10, 0, 0, 0];
    h.extend(((16 + body_len) as u16).to_le_bytes());
    h.extend(0u16.to_le_bytes()); // no auth
    h.extend(call_id.to_le_bytes());
    h
}

pub fn bind_pdu() -> Vec<u8> {
    let mut b = vec![];
    b.extend(FRAG.to_le_bytes());
    b.extend(FRAG.to_le_bytes());
    b.extend(0u32.to_le_bytes()); // new association group
    b.extend([1, 0, 0, 0]); // one context element
    b.extend(0u16.to_le_bytes()); // context id 0
    b.extend([1, 0]); // one transfer syntax
    b.extend(SRVSVC_UUID);
    b.extend(SRVSVC_VERSION.to_le_bytes());
    b.extend(NDR32_UUID);
    b.extend(NDR32_VERSION.to_le_bytes());
    let mut p = header(11, b.len(), 1);
    p.extend(b);
    p
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

/// A [string, unique] wchar_t* argument: referent, then a conformant varying string with its terminator.
fn put_wstr(v: &mut Vec<u8>, referent: u32, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    v.extend(referent.to_le_bytes());
    v.extend((w.len() as u32).to_le_bytes());
    v.extend(0u32.to_le_bytes());
    v.extend((w.len() as u32).to_le_bytes());
    for c in w {
        v.extend(c.to_le_bytes());
    }
    pad4(v);
}

pub fn share_enum_pdu(server: &str) -> Vec<u8> {
    let mut stub = vec![];
    put_wstr(&mut stub, 0x0002_0000, &format!("\\\\{server}"));
    stub.extend(1u32.to_le_bytes()); // Level 1
    stub.extend(1u32.to_le_bytes()); // union switch = 1
    stub.extend(0x0002_0004u32.to_le_bytes()); // pointer to SHARE_INFO_1_CONTAINER
    stub.extend(0u32.to_le_bytes()); // EntriesRead
    stub.extend(0u32.to_le_bytes()); // Buffer = NULL
    stub.extend(u32::MAX.to_le_bytes()); // PreferedMaximumLength: everything
    stub.extend(0x0002_0008u32.to_le_bytes()); // ResumeHandle pointer
    stub.extend(0u32.to_le_bytes()); // ResumeHandle = 0
    let mut body = vec![];
    body.extend((stub.len() as u32).to_le_bytes()); // alloc hint
    body.extend(0u16.to_le_bytes()); // context id
    body.extend(OPNUM_NETR_SHARE_ENUM.to_le_bytes());
    body.extend(stub);
    let mut p = header(0, body.len(), 2);
    p.extend(body);
    p
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}
impl Rd<'_> {
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes(self.b.get(self.at..self.at + 4)?.try_into().ok()?);
        self.at += 4;
        Some(v)
    }
    fn wstr(&mut self) -> Option<String> {
        let _max = self.u32()?;
        let _off = self.u32()?;
        let n = self.u32()? as usize;
        if n > 4096 {
            return None;
        }
        let raw = self.b.get(self.at..self.at + n * 2)?;
        self.at += n * 2;
        self.at = (self.at + 3) & !3;
        let w: Vec<u16> = raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
        Some(String::from_utf16_lossy(&w))
    }
}

fn check_header(p: &[u8], want: u8) -> Result<(), String> {
    if p.len() < 16 || p[0] != 5 {
        return Err("not a DCE/RPC reply".into());
    }
    if p[2] == 3 {
        let status = p.get(24..28).map(|s| u32::from_le_bytes(s.try_into().unwrap())).unwrap_or(0);
        return Err(format!("the server refused the share list (fault {status:#x})"));
    }
    if p[2] != want {
        return Err(format!("unexpected DCE/RPC packet type {}", p[2]));
    }
    if p[3] & 0x03 != 0x03 {
        return Err("the share list does not fit in one reply".into());
    }
    Ok(())
}

pub fn check_bind_ack(p: &[u8]) -> Result<(), String> {
    check_header(p, 12)?;
    let mut at = 16 + 8;
    let sec_len = u16::from_le_bytes(p.get(at..at + 2).ok_or("short bind ack")?.try_into().unwrap()) as usize;
    at += 2 + sec_len;
    at = (at + 3) & !3;
    let n = *p.get(at).ok_or("short bind ack")?;
    at += 4;
    if n < 1 {
        return Err("bind rejected".into());
    }
    let result = u16::from_le_bytes(p.get(at..at + 2).ok_or("short bind ack")?.try_into().unwrap());
    if result != 0 {
        return Err(format!("the server does not accept NDR for srvsvc (result {result})"));
    }
    Ok(())
}

/// Disk share names from a NetrShareEnum response, hidden `$` shares left out.
pub fn parse_share_enum(p: &[u8]) -> Result<Vec<String>, String> {
    check_header(p, 2)?;
    let bad = || "malformed share list".to_string();
    let mut r = Rd { b: p, at: 24 };
    let _level = r.u32().ok_or_else(bad)?;
    let _switch = r.u32().ok_or_else(bad)?;
    if r.u32().ok_or_else(bad)? == 0 {
        return Ok(vec![]);
    }
    let count = r.u32().ok_or_else(bad)? as usize;
    if r.u32().ok_or_else(bad)? == 0 || count > 10_000 {
        return Ok(vec![]);
    }
    let max = r.u32().ok_or_else(bad)? as usize;
    if max < count {
        return Err(bad());
    }
    let mut fixed = vec![];
    for _ in 0..count {
        let name_ptr = r.u32().ok_or_else(bad)?;
        let ty = r.u32().ok_or_else(bad)?;
        let remark_ptr = r.u32().ok_or_else(bad)?;
        fixed.push((name_ptr, ty, remark_ptr));
    }
    let mut out = vec![];
    for (name_ptr, ty, remark_ptr) in fixed {
        let name = if name_ptr != 0 { r.wstr().ok_or_else(bad)? } else { String::new() };
        if remark_ptr != 0 {
            r.wstr().ok_or_else(bad)?;
        }
        if ty & 0x0FFF_FFFF == STYPE_DISKTREE && ty & 0x8000_0000 == 0 && !name.is_empty() && !name.ends_with('$') {
            out.push(name);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_little_endian() {
        assert_eq!(&SRVSVC_UUID[..4], &[0xc8, 0x4f, 0x32, 0x4b]);
        assert_eq!(&NDR32_UUID[..8], &[0x04, 0x5d, 0x88, 0x8a, 0xeb, 0x1c, 0xc9, 0x11]);
    }

    #[test]
    fn pdus_have_correct_lengths() {
        let b = bind_pdu();
        assert_eq!(u16::from_le_bytes([b[8], b[9]]) as usize, b.len());
        assert_eq!(b.len(), 72);
        let r = share_enum_pdu("nas");
        assert_eq!(u16::from_le_bytes([r[8], r[9]]) as usize, r.len());
        assert!(r.len().is_multiple_of(4));
    }

    fn wstr(v: &mut Vec<u8>, s: &str) {
        let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        for x in [w.len() as u32, 0, w.len() as u32] {
            v.extend(x.to_le_bytes());
        }
        for c in w {
            v.extend(c.to_le_bytes());
        }
        pad4(v);
    }

    #[test]
    fn parses_a_samba_style_reply() {
        let shares = [("backups", 0u32, "Backups"), ("IPC$", 0x8000_0003, "IPC Service"), ("photo", 0, ""), ("admin$", 0x8000_0000, "")];
        let mut stub = vec![];
        for x in [1u32, 1, 0x20000, shares.len() as u32, 0x20004, shares.len() as u32] {
            stub.extend(x.to_le_bytes());
        }
        for (i, (_, t, _)) in shares.iter().enumerate() {
            for x in [0x20008 + i as u32 * 8, *t, 0x2000c + i as u32 * 8] {
                stub.extend(x.to_le_bytes());
            }
        }
        for (n, _, rmk) in shares {
            wstr(&mut stub, n);
            wstr(&mut stub, rmk);
        }
        for x in [4u32, 0, 0, 0] {
            stub.extend(x.to_le_bytes());
        }
        let mut body = vec![0u8; 8];
        body.extend(stub);
        let mut p = header(2, body.len(), 2);
        p.extend(body);
        assert_eq!(parse_share_enum(&p).unwrap(), vec!["backups".to_string(), "photo".to_string()]);
        assert!(parse_share_enum(&p[..40]).is_err());
    }
}
