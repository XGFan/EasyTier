//! Keeping easytier's own underlay sockets off TUN routes.
//!
//! Under full tunnel the split default routes (see `split_route`) point the
//! whole IPv4 space at the TUN. A wildcard-bound socket follows the routing
//! table, so peer-facing sockets — STUN probes, hole punch sockets,
//! direct-connect probes — would be captured by the TUN and loop back into
//! easytier. Scoping them with IP_BOUND_IF to the physical default-route
//! interface keeps them on the physical network.
//!
//! The host socket factories (`crate::socket::{udp, tcp}`) ask this module
//! which device, if any, a socket should be scoped to.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};

use easytier_core::socket::{
    tcp::{TcpBindOptions, TcpSocketPurpose},
    udp::{UdpBindOptions, UdpSocketPurpose},
};

// The instance `bind_device` flag (on by default); `--bind-device false`
// opts out of interface binding altogether. Process-wide because the socket
// factories carry no instance context: with several instances in one
// process, the last one created decides.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Follow the instance `bind_device` flag.
pub(crate) fn set_underlay_scoping(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether a UDP socket of this purpose carries easytier's own traffic to
/// remote peers. Listeners are excluded on purpose: IP_BOUND_IF also restricts
/// *receive* to the one interface, which would drop inbound traffic arriving
/// over loopback, the TUN or another NIC. Their replies are covered by the
/// bypass host routes instead.
///
/// `HolePunchControl` is also what `UdpBindOptions::default()` carries, so
/// it covers the route probes that pick a punch socket's source address
/// (`host_runtime::local_addr_for_remote`): unscoped, they would report the
/// TUN's IP while TUN routes are active and every punch would be refused.
///
/// No `_` arm: a purpose added upstream must be classified here.
fn is_underlay_udp(purpose: UdpSocketPurpose) -> bool {
    match purpose {
        UdpSocketPurpose::HolePunchControl
        | UdpSocketPurpose::HolePunchCandidate
        | UdpSocketPurpose::DirectConnect
        | UdpSocketPurpose::StunProbe => true,
        UdpSocketPurpose::PortBoundListener
        | UdpSocketPurpose::ProxyNat
        | UdpSocketPurpose::Socks5
        | UdpSocketPurpose::PortForward
        | UdpSocketPurpose::PortLease => false,
    }
}

/// TCP counterpart of [`is_underlay_udp`], for outgoing connections only.
/// Direct and manual connects race one socket per physical interface IP
/// (already interface-scoped) and their endpoints get bypass host routes, so
/// they keep upstream behaviour. TCP hole punching pairs each connect with a
/// listener on the same port; scoping only one half would split its traffic
/// across two paths, so it stays unscoped as a whole.
///
/// No `_` arm: a purpose added upstream must be classified here.
fn is_underlay_tcp(purpose: TcpSocketPurpose) -> bool {
    match purpose {
        TcpSocketPurpose::StunProbe => true,
        TcpSocketPurpose::HolePunch
        | TcpSocketPurpose::DirectConnect
        | TcpSocketPurpose::ManualConnect
        | TcpSocketPurpose::FakeTcp
        | TcpSocketPurpose::ProxyNat
        | TcpSocketPurpose::Socks5
        | TcpSocketPurpose::PortForward
        | TcpSocketPurpose::DataPlane => false,
    }
}

/// Only IPv4 wildcard binds are scoped: a concrete local IP already selects
/// its interface, and the full tunnel is IPv4-only.
fn is_ipv4_wildcard(addr: SocketAddr) -> bool {
    addr.is_ipv4() && addr.ip().is_unspecified()
}

/// The device a new UDP socket should be scoped to, or None to keep the
/// factory's own choice.
pub(crate) fn underlay_udp_device(options: &UdpBindOptions) -> Option<String> {
    let bind_addr = options
        .local_addr
        .unwrap_or_else(|| SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 0));
    if !options.need_protect
        || options.bind_device.is_some()
        || !is_ipv4_wildcard(bind_addr)
        || !is_underlay_udp(options.purpose)
    {
        return None;
    }
    physical_iface()
}

/// The device an outgoing TCP connection should be scoped to, or None to
/// keep the factory's own choice.
pub(crate) fn underlay_tcp_device(
    remote_addr: SocketAddr,
    purpose: TcpSocketPurpose,
    bind: &TcpBindOptions,
) -> Option<String> {
    let bind_addr = bind
        .local_addr
        .unwrap_or_else(|| SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 0));
    if !bind.need_protect
        || bind.bind_device.is_some()
        || !remote_addr.is_ipv4()
        || !is_ipv4_wildcard(bind_addr)
        || !is_underlay_tcp(purpose)
    {
        return None;
    }
    physical_iface()
}

/// The physical default-route interface, if it exists and can be scoped to.
/// None falls back to an unscoped socket rather than failing its creation.
fn physical_iface() -> Option<String> {
    #[cfg(test)]
    if let Some(iface) = TEST_IFACE.with(|i| i.borrow().clone()) {
        return Some(iface);
    }
    // unit tests talk over loopback, which a scoped socket cannot reach
    if cfg!(test) || !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let route = super::get_default_route_v4()?;
    let name = std::ffi::CString::new(route.iface.as_str()).ok()?;
    // the socket setup errors out on an unknown interface; check first so a
    // just-vanished uplink degrades to an unscoped socket instead
    if unsafe { nix::libc::if_nametoindex(name.as_ptr()) } == 0 {
        tracing::warn!(iface = %route.iface, "default route iface not found, underlay socket left unscoped");
        return None;
    }
    Some(route.iface)
}

/// True when `ip` is owned by a utun device: easytier's own TUN, another VPN,
/// or a stale tun. Such an IP is what the routing table hands out as "the
/// local address" while TUN routes are active, and must not be used as an
/// underlay source: connecting through another tunnel adds a detour at best
/// and a routing loop at worst.
pub(crate) fn is_tun_owned_ip(ip: IpAddr) -> bool {
    use network_interface::NetworkInterfaceConfig;
    let Ok(ifaces) = network_interface::NetworkInterface::show() else {
        return false;
    };
    ifaces
        .iter()
        .filter(|iface| iface.name.starts_with("utun"))
        .any(|iface| iface.addr.iter().any(|addr| addr.ip() == ip))
}

#[cfg(test)]
thread_local! {
    static TEST_IFACE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Scopes underlay sockets created on this thread to `iface` until dropped,
/// regardless of the process-wide switch. Tokio's default test runtime is
/// single-threaded, so this covers sockets created inside the test body.
#[cfg(test)]
pub(crate) struct UnderlayIfaceOverride(std::marker::PhantomData<*const ()>);

#[cfg(test)]
impl UnderlayIfaceOverride {
    pub(crate) fn set(iface: &str) -> Self {
        TEST_IFACE.with(|i| *i.borrow_mut() = Some(iface.to_string()));
        // !Send: the override is per thread and must be dropped where it was set
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
impl Drop for UnderlayIfaceOverride {
    fn drop(&mut self) {
        TEST_IFACE.with(|i| *i.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use easytier_core::socket::tcp::TcpConnectOptions;

    fn v4(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn disabled_by_default_under_test() {
        assert_eq!(underlay_udp_device(&UdpBindOptions::stun_probe()), None);
    }

    #[test]
    fn scopes_peer_facing_udp_wildcard_sockets() {
        let _g = UnderlayIfaceOverride::set("en9");
        for options in [
            UdpBindOptions::stun_probe(),
            UdpBindOptions::hole_punch_candidate(),
            UdpBindOptions::hole_punch_control(),
            // route probes for a punch socket's source address
            UdpBindOptions::default().with_local_addr(Some(v4("0.0.0.0:0"))),
            UdpBindOptions::direct_connect().with_local_addr(Some(v4("0.0.0.0:0"))),
            UdpBindOptions::hole_punch_candidate().with_local_addr(Some(v4("0.0.0.0:23456"))),
        ] {
            assert_eq!(
                underlay_udp_device(&options).as_deref(),
                Some("en9"),
                "{options:?}"
            );
        }
    }

    #[test]
    fn leaves_listeners_local_and_explicit_udp_sockets_alone() {
        let _g = UnderlayIfaceOverride::set("en9");
        for options in [
            // receive-restricting a listener would drop loopback/TUN inbound
            UdpBindOptions::port_bound_listener(v4("0.0.0.0:11010")),
            UdpBindOptions::proxy_nat(),
            UdpBindOptions::socks5(),
            // loopback control packets opt out of protection
            UdpBindOptions::hole_punch_control().with_need_protect(false),
            // the caller chose a device, or disabled binding with ""
            UdpBindOptions::stun_probe().with_bind_device(Some(String::new())),
            UdpBindOptions::stun_probe().with_bind_device(Some("en0".into())),
            // a concrete local IP already selects its interface
            UdpBindOptions::direct_connect().with_local_addr(Some(v4("192.168.1.2:0"))),
            // IPv6 is out of scope for the full tunnel
            UdpBindOptions::direct_connect().with_local_addr(Some("[::]:0".parse().unwrap())),
        ] {
            assert_eq!(underlay_udp_device(&options), None, "{options:?}");
        }
    }

    #[test]
    fn scopes_only_peer_facing_tcp_connects() {
        let _g = UnderlayIfaceOverride::set("en9");
        let remote = v4("203.0.113.7:3478");
        let scoped = |o: TcpConnectOptions| underlay_tcp_device(o.remote_addr, o.purpose, &o.bind);

        assert_eq!(
            scoped(TcpConnectOptions::stun_probe(remote, v4("0.0.0.0:40000"))).as_deref(),
            Some("en9")
        );

        assert_eq!(scoped(TcpConnectOptions::hole_punch(remote, None)), None);
        assert_eq!(scoped(TcpConnectOptions::direct_connect(remote)), None);
        assert_eq!(
            scoped(TcpConnectOptions::manual_connect(remote, None)),
            None
        );
        assert_eq!(scoped(TcpConnectOptions::proxy_nat(remote)), None);
        assert_eq!(
            scoped(TcpConnectOptions::stun_probe(remote, v4("192.168.1.2:0"))),
            None
        );
        assert_eq!(
            scoped(TcpConnectOptions::stun_probe(
                v4("[2001:db8::1]:1"),
                v4("[::]:0")
            )),
            None
        );
    }
}
