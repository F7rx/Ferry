//! Local network interfaces: which ones to use for discovery, which addresses
//! to show in QR codes, and whether a peer address is on our network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LocalInterface {
    pub name: String,
    pub addr: IpAddr,
    pub prefix_len: u8,
    pub index: Option<u32>,
    /// VPN, virtual machine, container or tunnel adapter.
    pub is_virtual: bool,
}

/// All usable interfaces: up, not loopback, not IPv4 link-local (169.254/16).
/// Virtual adapters are included only when asked for.
pub fn list(include_virtual: bool) -> Vec<LocalInterface> {
    let Ok(all) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<LocalInterface> = all
        .into_iter()
        .filter(|i| !i.is_loopback() && i.is_oper_up())
        .filter_map(|i| {
            let (addr, prefix_len) = match &i.addr {
                if_addrs::IfAddr::V4(v4) => (IpAddr::V4(v4.ip), v4.prefixlen),
                if_addrs::IfAddr::V6(v6) => (IpAddr::V6(v6.ip), v6.prefixlen),
            };
            if let IpAddr::V4(v4) = addr
                && (v4.is_link_local() || v4.is_unspecified())
            {
                return None;
            }
            let is_virtual = i.is_p2p() || is_virtual_name(&i.name);
            Some(LocalInterface { name: i.name.clone(), addr, prefix_len, index: i.index, is_virtual })
        })
        .filter(|i| include_virtual || !i.is_virtual)
        .collect();
    out.sort_by_key(address_rank);
    out.dedup_by(|a, b| a.addr == b.addr);
    out
}

/// Recognizes adapters that are not the user's physical network: hypervisors,
/// containers, VPN tunnels. Discovery on them leaks our identity into foreign
/// subnets and finds nothing useful.
pub fn is_virtual_name(name: &str) -> bool {
    let n = name.to_lowercase();
    const CONTAINS: [&str; 22] = [
        "vmware",
        "virtualbox",
        "vbox",
        "hyper-v",
        "vethernet",
        "wsl",
        "docker",
        "wireguard",
        "tailscale",
        "zerotier",
        "nordlynx",
        "openvpn",
        "vpn",
        "npcap",
        "teredo",
        "isatap",
        "pseudo",
        "loopback",
        "parallels",
        "globalprotect",
        "fortinet",
        "anyconnect",
    ];
    const PREFIXES: [&str; 17] =
        ["tun", "tap", "utun", "wg", "br-", "veth", "virbr", "vmnet", "ppp", "ipsec", "llw", "awdl", "anpi", "gif", "stf", "zt", "cni"];
    CONTAINS.iter().any(|t| n.contains(t)) || PREFIXES.iter().any(|p| n.starts_with(p))
}

/// Lower is better: private IPv4 on physical adapters first (what people type
/// and what other devices on the Wi-Fi can reach), then ULA/global IPv6, then
/// IPv6 link-local (needs a scope to be dialed).
fn address_rank(i: &LocalInterface) -> (u8, u8) {
    let class = match i.addr {
        IpAddr::V4(v4) if v4.is_private() => 0,
        IpAddr::V4(_) => 1,
        IpAddr::V6(v6) if is_unique_local(&v6) => 2,
        IpAddr::V6(v6) if is_link_local_v6(&v6) => 4,
        IpAddr::V6(_) => 3,
    };
    (i.is_virtual as u8, class)
}

pub fn is_link_local_v6(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

pub fn is_unique_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// Addresses to show to users (QR codes, "connect by address"), best first.
pub fn shareable_addresses(interfaces: &[LocalInterface]) -> Vec<String> {
    interfaces
        .iter()
        .filter(|i| match i.addr {
            IpAddr::V6(v6) => !is_link_local_v6(&v6),
            IpAddr::V4(_) => true,
        })
        .map(|i| i.addr.to_string())
        .collect()
}

/// Whether `ip` is on one of our local networks; announcements from anywhere
/// else are never answered (prevents off-network reflection).
pub fn is_local_peer(ip: IpAddr, interfaces: &[LocalInterface]) -> bool {
    let ip = canonical(ip);
    if ip.is_loopback() {
        return true;
    }
    match ip {
        IpAddr::V6(v6) if is_link_local_v6(&v6) => true,
        _ => interfaces.iter().any(|i| same_subnet(ip, i.addr, i.prefix_len)),
    }
}

/// IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) as plain IPv4.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

fn same_subnet(a: IpAddr, b: IpAddr, prefix_len: u8) -> bool {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let bits = prefix_len.min(32) as u32;
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            (u32::from(a) & mask) == (u32::from(b) & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let bits = prefix_len.min(128) as u32;
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            (u128::from(a) & mask) == (u128::from(b) & mask)
        }
        _ => false,
    }
}

/// Every host address of the IPv4 networks we are on, for the legacy subnet
/// scan. Respects the real netmask; networks larger than /22 are limited to
/// the /24 around our own address (scanning a /16 would take minutes).
pub fn scan_targets(interfaces: &[LocalInterface]) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for i in interfaces {
        let IpAddr::V4(own) = i.addr else { continue };
        let prefix = if i.prefix_len < 22 { 24 } else { i.prefix_len.min(30) };
        let mask = u32::MAX << (32 - prefix as u32);
        let network = u32::from(own) & mask;
        let broadcast = network | !mask;
        for host in (network + 1)..broadcast {
            let ip = Ipv4Addr::from(host);
            if ip != own {
                out.push(ip);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, addr: &str, prefix_len: u8) -> LocalInterface {
        LocalInterface { name: name.into(), addr: addr.parse().unwrap(), prefix_len, index: None, is_virtual: is_virtual_name(name) }
    }

    #[test]
    fn recognizes_virtual_adapters() {
        for name in
            ["VMware Network Adapter VMnet8", "vEthernet (WSL)", "NordLynx", "docker0", "utun3", "wg0", "Tailscale", "tun0", "br-1a2b"]
        {
            assert!(is_virtual_name(name), "{name}");
        }
        for name in ["Wi-Fi", "Ethernet", "en0", "wlan0", "eth0", "Ethernet 2", "wlp3s0"] {
            assert!(!is_virtual_name(name), "{name}");
        }
    }

    #[test]
    fn local_peer_check_uses_real_prefixes() {
        let ifaces = vec![iface("Wi-Fi", "192.168.11.102", 24), iface("Ethernet", "fd00::5", 64)];
        assert!(is_local_peer("192.168.11.7".parse().unwrap(), &ifaces));
        assert!(!is_local_peer("192.168.12.7".parse().unwrap(), &ifaces));
        assert!(!is_local_peer("8.8.8.8".parse().unwrap(), &ifaces));
        assert!(is_local_peer("fd00::99".parse().unwrap(), &ifaces));
        assert!(is_local_peer("fe80::1".parse().unwrap(), &ifaces));
        assert!(is_local_peer("::ffff:192.168.11.9".parse().unwrap(), &ifaces));
    }

    #[test]
    fn scan_targets_respect_netmask() {
        let small = scan_targets(&[iface("Wi-Fi", "10.0.0.5", 28)]);
        assert_eq!(small.len(), 13); // 14 hosts minus ourselves
        let big = scan_targets(&[iface("Ethernet", "10.1.2.3", 16)]);
        assert_eq!(big.len(), 253); // limited to the /24 around us
        assert!(!big.contains(&"10.1.2.3".parse().unwrap()));
        assert!(!big.contains(&"10.1.2.0".parse().unwrap()));
        assert!(!big.contains(&"10.1.2.255".parse().unwrap()));
    }

    #[test]
    #[ignore = "prints this machine's interfaces"]
    fn print_interfaces() {
        for i in list(true) {
            println!("{:<40} {:<40} /{:<3} virtual={}", i.name, i.addr, i.prefix_len, i.is_virtual);
        }
    }
}
