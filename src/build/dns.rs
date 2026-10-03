//! The `/etc/resolv.conf` a RUN step sees. The guest's sockets are proxied
//! by the host (TSI), but loopback resolvers on the host are not reachable
//! from the guest's own loopback, so they are dropped as Docker does.

use std::fs;
use std::net::Ipv4Addr;

const HOST_RESOLV_CONF: &str = "/etc/resolv.conf";
/// The upstream servers behind systemd-resolved's 127.0.0.53 stub.
const SYSTEMD_RESOLV_CONF: &str = "/run/systemd/resolve/resolv.conf";
const FALLBACK: &str = "nameserver 8.8.8.8\nnameserver 8.8.4.4\n";

pub fn resolv_conf() -> String {
    guest_resolv_conf(
        fs::read_to_string(HOST_RESOLV_CONF).ok().as_deref(),
        fs::read_to_string(SYSTEMD_RESOLV_CONF).ok().as_deref(),
    )
}

pub fn guest_resolv_conf(host: Option<&str>, systemd: Option<&str>) -> String {
    let host = host.unwrap_or("");
    let servers: Vec<Ipv4Addr> = nameservers(host).collect();
    let source = if !servers.is_empty() && servers.iter().all(Ipv4Addr::is_loopback) {
        systemd.unwrap_or(host)
    } else {
        host
    };
    let mut out = String::new();
    let mut found = false;
    for line in source.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(ip) = words.next().and_then(|w| w.parse::<Ipv4Addr>().ok())
                    && !ip.is_loopback()
                {
                    out.push_str(line.trim());
                    out.push('\n');
                    found = true;
                }
            }
            Some("search" | "domain" | "options") => {
                out.push_str(line.trim());
                out.push('\n');
            }
            _ => {}
        }
    }
    if !found {
        out.push_str(FALLBACK);
    }
    out
}

fn nameservers(conf: &str) -> impl Iterator<Item = Ipv4Addr> + '_ {
    conf.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("nameserver"))
            .then(|| words.next()?.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_ipv6_servers_are_dropped() {
        let got = guest_resolv_conf(
            Some(
                "search corp.example\nnameserver 127.0.0.1\nnameserver fe80::1%en0\nnameserver 10.0.0.2\noptions ndots:2\n",
            ),
            None,
        );
        assert_eq!(
            got,
            "search corp.example\nnameserver 10.0.0.2\noptions ndots:2\n"
        );
    }

    #[test]
    fn systemd_stub_is_replaced_by_upstream_servers() {
        let got = guest_resolv_conf(
            Some("nameserver 127.0.0.53\nsearch lan\n"),
            Some("nameserver 192.168.1.1\nsearch lan\n"),
        );
        assert_eq!(got, "nameserver 192.168.1.1\nsearch lan\n");
    }

    #[test]
    fn falls_back_to_public_servers() {
        assert_eq!(
            guest_resolv_conf(None, None),
            "nameserver 8.8.8.8\nnameserver 8.8.4.4\n"
        );
        assert_eq!(
            guest_resolv_conf(Some("nameserver 127.0.0.53\n"), None),
            "nameserver 8.8.8.8\nnameserver 8.8.4.4\n"
        );
    }
}
