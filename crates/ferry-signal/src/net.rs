// Derived from LocalSend (https://github.com/localsend/localsend, Apache-2.0); modified by the Ferry authors.
//! Client address resolution and "nearby" IP grouping.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::http::HeaderMap;
use ipnet::IpNet;

/// Peers in the same group are "nearby": same public IPv4 address, or same
/// IPv6 /64 prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IpGroup {
    V4(Ipv4Addr),
    V6([u16; 4]),
}

impl IpGroup {
    pub(crate) fn of(ip: IpAddr) -> Self {
        // IPv4-mapped IPv6 addresses (dual-stack listeners) are IPv4 clients.
        match ip.to_canonical() {
            IpAddr::V4(v4) => Self::V4(v4),
            IpAddr::V6(v6) => {
                let s = v6.segments();
                Self::V6([s[0], s[1], s[2], s[3]])
            }
        }
    }
}

/// Resolves the client address of a request.
///
/// The socket peer address is used unless the peer is a trusted proxy; then
/// `X-Forwarded-For` is walked from the right and the first entry that is not
/// itself a trusted proxy wins. If every entry is a trusted proxy the
/// left-most one is used. Returns `None` when a trusted proxy sent an
/// unparseable chain (the request should be rejected rather than guessed).
pub(crate) fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> Option<IpAddr> {
    let peer = peer.to_canonical();
    if !is_trusted(peer, trusted) {
        return Some(peer);
    }
    let mut client = peer;
    for value in headers.get_all("x-forwarded-for").iter().rev() {
        let value = value.to_str().ok()?;
        for entry in value.rsplit(',') {
            let ip = parse_forwarded(entry)?;
            if !is_trusted(ip, trusted) {
                return Some(ip);
            }
            client = ip;
        }
    }
    Some(client)
}

fn is_trusted(ip: IpAddr, trusted: &[IpNet]) -> bool {
    trusted.iter().any(|net| net.contains(&ip))
}

/// Parses one `X-Forwarded-For` entry: `1.2.3.4`, `1.2.3.4:5678`, `::1`,
/// `[::1]` or `[::1]:5678`.
fn parse_forwarded(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    let ip = entry
        .parse::<IpAddr>()
        .ok()
        .or_else(|| entry.parse::<SocketAddr>().ok().map(|s| s.ip()))
        .or_else(|| entry.strip_prefix('[').and_then(|e| e.strip_suffix(']')).and_then(|e| e.parse::<IpAddr>().ok()))?;
    Some(ip.to_canonical())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn xff(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for v in values {
            headers.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        headers
    }

    #[test]
    fn groups_ipv4_exactly_and_ipv6_by_64() {
        assert_eq!(IpGroup::of(ip("1.2.3.4")), IpGroup::V4(Ipv4Addr::new(1, 2, 3, 4)));
        assert_ne!(IpGroup::of(ip("1.2.3.4")), IpGroup::of(ip("1.2.3.5")));
        assert_eq!(IpGroup::of(ip("1:2:3:4:5:6:7:8")), IpGroup::V6([1, 2, 3, 4]));
        assert_eq!(IpGroup::of(ip("2001:db8:a:b::1")), IpGroup::of(ip("2001:db8:a:b:ffff::2")));
        assert_ne!(IpGroup::of(ip("2001:db8:a:b::1")), IpGroup::of(ip("2001:db8:a:c::1")));
        // Dual-stack listeners see IPv4 clients as IPv4-mapped addresses.
        assert_eq!(IpGroup::of(ip("::ffff:1.2.3.4")), IpGroup::of(ip("1.2.3.4")));
        assert_ne!(IpGroup::of(ip("::ffff:1.2.3.4")), IpGroup::of(ip("::ffff:9.9.9.9")));
    }

    #[test]
    fn untrusted_peer_ignores_forwarded_header() {
        let headers = xff(&["203.0.113.7"]);
        assert_eq!(client_ip(ip("198.51.100.1"), &headers, &[]), Some(ip("198.51.100.1")));
        let trusted = nets(&["10.0.0.0/8"]);
        assert_eq!(client_ip(ip("198.51.100.1"), &headers, &trusted), Some(ip("198.51.100.1")));
    }

    #[test]
    fn trusted_proxy_uses_rightmost_untrusted_entry() {
        let trusted = nets(&["10.0.0.0/8", "127.0.0.1/32"]);
        let peer = ip("10.0.0.2");
        // Spoofed left-most entries are ignored.
        let headers = xff(&["1.1.1.1, 203.0.113.5"]);
        assert_eq!(client_ip(peer, &headers, &trusted), Some(ip("203.0.113.5")));
        // Proxy chains are skipped.
        let headers = xff(&["1.1.1.1, 203.0.113.5, 10.0.0.9"]);
        assert_eq!(client_ip(peer, &headers, &trusted), Some(ip("203.0.113.5")));
        // Multiple header instances form one list.
        let headers = xff(&["1.1.1.1", "203.0.113.6:4444, 10.1.1.1"]);
        assert_eq!(client_ip(peer, &headers, &trusted), Some(ip("203.0.113.6")));
        // IPv6 forms.
        let headers = xff(&["[2001:db8::1]:443"]);
        assert_eq!(client_ip(peer, &headers, &trusted), Some(ip("2001:db8::1")));
        let headers = xff(&["[2001:db8::2]"]);
        assert_eq!(client_ip(peer, &headers, &trusted), Some(ip("2001:db8::2")));
        // IPv4-mapped peers match IPv4 proxy ranges.
        let headers = xff(&["203.0.113.8"]);
        assert_eq!(client_ip(ip("::ffff:10.0.0.2"), &headers, &trusted), Some(ip("203.0.113.8")));
    }

    #[test]
    fn trusted_proxy_edge_cases() {
        let trusted = nets(&["10.0.0.0/8"]);
        let peer = ip("10.0.0.2");
        // No header: the proxy itself is the client.
        assert_eq!(client_ip(peer, &HeaderMap::new(), &trusted), Some(peer));
        // Only proxies: the left-most entry.
        let headers = xff(&["10.0.0.7, 10.0.0.8"]);
        assert_eq!(client_ip(peer, &headers, &trusted), Some(ip("10.0.0.7")));
        // Garbage written by the proxy chain is rejected, not guessed.
        assert_eq!(client_ip(peer, &xff(&["unknown"]), &trusted), None);
        assert_eq!(client_ip(peer, &xff(&["203.0.113.5, "]), &trusted), None);
        // Garbage left of the client entry is never reached.
        let headers = xff(&["garbage, 203.0.113.5"]);
        assert_eq!(client_ip(peer, &headers, &trusted), Some(ip("203.0.113.5")));
    }
}
