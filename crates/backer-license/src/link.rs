//! `backer://activate?key=<license key>` links, as put in the purchase email. The operating system hands the link
//! to Backer, which activates the license without any copying and pasting. Parsing is strict: only the activation
//! action and a plain key (letters, digits and dashes) get through; the key itself is checked later by the agent.

const MAX_LEN: usize = 512;

fn plain_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The license key inside one activation link, or None for any other link.
pub fn key_from_link(link: &str) -> Option<String> {
    let rest = link.trim().strip_prefix("backer://")?;
    let (action, query) = rest.split_once('?')?;
    if action.trim_end_matches('/') != "activate" {
        return None;
    }
    let query = query.split('#').next().unwrap_or("");
    query.split('&').find_map(|kv| kv.strip_prefix("key=")).filter(|k| plain_key(k)).map(str::to_string)
}

/// The first activation key among the links the OS passed in.
pub fn key_from_links<S: AsRef<str>>(links: &[S]) -> Option<String> {
    links.iter().find_map(|l| key_from_link(l.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    const K: &str = "BACKER-ABCDE-FGH23-45672";

    #[test]
    fn an_activation_link_yields_its_key() {
        assert_eq!(key_from_link(&format!("backer://activate?key={K}")).as_deref(), Some(K));
        assert_eq!(key_from_link(&format!("backer://activate/?key={K}")).as_deref(), Some(K));
        assert_eq!(key_from_link(&format!("  backer://activate?x=1&key={K}#frag ")).as_deref(), Some(K));
        assert_eq!(key_from_links(&["https://x", &format!("backer://activate?key={K}")]).as_deref(), Some(K));
    }

    #[test]
    fn anything_else_is_ignored() {
        for bad in ["", "backer://activate", "backer://activate?key=", "backer://open?key=BACKER-AAAA", "https://activate?key=BACKER-AAAA",
            "backer://activate?key=BACKER%2DAAAA", "backer://activate?key=a b", "backer://activate?key=a;rm"] {
            assert_eq!(key_from_link(bad), None, "{bad}");
        }
        assert_eq!(key_from_link(&format!("backer://activate?key={}", "A".repeat(513))), None);
    }
}
