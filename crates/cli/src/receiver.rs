//! What a running receiver says about itself, and how `doctor` asks. A receiver keeps the binary
//! it started from, so after an upgrade the build answering on the port is not necessarily the
//! build diagnosing it; the only way to know is to ask the process. The answer is an HTTP
//! `GET /healthz` on the OTLP port — the address Claude Code already pushes to — carrying the
//! build as JSON, so a shell can read it the same way (`curl localhost:4318/healthz`).

use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const IDENTITY_PATH: &str = "/healthz";
/// The first build that answers on the identity route; an older one answers 404, like any
/// server that is not a receiver.
pub const IDENTITY_SINCE: &str = "0.18.0";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// The whole exchange after connecting — a server that trickles bytes is cut off here, not
/// granted this much per read.
const ANSWER_DEADLINE: Duration = Duration::from_secs(2);
/// Enough for any identity answer; a foreign server's page is cut here rather than read whole.
const MAX_ANSWER_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub service: String,
    pub version: String,
}

impl Identity {
    pub fn this_build() -> Self {
        Self {
            service: "hatel".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// What answered at an authority (`host:port`).
#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// A hatel receiver, and its build.
    Build(String),
    /// Something accepted the connection but did not identify as a hatel receiver — a build
    /// from before the identity route, or another collector.
    Foreign,
    /// No connection, and why.
    Unreachable(String),
}

pub fn probe(authority: &str) -> Probe {
    let mut stream = match connect(authority) {
        Ok(s) => s,
        Err(e) => return Probe::Unreachable(e),
    };
    let request =
        format!("GET {IDENTITY_PATH} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    let started = Instant::now();
    let _ = stream.set_write_timeout(Some(ANSWER_DEADLINE));
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return Probe::Unreachable(e.to_string());
    }
    // Whatever arrived by the deadline is judged: a server that ignores `Connection: close`, or
    // trickles bytes, gets no longer than a silent one.
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    while answer.len() < MAX_ANSWER_BYTES {
        let Some(left) = ANSWER_DEADLINE.checked_sub(started.elapsed()) else {
            break;
        };
        let _ = stream.set_read_timeout(Some(left));
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
        }
    }
    identify(&answer)
}

fn connect(authority: &str) -> Result<TcpStream, String> {
    let addrs = authority.to_socket_addrs().map_err(|e| e.to_string())?;
    let mut last = format!("{authority} resolves to no address");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// Judge an HTTP/1.1 answer: a 200 whose body is an [`Identity`] naming this service is a
/// receiver; anything else is foreign. Only the status line and the body are read, and the body
/// is taken as it lies after the headers — the receiver sends it whole with a `Content-Length`
/// and closes, so a chunked answer (a proxy in front of one) reads as foreign.
fn identify(answer: &[u8]) -> Probe {
    let text = String::from_utf8_lossy(answer);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return Probe::Foreign;
    };
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok());
    if status != Some(200) {
        return Probe::Foreign;
    }
    match serde_json::from_str::<Identity>(body.trim()) {
        Ok(id) if id.service == "hatel" => Probe::Build(id.version),
        _ => Probe::Foreign,
    }
}

/// The `host:port` an OTLP endpoint addresses — the part a probe connects to. A scheme-less
/// endpoint is an authority already; a path after it is Claude Code's business.
pub fn authority(endpoint: &str) -> String {
    let rest = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint);
    let authority = rest.split('/').next().unwrap_or(rest);
    if (authority.starts_with('[') && !authority.contains("]:")) || !authority.contains(':') {
        format!("{authority}:80")
    } else {
        authority.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    /// A one-shot server answering `response` to whatever arrives, on a port of the OS's choice.
    fn server(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(response.as_bytes());
        });
        authority
    }

    #[test]
    fn a_receiver_answers_its_build() {
        let body = r#"{"service":"hatel","version":"9.9.9"}"#;
        let authority = server(Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        ));
        assert_eq!(probe(&authority), Probe::Build("9.9.9".to_string()));
    }

    #[test]
    fn a_server_that_is_not_a_receiver_is_foreign() {
        for response in [
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n{\"service\":\"other\",\"version\":\"1.0.0\"}",
            "not http at all",
        ] {
            assert_eq!(probe(&server(response)), Probe::Foreign, "{response:?}");
        }
    }

    #[test]
    fn a_server_that_trickles_is_cut_off_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 1024]);
            for byte in b"HTTP/1.1 200 OK\r\n".iter().cycle() {
                if s.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let started = Instant::now();
        assert_eq!(probe(&authority), Probe::Foreign);
        assert!(started.elapsed() < ANSWER_DEADLINE + Duration::from_secs(1));
    }

    #[test]
    fn a_closed_port_is_unreachable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(matches!(probe(&authority), Probe::Unreachable(_)));
    }

    #[test]
    fn an_endpoint_reduces_to_its_authority() {
        assert_eq!(authority("http://127.0.0.1:4318"), "127.0.0.1:4318");
        assert_eq!(authority("http://localhost:4318/"), "localhost:4318");
        assert_eq!(authority("http://[::1]:4318/v1/metrics"), "[::1]:4318");
        assert_eq!(authority("127.0.0.1:4318"), "127.0.0.1:4318");
        assert_eq!(authority("http://localhost"), "localhost:80");
    }
}
