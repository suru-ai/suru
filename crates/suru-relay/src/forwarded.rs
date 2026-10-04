//! The network address a Server connects to the Relay from, as the
//! connection log names it.
//!
//! Behind a reverse proxy the Relay sees only the proxy's address. The proxy
//! names the address it forwards for in an `X-Forwarded-For` header, but
//! anything that reaches the Relay can write that header, so the Relay
//! believes it only from a proxy its operator names, and from none unless
//! named.

use std::{
    fmt,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    str::FromStr,
};

use axum::http::HeaderMap;
use ipnet::IpNet;

/// The header a reverse proxy names the address it forwards for in, each
/// proxy on the way adding the address it was reached from after those
/// already named.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// A reverse proxy whose `X-Forwarded-For` header a Relay believes, named by
/// its address — `10.0.0.5`, `::1` — or by a network it is among, in CIDR
/// notation — `10.0.0.0/8`, `fd00::/8`.
///
/// A Relay believes the header only from a connection a named proxy made to
/// it, and reads it from the nearest hop out: each named proxy is believed
/// about the address it was reached from, and the first address no named
/// proxy is at is the Server's. Whatever lies beyond that address — written
/// by the Server itself, or by anything else before the named proxies — is
/// passed over. Where a named proxy names nothing that reads as an address,
/// nothing further out is believed and the address is that proxy's own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrustedProxy(IpNet);

impl TrustedProxy {
    /// Whether `address`, written as [`IpAddr::to_canonical`] writes it, is
    /// this proxy's, however the operator wrote the proxy.
    fn names(&self, address: IpAddr) -> bool {
        let mapped = match address {
            IpAddr::V4(address) => IpAddr::V6(address.to_ipv6_mapped()),
            IpAddr::V6(_) => address,
        };
        self.0.contains(&address) || self.0.contains(&mapped)
    }
}

impl FromStr for TrustedProxy {
    type Err = UnrecognizedProxy;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        text.parse::<IpNet>()
            .map(|network| network.trunc())
            .or_else(|_| text.parse::<IpAddr>().map(IpNet::from))
            .map(Self)
            .map_err(|_| UnrecognizedProxy(text.to_owned()))
    }
}

/// A proxy named as neither an address nor a network.
#[derive(Debug, Eq, PartialEq)]
pub struct UnrecognizedProxy(String);

impl fmt::Display for UnrecognizedProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "`{}` names a proxy by neither an address, such as `10.0.0.5`, nor a network, such \
             as `10.0.0.0/8`",
            self.0
        )
    }
}

impl std::error::Error for UnrecognizedProxy {}

/// The network address a Server's connection comes from, where `peer` is
/// the address that connected to the Relay and `headers` are those its
/// request to connect carried: `peer`, unless it is among the `trusted`
/// proxies, when it is the address their `X-Forwarded-For` header names as
/// [`TrustedProxy`] says — its lines read as one list, in order, and where
/// every address it names is a named proxy's, the furthest of them.
pub(crate) fn network_address(
    peer: IpAddr,
    headers: &HeaderMap,
    trusted: &[TrustedProxy],
) -> IpAddr {
    let named = |address: IpAddr| trusted.iter().any(|proxy| proxy.names(address));
    // Each hop the header names, nearest first: an address, or `None` where
    // it names nothing that reads as one. A line that is not even text is
    // one such hop.
    let mut hops = headers
        .get_all(FORWARDED_FOR)
        .iter()
        .rev()
        .flat_map(|line| {
            let readable = line.to_str().ok();
            let unreadable = readable.is_none().then_some(None);
            readable
                .into_iter()
                .flat_map(|line| line.rsplit(','))
                .map(str::trim)
                .filter(|hop| !hop.is_empty())
                .map(hop_address)
                .chain(unreadable)
        });
    let mut address = peer.to_canonical();
    while named(address) {
        match hops.next() {
            Some(Some(hop)) => address = hop.to_canonical(),
            Some(None) | None => break,
        }
    }
    address
}

/// The address one hop names, written as proxies variously write it: bare,
/// bracketed, or with a port.
fn hop_address(hop: &str) -> Option<IpAddr> {
    hop.parse::<IpAddr>()
        .ok()
        .or_else(|| hop.parse::<SocketAddr>().ok().map(|socket| socket.ip()))
        .or_else(|| {
            hop.strip_prefix('[')?
                .strip_suffix(']')?
                .parse::<Ipv6Addr>()
                .ok()
                .map(IpAddr::V6)
        })
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn address(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn proxies(named: &[&str]) -> Vec<TrustedProxy> {
        named.iter().map(|proxy| proxy.parse().unwrap()).collect()
    }

    /// The address a connection from `peer`, carrying an `X-Forwarded-For`
    /// line for each of `lines`, comes from, `named` being the proxies
    /// believed.
    fn forwarded(peer: &str, lines: &[&[u8]], named: &[&str]) -> IpAddr {
        let mut headers = HeaderMap::new();
        for line in lines {
            headers.append(FORWARDED_FOR, HeaderValue::from_bytes(line).unwrap());
        }
        network_address(address(peer), &headers, &proxies(named))
    }

    #[test]
    fn a_proxy_is_named_by_its_address_or_a_network_it_is_among() {
        for (named, among, apart) in [
            ("10.0.0.5", "10.0.0.5", "10.0.0.6"),
            (" 10.0.0.0/8 ", "10.200.3.4", "11.0.0.1"),
            ("10.9.9.9/8", "10.0.0.1", "11.0.0.1"),
            ("::1", "::1", "::2"),
            ("fd00::/8", "fd12:3456::1", "fe80::1"),
            ("::ffff:127.0.0.1", "127.0.0.1", "127.0.0.2"),
        ] {
            let proxy = named.parse::<TrustedProxy>().unwrap();
            assert!(proxy.names(address(among)), "{named} names {among}");
            assert!(
                !proxy.names(address(apart)),
                "{named} does not name {apart}"
            );
        }
        for named in [
            "",
            "localhost",
            "proxy.example.com",
            "10.0.0.0/33",
            "10.0.0",
            "10.0.0.5:8080",
        ] {
            let refused = named.parse::<TrustedProxy>().unwrap_err();
            assert!(
                refused.to_string().contains("neither an address"),
                "{refused}"
            );
        }
    }

    #[test]
    fn the_header_is_ignored_unless_the_peer_is_a_named_proxy() {
        assert_eq!(
            forwarded("192.0.2.1", &[b"203.0.113.7"], &[]),
            address("192.0.2.1"),
            "no proxy is named by default"
        );
        assert_eq!(
            forwarded("192.0.2.1", &[b"203.0.113.7"], &["192.0.2.2", "10.0.0.0/8"]),
            address("192.0.2.1")
        );
        assert_eq!(
            forwarded("192.0.2.1", &[b"10.0.0.1, 203.0.113.7"], &["10.0.0.0/8"]),
            address("192.0.2.1"),
            "a header naming a named proxy is believed only from one"
        );
    }

    #[test]
    fn a_named_proxy_is_believed_about_the_address_it_was_reached_from() {
        assert_eq!(
            forwarded("192.0.2.1", &[b"203.0.113.7"], &["192.0.2.1"]),
            address("203.0.113.7")
        );
        assert_eq!(
            forwarded(
                "192.0.2.1",
                &[b"198.51.100.1, 6.6.6.6 , 203.0.113.7"],
                &["192.0.2.1"]
            ),
            address("203.0.113.7"),
            "what the Server wrote itself, beyond the nearest hop, is passed over"
        );
    }

    #[test]
    fn several_named_proxies_are_each_believed_about_the_hop_before_them() {
        let named = ["192.0.2.1", "10.0.0.0/8"];
        assert_eq!(
            forwarded(
                "192.0.2.1",
                &[b"6.6.6.6, 203.0.113.7, 10.0.0.2, 10.0.0.3"],
                &named
            ),
            address("203.0.113.7")
        );
        assert_eq!(
            forwarded(
                "192.0.2.1",
                &[b"6.6.6.6, 203.0.113.7", b"10.0.0.2", b"10.0.0.3"],
                &named
            ),
            address("203.0.113.7"),
            "the header's lines are one list, in order"
        );
        assert_eq!(
            forwarded("192.0.2.1", &[b"10.0.0.2, 10.0.0.3"], &named),
            address("10.0.0.2"),
            "where every hop is a named proxy, the furthest is taken"
        );
        assert_eq!(
            forwarded("192.0.2.1", &[], &named),
            address("192.0.2.1"),
            "a named proxy naming nothing is taken at its own address"
        );
    }

    #[test]
    fn addresses_are_read_however_a_proxy_writes_them() {
        for (written, read) in [
            ("2001:db8::7", "2001:db8::7"),
            ("[2001:db8::7]", "2001:db8::7"),
            ("[2001:db8::7]:4711", "2001:db8::7"),
            ("203.0.113.7:4711", "203.0.113.7"),
            ("::ffff:203.0.113.7", "203.0.113.7"),
            ("  203.0.113.7\t", "203.0.113.7"),
        ] {
            assert_eq!(
                forwarded("192.0.2.1", &[written.as_bytes()], &["192.0.2.1"]),
                address(read),
                "{written:?}"
            );
        }
        assert_eq!(
            forwarded("::ffff:192.0.2.1", &[b"203.0.113.7"], &["192.0.2.1"]),
            address("203.0.113.7"),
            "a proxy reaching a dual-stack listener over IPv4 is the proxy named"
        );
        assert_eq!(
            forwarded("192.0.2.1", &[b"203.0.113.7,, ,"], &["192.0.2.1"]),
            address("203.0.113.7"),
            "empty hops are passed over"
        );
    }

    #[test]
    fn nothing_beyond_a_hop_that_is_no_address_is_believed() {
        let named = ["192.0.2.1", "10.0.0.0/8"];
        for lines in [
            &[b"203.0.113.7, unknown".as_slice()][..],
            &[b"203.0.113.7, _hidden"],
            &[b"203.0.113.7, fe80::1%eth0"],
            &[b"203.0.113.7", b"\xff\xfe"],
            &[b"203.0.113.7, 10.0.0.2", b"garbage"],
        ] {
            assert_eq!(
                forwarded("192.0.2.1", lines, &named),
                address("192.0.2.1"),
                "{:?}",
                lines
                    .iter()
                    .map(|line| String::from_utf8_lossy(line))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            forwarded("192.0.2.1", &[b"203.0.113.7, garbage, 10.0.0.2"], &named),
            address("10.0.0.2"),
            "the address is the nearest a named proxy vouches for"
        );
    }
}
