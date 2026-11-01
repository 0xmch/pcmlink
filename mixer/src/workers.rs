//! The threads that talk to receivers and agents. Each owns one blocking
//! connection and posts what it reads to the main loop, which alone changes the
//! model, so a slow or dead peer can never hold up the screen.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::mem;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::names;

/// How long before a socket that refused a connection is tried again.
const RECEIVER_RETRY: Duration = Duration::from_secs(1);
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
const AGENT_RETRY: Duration = Duration::from_secs(4);

pub enum Msg {
    StreamUp(PathBuf, UnixStream),
    StreamStatus(PathBuf, Value),
    StreamGone(PathBuf),
    AgentUp(IpAddr, TcpStream),
    AgentLine(IpAddr, Value),
    AgentGone(IpAddr),
    Resolved(IpAddr, String),
}

/// Where receivers publish their sockets. The same rule as pcmlink-rx.
pub fn default_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PCMLINK_RX_DIR") {
        return dir.into();
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/pcmlink/rx")
    } else {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or(home.join(".cache"));
        base.join("pcmlink/rx")
    }
}

/// Finds receivers: every socket in the directory is one.
pub struct Discovery {
    dir: PathBuf,
    connected: HashSet<PathBuf>,
    refused_until: HashMap<PathBuf, Instant>,
    tx: Sender<Msg>,
}

impl Discovery {
    pub fn new(dir: PathBuf, tx: Sender<Msg>) -> Discovery {
        Discovery { dir, connected: HashSet::new(), refused_until: HashMap::new(), tx }
    }

    pub fn forget(&mut self, path: &PathBuf) {
        self.connected.remove(path);
        self.refused_until.insert(path.clone(), Instant::now() + RECEIVER_RETRY);
    }

    pub fn scan(&mut self) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "sock") || self.connected.contains(&path) {
                continue;
            }
            if self.refused_until.get(&path).is_some_and(|t| Instant::now() < *t) {
                continue;
            }
            // A receiver that is gone leaves its socket behind until the next one
            // on that port takes it over: refused, and harmless.
            match UnixStream::connect(&path) {
                Ok(stream) => {
                    self.connected.insert(path.clone());
                    self.refused_until.remove(&path);
                    spawn_stream(path, stream, self.tx.clone());
                }
                Err(_) => {
                    self.refused_until.insert(path, Instant::now() + RECEIVER_RETRY);
                }
            }
        }
    }
}

fn spawn_stream(path: PathBuf, stream: UnixStream, tx: Sender<Msg>) {
    thread::spawn(move || {
        if let Ok(writer) = stream.try_clone() {
            let _ = tx.send(Msg::StreamUp(path.clone(), writer));
        }
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if tx.send(Msg::StreamStatus(path.clone(), value)).is_err() {
                    return;
                }
            }
        }
        let _ = tx.send(Msg::StreamGone(path));
    });
}

/// Keep a connection to the agent at `ip`, if there is one, for as long as this
/// program runs. A machine with no agent costs one refused connection every few
/// seconds.
pub fn spawn_agent(ip: IpAddr, port: u16, tx: Sender<Msg>) {
    thread::spawn(move || loop {
        let address = SocketAddr::new(ip, port);
        if let Ok(stream) = TcpStream::connect_timeout(&address, AGENT_CONNECT_TIMEOUT) {
            let _ = stream.set_nodelay(true);
            if let Ok(writer) = stream.try_clone() {
                let _ = tx.send(Msg::AgentUp(ip, writer));
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        if tx.send(Msg::AgentLine(ip, value)).is_err() {
                            return;
                        }
                    }
                }
                let _ = tx.send(Msg::AgentGone(ip));
            }
        }
        thread::sleep(AGENT_RETRY);
    });
}

/// Ask the network for the machine's name, off the main loop: it can take seconds.
pub fn spawn_resolve(ip: IpAddr, tx: Sender<Msg>) {
    thread::spawn(move || {
        if let Some(name) = reverse_lookup(ip) {
            let _ = tx.send(Msg::Resolved(ip, name));
        }
    });
}

fn reverse_lookup(ip: IpAddr) -> Option<String> {
    if let IpAddr::V4(v4) = ip {
        if let Some(name) = names::mdns_reverse(v4) {
            return Some(name);
        }
    }
    let mut host = [0 as libc::c_char; 256];
    // SAFETY: the sockaddr is built for its family and outlives the call, and
    // getnameinfo gets the real size of `host`, which it NUL-terminates.
    let status = unsafe {
        let (storage, len) = sockaddr(ip);
        libc::getnameinfo(
            (&storage as *const libc::sockaddr_storage).cast(),
            len,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: getnameinfo succeeded, so `host` holds a NUL-terminated string.
    Some(unsafe { CStr::from_ptr(host.as_ptr()) }.to_string_lossy().into_owned())
}

unsafe fn sockaddr(ip: IpAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = mem::zeroed();
    let len = match ip {
        IpAddr::V4(v4) => {
            let addr = (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>();
            (*addr).sin_family = libc::AF_INET as libc::sa_family_t;
            (*addr).sin_addr.s_addr = u32::from_ne_bytes(v4.octets());
            mem::size_of::<libc::sockaddr_in>()
        }
        IpAddr::V6(v6) => {
            let addr = (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>();
            (*addr).sin6_family = libc::AF_INET6 as libc::sa_family_t;
            (*addr).sin6_addr.s6_addr = v6.octets();
            mem::size_of::<libc::sockaddr_in6>()
        }
    };
    #[cfg(target_os = "macos")]
    {
        (*(&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr>()).sa_len = len as u8;
    }
    (storage, len as libc::socklen_t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_socket_in_the_directory_is_a_receiver() {
        let dir = std::env::temp_dir().join(format!("pcmlink-mixer-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let listening = dir.join("7777.sock");
        let _ = fs::remove_file(&listening);
        let _listener = std::os::unix::net::UnixListener::bind(&listening).unwrap();
        fs::write(dir.join("notes.txt"), "not a socket").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut discovery = Discovery::new(dir.clone(), tx);
        discovery.scan();
        discovery.scan(); // once only
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::StreamUp(p, _)) if p == listening));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
