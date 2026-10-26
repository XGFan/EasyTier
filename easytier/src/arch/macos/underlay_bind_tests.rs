//! Locks the macOS socket-scoping behaviour the full tunnel depends on.
//!
//! Each of these breaks silently: the code still compiles and every other
//! test passes, while underlay sockets quietly fall back to the routing table
//! and loop into the full-tunnel TUN.
//!
//! - `tunnel::common::setup_socket2_ext` must apply IP_BOUND_IF to wildcard
//!   binds. Underlay sockets are all bound to 0.0.0.0, so an early return for
//!   unspecified addresses (upstream had one until v2.7.0) would leave every
//!   one of them unscoped.
//! - The host socket factories must consult `underlay` for every socket they
//!   create. A new creation path that skips it leaves its sockets unscoped.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd as _, RawFd};

use easytier_core::socket::{
    tcp::TcpConnectOptions,
    udp::{UdpBindOptions, UdpSocketPurpose},
};
use tokio::net::UdpSocket;

use super::underlay::UnderlayIfaceOverride;
use crate::socket::{tcp::connect_tcp, udp::create_udp_socket};
use crate::tunnel::common::{BindDev, bind};

/// The interface index a socket is scoped to, 0 when it is unscoped.
fn bound_if_index(fd: RawFd) -> u32 {
    let mut index: nix::libc::c_uint = 0;
    let mut len = size_of::<nix::libc::c_uint>() as nix::libc::socklen_t;
    let ret = unsafe {
        nix::libc::getsockopt(
            fd,
            nix::libc::IPPROTO_IP,
            nix::libc::IP_BOUND_IF,
            &raw mut index as *mut nix::libc::c_void,
            &mut len,
        )
    };
    assert_eq!(
        ret,
        0,
        "getsockopt(IP_BOUND_IF) failed: {}",
        std::io::Error::last_os_error()
    );
    index
}

fn wildcard() -> SocketAddr {
    "0.0.0.0:0".parse().unwrap()
}

/// lo0 always exists and needs no privileges to scope to.
fn lo0_index() -> u32 {
    let index = unsafe { nix::libc::if_nametoindex(c"lo0".as_ptr()) };
    assert_ne!(index, 0, "lo0 not found");
    index
}

#[tokio::test]
async fn wildcard_bind_is_still_scoped_to_the_device() {
    let socket: UdpSocket = bind::<UdpSocket>()
        .addr(wildcard())
        .dev(BindDev::Custom("lo0".to_string()))
        .call()
        .await
        .unwrap();

    assert_eq!(
        bound_if_index(socket.as_raw_fd()),
        lo0_index(),
        "a wildcard-bound socket was left unscoped: underlay traffic would be \
         captured by the full-tunnel TUN routes. Check that setup_socket2_ext \
         still applies IP_BOUND_IF for unspecified bind addresses."
    );
}

#[tokio::test]
async fn bind_without_a_device_stays_unscoped() {
    let socket: UdpSocket = bind::<UdpSocket>()
        .addr(wildcard())
        .dev(BindDev::Disabled)
        .call()
        .await
        .unwrap();

    assert_eq!(bound_if_index(socket.as_raw_fd()), 0);
}

#[tokio::test]
async fn udp_factory_scopes_underlay_sockets() {
    let _g = UnderlayIfaceOverride::set("lo0");

    for purpose in [
        UdpBindOptions::stun_probe(),
        UdpBindOptions::hole_punch_candidate(),
        UdpBindOptions::hole_punch_control(),
        UdpBindOptions::direct_connect(),
    ] {
        let socket = create_udp_socket(&purpose).await.unwrap();
        assert_eq!(
            bound_if_index(socket.as_raw_fd()),
            lo0_index(),
            "{:?} socket from the host UDP factory is unscoped: check that \
             socket::udp::RuntimeUdpSocketFactory::bind_device_for still asks \
             arch::macos::underlay::underlay_udp_device",
            purpose.purpose,
        );
    }

    let listener = create_udp_socket(&UdpBindOptions::port_bound_listener(wildcard()))
        .await
        .unwrap();
    assert_eq!(
        bound_if_index(listener.as_raw_fd()),
        0,
        "{:?} must stay unscoped: IP_BOUND_IF would restrict what it receives",
        UdpSocketPurpose::PortBoundListener,
    );
}

#[tokio::test]
async fn tcp_factory_scopes_underlay_connects() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });

    // RuntimeTcpSocket exposes no fd, so watch the effect instead: scoping to
    // a missing interface makes the socket setup fail, an unscoped socket
    // connects fine.
    let _g = UnderlayIfaceOverride::set("et-missing0");

    let scoped = connect_tcp(TcpConnectOptions::stun_probe(remote, wildcard())).await;
    assert!(
        scoped
            .as_ref()
            .is_err_and(|e| e.to_string().contains("et-missing0")),
        "STUN connect from the host TCP factory is unscoped: check that \
         socket::tcp::connect_tcp still asks \
         arch::macos::underlay::underlay_tcp_device (got {:?})",
        scoped.map(|_| ()),
    );

    assert!(
        connect_tcp(TcpConnectOptions::direct_connect(remote))
            .await
            .is_ok(),
        "direct connects are not meant to be scoped"
    );
}
