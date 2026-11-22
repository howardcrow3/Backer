//! A stable identifier for this computer, used to count "2 computers per key".
//!
//! Uses the operating system's own machine id (which survives app reinstalls and updates), falling back to a random id
//! saved by the caller when the OS gives none. The raw id never leaves the computer; `backer_license::activation::machine_fingerprint`
//! hashes it first.

use std::process::Command;

/// The OS machine id, or None when it can't be read.
pub fn os_machine_id() -> Option<String> {
    let id = read();
    id.map(|s| s.trim().to_string()).filter(|s| s.len() >= 8)
}

#[cfg(target_os = "macos")]
fn read() -> Option<String> {
    let out = Command::new("/usr/sbin/ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output().ok()?;
    parse_ioreg(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "windows")]
fn read() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = Command::new("reg").args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    parse_reg(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read() -> Option<String> {
    let _ = Command::new("true");
    std::fs::read_to_string("/etc/machine-id").or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id")).ok()
}

#[allow(dead_code)]
fn parse_ioreg(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains("IOPlatformUUID"))?;
    line.split('"').nth(3).map(str::to_string)
}

#[allow(dead_code)]
fn parse_reg(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains("MachineGuid"))?;
    line.split_whitespace().last().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_id_out_of_ioreg_and_reg_output() {
        let io = "+-o Root  <class IORegistryEntry>\n    \"IOPlatformUUID\" = \"8F2C1D3E-1234-5678-9ABC-DEF012345678\"\n    \"IOPlatformSerialNumber\" = \"X\"\n";
        assert_eq!(parse_ioreg(io).as_deref(), Some("8F2C1D3E-1234-5678-9ABC-DEF012345678"));
        let reg = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\r\n    MachineGuid    REG_SZ    4c4c4544-0042-3010-8051-b8c04f4d4e32\r\n\r\n";
        assert_eq!(parse_reg(reg).as_deref(), Some("4c4c4544-0042-3010-8051-b8c04f4d4e32"));
        assert_eq!(parse_ioreg("nothing here"), None);
    }
}
