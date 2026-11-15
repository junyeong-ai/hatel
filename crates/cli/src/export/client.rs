//! The HTTP client every egress path posts through.

use std::net::IpAddr;
use std::time::Duration;

/// Cap on establishing the TCP/TLS connection, so a downstream that accepts the socket but never
/// responds can't pin a delivery task past this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Posts to downstream collectors. A loopback destination is reached directly and any other
/// through the proxy the environment configures: loopback traffic never leaves the host, so it is
/// never proxied — the rule Go's `net/http` applies to `localhost` and loopback addresses.
#[derive(Clone)]
pub struct Egress {
    direct: reqwest::Client,
    proxied: reqwest::Client,
}

impl Egress {
    pub fn new() -> reqwest::Result<Self> {
        Ok(Self {
            direct: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .no_proxy()
                .build()?,
            proxied: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()?,
        })
    }

    pub fn post(&self, url: &str) -> reqwest::RequestBuilder {
        let client = if is_loopback(url) {
            &self.direct
        } else {
            &self.proxied
        };
        client.post(url)
    }
}

fn is_loopback(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::is_loopback;

    #[test]
    fn only_loopback_destinations_bypass_the_proxy() {
        for url in [
            "http://localhost:4318/v1/logs",
            "http://LOCALHOST/v1/logs",
            "http://127.0.0.1:4318/v1/metrics",
            "http://127.8.0.1/v1/metrics",
            "http://[::1]:4318/v1/traces",
        ] {
            assert!(is_loopback(url), "{url}");
        }
        for url in [
            "https://collector.example.com/v1/logs",
            "http://10.0.0.1:4318/v1/logs",
            "http://localhost.example.com/v1/logs",
            "http://[::2]/v1/logs",
            "not a url",
        ] {
            assert!(!is_loopback(url), "{url}");
        }
    }
}
