//! Locks the macOS socket-scoping behaviour the full tunnel depends on.
//!
//! `tunnel::common::setup_socket2_ext` applies IP_BOUND_IF *before* its early
//! return for wildcard bind addresses. That ordering is the whole reason
//! underlay sockets can escape the split default routes — they are all bound
//! to 0.0.0.0, so an early return above the IP_BOUND_IF block would leave
//! every one of them unscoped. Such a change compiles, passes every other
//! test, and silently reopens the routing loop; hence this test.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd as _, RawFd};

use tokio::net::UdpSocket;

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
        .unwrap();

    assert_eq!(
        bound_if_index(socket.as_raw_fd()),
        lo0_index(),
        "a wildcard-bound socket was left unscoped: underlay traffic would be \
         captured by the full-tunnel TUN routes. Check that setup_socket2_ext \
         still applies IP_BOUND_IF before returning early for unspecified \
         bind addresses."
    );
}

#[tokio::test]
async fn bind_without_a_device_stays_unscoped() {
    let socket: UdpSocket = bind::<UdpSocket>()
        .addr(wildcard())
        .dev(BindDev::Disabled)
        .call()
        .unwrap();

    assert_eq!(bound_if_index(socket.as_raw_fd()), 0);
}
