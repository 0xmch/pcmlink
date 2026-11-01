//! What to call a machine, given only the address its packets come from.
//!
//! The mixer tries, in order: the name you already use for it (an alias in
//! ~/.ssh/config), the name its agent reports, its name on the local network
//! (multicast DNS), and reverse DNS. Nothing here has to be set up for pcmlink.
//!
//! Multicast DNS is asked directly because the system resolver does not use it for
//! ordinary private addresses, only for link-local ones. Not every machine answers
//! a reverse question: Avahi and macOS do, Windows does not (but it can run an agent).

use std::fs;
use std::net::{Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MDNS_GROUP: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 251), 5353);
const MDNS_WAIT: Duration = Duration::from_millis(800);
const TYPE_PTR: u16 = 12;
const MAX_INCLUDES: usize = 8;

/// The Host entries of an ssh config: each one's first alias and where it points.
#[derive(Default)]
pub struct SshHosts(Vec<(String, String)>);

impl SshHosts {
    pub fn load() -> SshHosts {
        let mut hosts = SshHosts::default();
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            hosts.read_file(&home.join(".ssh/config"), &home, 0);
        }
        hosts
    }

    fn read_file(&mut self, path: &Path, home: &Path, depth: usize) {
        if depth <= MAX_INCLUDES {
            if let Ok(text) = fs::read_to_string(path) {
                self.parse(&text, home, depth);
            }
        }
    }

    fn parse(&mut self, text: &str, home: &Path, depth: usize) {
        // The current block's first usable alias, until its HostName is seen.
        let mut alias: Option<String> = None;
        let mut hostname: Option<String> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once(|c: char| c.is_whitespace() || c == '=').unwrap_or((line, ""));
            let value = value.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim_matches('"');
            match key.to_ascii_lowercase().as_str() {
                "host" | "match" => {
                    self.finish(&mut alias, &mut hostname);
                    if key.eq_ignore_ascii_case("host") {
                        alias = value.split_whitespace().find(|a| !a.contains(['*', '?', '!'])).map(str::to_string);
                    }
                }
                "hostname" if hostname.is_none() => hostname = Some(value.to_string()),
                "include" => {
                    for pattern in value.split_whitespace() {
                        for path in expand_include(pattern, home) {
                            self.read_file(&path, home, depth + 1);
                        }
                    }
                }
                _ => {}
            }
        }
        self.finish(&mut alias, &mut hostname);
    }

    fn finish(&mut self, alias: &mut Option<String>, hostname: &mut Option<String>) {
        if let Some(alias) = alias.take() {
            // With no HostName, ssh connects to the alias itself.
            let target = hostname.take().unwrap_or_else(|| alias.clone());
            if target != alias {
                self.0.push((alias, target));
            }
        }
        *hostname = None;
    }

    /// The alias pointing at this address or name.
    pub fn alias_of(&self, target: &str) -> Option<String> {
        let target = target.trim_end_matches('.');
        self.0
            .iter()
            .find(|(_, t)| t.trim_end_matches('.').eq_ignore_ascii_case(target))
            .map(|(alias, _)| alias.clone())
    }
}

/// An Include's files. ssh takes globs; a star in the file name covers real use.
fn expand_include(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if pattern.starts_with('/') => PathBuf::from(pattern),
        None => home.join(".ssh").join(pattern),
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some((before, after)) = name.split_once('*') else { return vec![path] };
    let Some(dir) = path.parent() else { return Vec::new() };
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(before) && n.ends_with(after)
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// `norns.local.` and `pc.home.lan` are `norns` and `pc` on a mixer.
pub fn short_name(name: &str) -> String {
    name.split('.').next().filter(|s| !s.is_empty()).unwrap_or(name).to_string()
}

fn reverse_labels(ip: Ipv4Addr) -> String {
    let [a, b, c, d] = ip.octets();
    format!("{d}.{c}.{b}.{a}.in-addr.arpa")
}

/// A DNS question asking for the PTR record of `ip`, wanting a direct reply.
fn ptr_question(ip: Ipv4Addr) -> Vec<u8> {
    let mut packet = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]; // header: one question
    for label in reverse_labels(ip).split('.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&TYPE_PTR.to_be_bytes());
    packet.extend_from_slice(&0x8001u16.to_be_bytes()); // class IN, unicast response
    packet
}

/// A name at `start`, following compression pointers. Also returns where the
/// record continues. This reads whatever the network sends, so every access is
/// checked and pointers can only go a limited number of times.
fn read_name(packet: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut at = start;
    let mut next = None;
    for _ in 0..32 {
        let len = *packet.get(at)? as usize;
        match len {
            0 => {
                return Some((labels.join("."), next.unwrap_or(at + 1)));
            }
            l if l & 0xc0 == 0xc0 => {
                next.get_or_insert(at + 2);
                at = ((l & 0x3f) << 8) | *packet.get(at + 1)? as usize;
            }
            l if l <= 63 => {
                labels.push(String::from_utf8_lossy(packet.get(at + 1..at + 1 + l)?).into_owned());
                at += 1 + l;
            }
            _ => return None,
        }
    }
    None
}

/// The PTR target for `ip` in a DNS message, if it has one.
fn ptr_answer(packet: &[u8], ip: Ipv4Addr) -> Option<String> {
    let count = |at: usize| Some(u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]) as usize);
    let (questions, answers, authority, additional) = (count(4)?, count(6)?, count(8)?, count(10)?);
    let wanted = reverse_labels(ip);
    let mut at = 12;
    for _ in 0..questions {
        at = read_name(packet, at)?.1 + 4;
    }
    for _ in 0..answers + authority + additional {
        let (name, after) = read_name(packet, at)?;
        let kind = u16::from_be_bytes([*packet.get(after)?, *packet.get(after + 1)?]);
        let length = u16::from_be_bytes([*packet.get(after + 8)?, *packet.get(after + 9)?]) as usize;
        let data = after + 10;
        if kind == TYPE_PTR && name.eq_ignore_ascii_case(&wanted) {
            let (target, _) = read_name(packet, data)?;
            return (!target.is_empty()).then_some(target);
        }
        at = data + length;
    }
    None
}

/// Ask the local network what `ip` is called.
pub fn mdns_reverse(ip: Ipv4Addr) -> Option<String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.set_multicast_ttl_v4(255).ok()?;
    socket.send_to(&ptr_question(ip), MDNS_GROUP).ok()?;
    let deadline = Instant::now() + MDNS_WAIT;
    let mut buffer = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        socket.set_read_timeout(Some(left)).ok()?;
        let (length, _) = socket.recv_from(&mut buffer).ok()?;
        if let Some(name) = ptr_answer(&buffer[..length], ip) {
            return Some(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(text: &str) -> SshHosts {
        let mut hosts = SshHosts::default();
        hosts.parse(text, Path::new("/nonexistent"), 0);
        hosts
    }

    const CONFIG: &str = "\
# a comment
Host *
    ServerAliveInterval 30

Host pc linux
    User someone
    HostName 192.0.2.5

host=box
  hostname=box.local

Host *.example !bad good
    HostName 192.0.2.9

Host named-only

Match host foo
    HostName 192.0.2.77

Host last
    HostName \"192.0.2.8\"
";

    #[test]
    fn an_alias_comes_from_the_host_line_above_its_address() {
        let h = hosts(CONFIG);
        assert_eq!(h.alias_of("192.0.2.5").as_deref(), Some("pc"));
        assert_eq!(h.alias_of("192.0.2.8").as_deref(), Some("last"));
    }

    #[test]
    fn local_names_match_whatever_their_case_or_final_dot() {
        let h = hosts(CONFIG);
        assert_eq!(h.alias_of("BOX.local.").as_deref(), Some("box"));
    }

    #[test]
    fn patterns_and_match_blocks_are_not_names() {
        let h = hosts(CONFIG);
        assert_eq!(h.alias_of("192.0.2.9").as_deref(), Some("good"));
        assert_eq!(h.alias_of("192.0.2.77"), None);
        assert_eq!(h.alias_of("192.0.2.200"), None);
    }

    #[test]
    fn includes_are_followed_and_cannot_loop() {
        let dir = std::env::temp_dir().join(format!("pcmlink-names-test-{}", std::process::id()));
        fs::create_dir_all(dir.join(".ssh/conf.d")).unwrap();
        fs::write(dir.join(".ssh/conf.d/a.conf"), "Host included\n  HostName 192.0.2.60\n").unwrap();
        fs::write(dir.join(".ssh/loop"), "Include ~/.ssh/loop\nHost looped\n  HostName 192.0.2.61\n").unwrap();
        let mut h = SshHosts::default();
        h.parse("Include conf.d/*.conf\nInclude ~/.ssh/loop\n", &dir, 0);
        assert_eq!(h.alias_of("192.0.2.60").as_deref(), Some("included"));
        assert_eq!(h.alias_of("192.0.2.61").as_deref(), Some("looped"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_lose_their_domain() {
        assert_eq!(short_name("norns.local."), "norns");
        assert_eq!(short_name("plain"), "plain");
        assert_eq!(short_name("."), ".");
    }

    const IP: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 5);

    /// A reply as Avahi sends it: the question echoed, then the answer.
    fn reply() -> Vec<u8> {
        let mut p = vec![0, 0, 0x84, 0, 0, 1, 0, 1, 0, 0, 0, 0];
        let question = ptr_question(IP);
        p.extend_from_slice(&question[12..]);
        p.extend_from_slice(&[0xc0, 12]); // the name again, by pointer
        p.extend_from_slice(&TYPE_PTR.to_be_bytes());
        p.extend_from_slice(&[0x80, 1, 0, 0, 0, 120]); // class, ttl
        let target = b"\x05norns\x05local\x00";
        p.extend_from_slice(&(target.len() as u16).to_be_bytes());
        p.extend_from_slice(target);
        p
    }

    #[test]
    fn reads_the_name_from_an_mdns_reply() {
        assert_eq!(ptr_answer(&reply(), IP).as_deref(), Some("norns.local"));
    }

    #[test]
    fn a_reply_about_another_address_is_ignored() {
        assert_eq!(ptr_answer(&reply(), Ipv4Addr::new(192, 0, 2, 6)), None);
    }

    #[test]
    fn garbage_from_the_network_is_only_ever_no_answer() {
        assert_eq!(ptr_answer(&[], IP), None);
        assert_eq!(ptr_answer(&[0; 11], IP), None);
        // A pointer to itself.
        let mut looped = reply();
        looped[12] = 0xc0;
        looped[13] = 12;
        assert_eq!(ptr_answer(&looped, IP), None);
        // Every truncation of a real reply.
        let whole = reply();
        for end in 0..whole.len() {
            assert_eq!(ptr_answer(&whole[..end], IP), None);
        }
        // Counts claiming far more records than there are.
        let mut lying = reply();
        lying[6] = 0xff;
        lying[7] = 0xff;
        lying[46] = 0xff; // and a record length past the end
        let _ = ptr_answer(&lying, IP);
    }
}
