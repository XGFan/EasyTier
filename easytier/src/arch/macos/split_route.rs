//! Replacing 0.0.0.0/0 on the TUN with a split route set.
//!
//! Adding 0.0.0.0/0 on macOS conflicts with the existing system default route,
//! so full tunnel installs the clash/mihomo-style split instead.

use std::net::Ipv4Addr;

use crate::common::error::Error;
use crate::common::ifcfg::{IfConfiger, IfConfiguerTrait as _};

/// The clash/mihomo-style default-route split: (first octet, prefix length).
/// Covers 1.0.0.0-255.255.255.255; 0.0.0.0/8 is reserved and deliberately
/// skipped so no route has destination 0.0.0.0 (see [`install`]).
pub(crate) const SPLIT_DEFAULT_ROUTES: [(u8, u8); 8] = [
    (1, 8),
    (2, 7),
    (4, 6),
    (8, 5),
    (16, 4),
    (32, 3),
    (64, 2),
    (128, 1),
];

/// True for the 0.0.0.0/0 cidr that [`install`] replaces with the split set.
pub(crate) fn is_default_route(cidr: &cidr::Ipv4Cidr) -> bool {
    cidr.network_length() == 0 && cidr.first_address() == Ipv4Addr::UNSPECIFIED
}

/// Routing-table operations behind the split route installer, abstracted so
/// the all-or-nothing rollback is unit-testable without touching the real
/// routing table.
pub(crate) trait SplitRouteOps {
    async fn add_via_gateway(
        &self,
        dst: Ipv4Addr,
        prefix: u8,
        gateway: Ipv4Addr,
    ) -> Result<(), Error>;
    async fn add_via_iface(&self, dst: Ipv4Addr, prefix: u8) -> Result<(), Error>;
    /// Delete `dst/prefix`, but only if the routing table still shows it as
    /// ours (see [`remove_route_if_on_iface`]).
    async fn remove_if_ours(&self, dst: Ipv4Addr, prefix: u8);
}

pub(crate) struct SysSplitRouteOps {
    pub(crate) ifname: String,
}

impl SplitRouteOps for SysSplitRouteOps {
    async fn add_via_gateway(
        &self,
        dst: Ipv4Addr,
        prefix: u8,
        gateway: Ipv4Addr,
    ) -> Result<(), Error> {
        IfConfiger::default()
            .add_ipv4_route_via_gateway(dst, prefix, gateway)
            .await
    }

    async fn add_via_iface(&self, dst: Ipv4Addr, prefix: u8) -> Result<(), Error> {
        IfConfiger::default()
            .add_ipv4_route(&self.ifname, dst, prefix, None)
            .await
    }

    async fn remove_if_ours(&self, dst: Ipv4Addr, prefix: u8) {
        remove_route_if_on_iface(dst, prefix, &self.ifname).await
    }
}

/// Install the split default route set.
///
/// Two hard-won constraints from live debugging:
///  - any route whose destination is 0.0.0.0 (e.g. 0.0.0.0/1) is marked
///    RTF_GLOBAL by the kernel and breaks sends from IP_BOUND_IF-scoped
///    underlay sockets (EHOSTUNREACH), killing STUN and hole punching — hence
///    the split starts at 1.0.0.0/8 and skips the reserved 0.0.0.0/8;
///  - gateway-form routes (via the TUN's own address) are used to match the
///    empirically verified working configuration.
///
/// Returns false when the set could not be installed in full. It is rolled
/// back first: half a split default route silently un-tunnels part of the
/// address space, which is worse than a fully absent one. The caller must
/// leave 0.0.0.0/0 unrecorded so a later reconcile retries the whole set.
pub(crate) async fn install<O: SplitRouteOps>(ops: &O, tun_ipv4: Option<Ipv4Addr>) -> bool {
    let Some(tun_ip) = tun_ipv4 else {
        for (octet, prefix) in SPLIT_DEFAULT_ROUTES {
            let _ = ops
                .add_via_iface(Ipv4Addr::new(octet, 0, 0, 0), prefix)
                .await;
        }
        tracing::warn!("macOS: tun has no ipv4, split default routes use interface form");
        return true;
    };

    let mut installed = vec![];
    for (octet, prefix) in SPLIT_DEFAULT_ROUTES {
        let dst = Ipv4Addr::new(octet, 0, 0, 0);
        match ops.add_via_gateway(dst, prefix, tun_ip).await {
            Ok(()) => installed.push((dst, prefix)),
            Err(e) => {
                tracing::warn!(
                    ?dst,
                    prefix,
                    ?e,
                    "macOS: split default route add failed, rolling back the set"
                );
                for (dst, prefix) in installed {
                    ops.remove_if_ours(dst, prefix).await;
                }
                return false;
            }
        }
    }
    tracing::info!(
        ?tun_ip,
        "macOS: installed split default routes for TUN routing"
    );
    true
}

/// Remove the split default route set.
pub(crate) async fn remove<O: SplitRouteOps>(ops: &O) {
    tracing::info!("macOS: removing split default routes from TUN");
    for (octet, prefix) in SPLIT_DEFAULT_ROUTES {
        ops.remove_if_ours(Ipv4Addr::new(octet, 0, 0, 0), prefix)
            .await;
    }
    // also clean the old-style /1 split left by earlier builds
    ops.remove_if_ours(Ipv4Addr::UNSPECIFIED, 1).await;
}

/// Delete the exact route `dst/prefix` only if the routing table shows it on
/// our own TUN interface — true for both the gateway-form entries (the TUN's
/// own address resolves on the TUN) and the no-ipv4 interface-form fallback.
/// Never touches an identical route owned by someone else: another VPN's
/// clash-style split routes point at that VPN's own utun, and deleting them
/// would cut the machine off the network.
async fn remove_route_if_on_iface(dst: Ipv4Addr, prefix: u8, ifname: &str) {
    let mac_ifcfg = IfConfiger::default();
    match mac_ifcfg.query_ipv4_route_exact(dst, prefix).await {
        // no exact entry (or unknown shape): nothing of ours to remove
        None => {}
        Some(entry) if entry.iface.as_deref() == Some(ifname) => {
            if let Err(e) = mac_ifcfg.remove_ipv4_route_any(dst, prefix).await {
                tracing::warn!(?dst, prefix, ?e, "failed to remove split route");
            }
        }
        Some(entry) => {
            tracing::warn!(
                ?dst,
                prefix,
                iface = ?entry.iface,
                "route exists but is not on our TUN, leaving it alone"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeOps {
        /// routes currently in the table, as (dst, prefix)
        table: Mutex<BTreeSet<(Ipv4Addr, u8)>>,
        /// gateway-form adds fail once the table holds this many entries
        fail_gateway_add_after: Option<usize>,
        iface_form_adds: Mutex<Vec<(Ipv4Addr, u8)>>,
        remove_attempts: Mutex<Vec<(Ipv4Addr, u8)>>,
    }

    impl SplitRouteOps for FakeOps {
        async fn add_via_gateway(
            &self,
            dst: Ipv4Addr,
            prefix: u8,
            _gateway: Ipv4Addr,
        ) -> Result<(), Error> {
            let mut table = self.table.lock().unwrap();
            if self.fail_gateway_add_after == Some(table.len()) {
                return Err(Error::ShellCommandError("simulated failure".into()));
            }
            table.insert((dst, prefix));
            Ok(())
        }

        async fn add_via_iface(&self, dst: Ipv4Addr, prefix: u8) -> Result<(), Error> {
            self.iface_form_adds.lock().unwrap().push((dst, prefix));
            self.table.lock().unwrap().insert((dst, prefix));
            Ok(())
        }

        async fn remove_if_ours(&self, dst: Ipv4Addr, prefix: u8) {
            self.remove_attempts.lock().unwrap().push((dst, prefix));
            self.table.lock().unwrap().remove(&(dst, prefix));
        }
    }

    fn tun_ip() -> Option<Ipv4Addr> {
        Some("10.126.126.13".parse().unwrap())
    }

    #[tokio::test]
    async fn installs_the_whole_split_set() {
        let ops = FakeOps::default();
        assert!(install(&ops, tun_ip()).await);
        let table = ops.table.lock().unwrap();
        assert_eq!(table.len(), SPLIT_DEFAULT_ROUTES.len());
        for (octet, prefix) in SPLIT_DEFAULT_ROUTES {
            assert!(table.contains(&(Ipv4Addr::new(octet, 0, 0, 0), prefix)));
        }
        // nothing may target 0.0.0.0: the kernel marks such routes RTF_GLOBAL,
        // which breaks IP_BOUND_IF-scoped underlay sockets
        assert!(!table.iter().any(|(dst, _)| dst.is_unspecified()));
    }

    #[tokio::test]
    async fn partial_failure_rolls_the_whole_set_back() {
        let ops = FakeOps {
            fail_gateway_add_after: Some(3),
            ..Default::default()
        };
        assert!(!install(&ops, tun_ip()).await);
        // the three routes added before the failure are gone again
        assert!(ops.table.lock().unwrap().is_empty());
        assert_eq!(
            *ops.remove_attempts.lock().unwrap(),
            vec![
                (Ipv4Addr::new(1, 0, 0, 0), 8),
                (Ipv4Addr::new(2, 0, 0, 0), 7),
                (Ipv4Addr::new(4, 0, 0, 0), 6),
            ]
        );
    }

    #[tokio::test]
    async fn first_add_failing_leaves_nothing_behind() {
        let ops = FakeOps {
            fail_gateway_add_after: Some(0),
            ..Default::default()
        };
        assert!(!install(&ops, tun_ip()).await);
        assert!(ops.table.lock().unwrap().is_empty());
        assert!(ops.remove_attempts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_tun_ipv4_falls_back_to_interface_form() {
        let ops = FakeOps::default();
        assert!(install(&ops, None).await);
        assert_eq!(
            ops.iface_form_adds.lock().unwrap().len(),
            SPLIT_DEFAULT_ROUTES.len()
        );
    }

    #[tokio::test]
    async fn remove_clears_the_set_and_the_legacy_split() {
        let ops = FakeOps::default();
        install(&ops, tun_ip()).await;
        remove(&ops).await;
        assert!(ops.table.lock().unwrap().is_empty());
        // the 0.0.0.0/1 left by earlier builds is cleaned up too
        assert!(
            ops.remove_attempts
                .lock()
                .unwrap()
                .contains(&(Ipv4Addr::UNSPECIFIED, 1))
        );
    }
}
