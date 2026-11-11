//! Where one request may go (`web egress`).
//! # What this is.
//! A guard rail, not a boundary.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

use super::fetch::WebError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Public internet only: loopback/private/link-local and friends are refused.
    Public,
    /// No address classification at all (the shell's effective reach, minus non-http schemes).
    Any,
    /// Only `[web] hosts`, everything else refused.
    Allowlist,
}

impl Policy {
    /// Unknown values fall back to [`Policy::Public`] — the conservative direction, the same rule
    /// `[auth]` uses for an unparsable level.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "any" | "all" => Self::Any,
            "allowlist" | "allow-list" | "list" => Self::Allowlist,
            _ => Self::Public,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Any => "any",
            Self::Allowlist => "allowlist",
        }
    }
}

/// Is `host` covered by one entry of `web hosts`?
pub fn host_listed(host: &str, list: &[String]) -> bool {
    let host = normalise(host);
    if host.is_empty() {
        return false;
    }
    list.iter().filter_map(|raw| {
        let entry = normalise(raw);
        (!entry.is_empty()).then_some(entry)
    })
    .any(|entry| {
        match entry.strip_prefix("*.") {
            Some(suffix) => is_subdomain(&host, suffix),
            None => host == entry || is_subdomain(&host, &entry),
        }
    })
}

fn normalise(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// `host` ends with `.suffix` and has at least one label in front of it.
fn is_subdomain(host: &str, suffix: &str) -> bool {
    if suffix.is_empty() || host.len() <= suffix.len() + 1 {
        return false;
    }
    host.ends_with(suffix) && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
}

/// Is this address one `public` refuses?
pub fn addr_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            // `::ffff:127.0.0.1` is loopback however it is spelled
            Some(v4) => v4_blocked(v4),
            None => v6_blocked(v6),
        },
    }
}

fn v4_blocked(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0                       // 0.0.0.0/8 "this network"
        || a == 10               // RFC1918
        || a == 127              // loopback
        || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64/10
        || (a == 169 && b == 254) // link-local
        || (a == 172 && (16..=31).contains(&b)) // RFC1918
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // IETF assignments, TEST-NET-1
        || (a == 192 && b == 168) // RFC1918
        // 198.18.0.0/15 is "benchmarking" on paper, but transparent proxies and VPN clients map every domain into it (a...
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 224 // multicast (224/4), reserved (240/4), broadcast
}

fn v6_blocked(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (s[0] & 0xff00) == 0xff00 // multicast ff00::/8
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation 2001:db8::/32
        || s[0] == 0x2002 // 6to4 embeds an IPv4 address; not worth reasoning about
        || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64
}

/// Is this URL's host literally a loopback/private address (or `localhost`)?
pub fn is_local_literal(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => v4_blocked(ip),
        Some(Host::Ipv6(ip)) => v6_blocked(ip),
        Some(Host::Domain(name)) => {
            let name = name.to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        None => false,
    }
}

/// Does `[web] egress` allow this URL? `Ok(())` = go ahead
pub async fn check(url: &Url, policy: Policy, hosts: &[String]) -> Result<(), WebError> {
    let Some(host) = url.host() else {
        return Err(WebError::new("the URL has no host").hint("pass an absolute http(s) URL"));
    };
    let shown = host.to_string();
    if policy == Policy::Any || host_listed(&shown, hosts) {
        return Ok(());
    }
    if policy == Policy::Allowlist {
        return Err(WebError::new(crate::tf!(
            "blocked by [web] egress = \"allowlist\": {} is not in [web] hosts",
            shown
        ))
        .hint(crate::tf!(
            "add \"{}\" to [web] hosts, or switch [web] egress to \"public\" / \"any\"",
            shown
        )));
    }

    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = match host {
        Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
        Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
        Host::Domain(name) => tokio::net::lookup_host((name, port))
            .await
            .map_err(|e| {
                WebError::new(crate::tf!("cannot resolve {}: {}", shown, e))
                    .hint("check the spelling; if this machine is behind a proxy, set HTTPS_PROXY")
            })?
            .map(|socket| socket.ip())
            .collect(),
    };
    if addrs.is_empty() {
        return Err(WebError::new(crate::tf!("cannot resolve {}: no address", shown))
            .hint("check the spelling; if this machine is behind a proxy, set HTTPS_PROXY"));
    }
    if let Some(bad) = addrs.iter().find(|ip| addr_blocked(**ip)) {
        return Err(WebError::new(crate::tf!(
            "blocked by [web] egress = \"public\": {} resolves to {} (a private/local address)",
            shown,
            bad
        ))
        .hint(crate::tf!(
            "add \"{}\" to [web] hosts to reach local services, or set [web] egress = \"any\"",
            shown
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn private_and_local_addresses_are_refused() {
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(addr_blocked(ip), "{s} must be blocked in public mode");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        // 198.18/15 is the fake-IP range a transparent proxy hands out; see `v4_blocked`.
        for s in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "93.184.216.34", "198.18.0.17", "2606:4700:4700::1111"] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!addr_blocked(ip), "{s} must be allowed in public mode");
        }
    }

    #[test]
    fn hosts_entries_match_on_label_boundaries() {
        let l = list(&["example.com"]);
        assert!(host_listed("example.com", &l));
        assert!(host_listed("www.example.com", &l));
        assert!(host_listed("a.b.example.com", &l));
        assert!(host_listed("EXAMPLE.com.", &l), "case and trailing dot are ignored");
        assert!(!host_listed("badexample.com", &l));
        assert!(!host_listed("example.com.evil.test", &l));
    }

    #[test]
    fn a_star_entry_matches_subdomains_only() {
        let l = list(&["*.example.com"]);
        assert!(host_listed("www.example.com", &l));
        assert!(!host_listed("example.com", &l), "*.example.com is not the domain itself");
        assert!(!host_listed("badexample.com", &l));
    }

    #[test]
    fn an_ip_entry_matches_that_address_only() {
        let l = list(&["127.0.0.1", "[::1]"]);
        assert!(host_listed("127.0.0.1", &l));
        assert!(host_listed("::1", &l));
        assert!(!host_listed("127.0.0.2", &l));
        assert!(!host_listed("localhost", &l), "localhost is a name; list it by name if wanted");
    }

    #[tokio::test]
    async fn public_mode_refuses_loopback_before_any_request() {
        let url = Url::parse("http://127.0.0.1:8080/admin").unwrap();
        let err = check(&url, Policy::Public, &[]).await.unwrap_err();
        assert!(err.message.contains("egress"), "{}", err.message);
        assert!(err.hint.unwrap().contains("hosts"));
    }

    #[tokio::test]
    async fn listed_host_is_the_way_in() {
        let url = Url::parse("http://127.0.0.1:8080/admin").unwrap();
        assert!(check(&url, Policy::Public, &list(&["127.0.0.1"])).await.is_ok());
    }

    #[tokio::test]
    async fn allowlist_refuses_everything_else() {
        let url = Url::parse("https://example.com/").unwrap();
        let err = check(&url, Policy::Allowlist, &[]).await.unwrap_err();
        assert!(err.message.contains("allowlist"), "{}", err.message);
        assert!(check(&url, Policy::Allowlist, &list(&["example.com"])).await.is_ok());
    }

    #[tokio::test]
    async fn any_skips_the_classification() {
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        assert!(check(&url, Policy::Any, &[]).await.is_ok());
    }

    #[test]
    fn local_literals_bypass_the_ambient_proxy() {
        for s in [
            "http://127.0.0.1:8080/",
            "http://[::1]:8080/",
            "http://localhost:3000/",
            "http://dev.localhost/",
            "http://192.168.1.10/",
        ] {
            assert!(is_local_literal(&Url::parse(s).unwrap()), "{s} is local");
        }
        for s in ["https://example.com/", "https://8.8.8.8/"] {
            assert!(!is_local_literal(&Url::parse(s).unwrap()), "{s} is not local");
        }
    }
}
