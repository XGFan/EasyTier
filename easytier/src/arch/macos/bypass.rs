//! Underlay bypass host routes for the macOS full tunnel.
//!
//! While the split default routes (see [`super::split_route`]) point the whole
//! address space at the TUN, easytier's own underlay packets must keep taking
//! the physical interface. Interface-bound sockets handle that for sockets we
//! create ourselves (see `super::underlay`); these /32 routes cover
//! the rest.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::Arc;

use easytier_core::instance::CorePacketPlane;

use crate::common::error::Error;
use crate::common::global_ctx::ArcGlobalCtx;
use crate::proto::common::TunnelInfo;

pub(crate) const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Exact /32 lookup result used for ownership checks before deletion.
pub(crate) enum HostRouteQuery {
    /// No /32 entry exists for this destination.
    Missing,
    /// An entry exists; `gateway` is None for non-gateway (interface /
    /// on-link) forms, which are by definition not ours.
    Entry { gateway: Option<Ipv4Addr> },
}

/// Routing-table operations behind the bypass manager, abstracted so the
/// ownership/retry state machine below is unit-testable without touching
/// the real routing table.
pub(crate) trait RouteOps {
    async fn add_host_route_via_gateway(
        &self,
        ip: Ipv4Addr,
        gateway: Ipv4Addr,
    ) -> Result<(), Error>;
    async fn remove_host_route(&self, ip: Ipv4Addr) -> Result<(), Error>;
    async fn query_host_route(&self, ip: Ipv4Addr) -> HostRouteQuery;
}

pub(crate) struct SysRouteOps;

impl RouteOps for SysRouteOps {
    async fn add_host_route_via_gateway(
        &self,
        ip: Ipv4Addr,
        gateway: Ipv4Addr,
    ) -> Result<(), Error> {
        let ifcfg = crate::common::ifcfg::IfConfiger {};
        ifcfg.add_ipv4_route_via_gateway(ip, 32, gateway).await
    }
    async fn remove_host_route(&self, ip: Ipv4Addr) -> Result<(), Error> {
        let ifcfg = crate::common::ifcfg::IfConfiger {};
        ifcfg.remove_ipv4_route_any(ip, 32).await
    }
    async fn query_host_route(&self, ip: Ipv4Addr) -> HostRouteQuery {
        let ifcfg = crate::common::ifcfg::IfConfiger {};
        match ifcfg.query_ipv4_route_exact(ip, 32).await {
            None => HostRouteQuery::Missing,
            Some(entry) => HostRouteQuery::Entry {
                gateway: entry.gateway,
            },
        }
    }
}

/// Host routes (/32 via the physical gateway) that let easytier's own
/// underlay traffic escape the broad split default routes installed for
/// full tunnel (`super::split_route::SPLIT_DEFAULT_ROUTES`). Without them,
/// packets to peer endpoints would be captured by the TUN and loop back into
/// easytier.
///
/// Ownership rules: only routes this instance successfully installed are
/// deleted, and deletion re-checks that the table still shows the gateway
/// we recorded. An identical /32 owned by someone else (another VPN
/// protecting the same endpoint) is never torn down — deleting it would
/// pull that software's underlay traffic into our tunnel.
pub(crate) struct BypassRouteManager<O: RouteOps> {
    ops: O,
    installed: BTreeMap<Ipv4Addr, Ipv4Addr>, // dst ip -> gateway we installed
}

impl<O: RouteOps> BypassRouteManager<O> {
    pub(crate) fn new(ops: O) -> Self {
        Self {
            ops,
            installed: BTreeMap::new(),
        }
    }

    pub(crate) async fn sync(&mut self, desired: &BTreeSet<Ipv4Addr>, gateway: Ipv4Addr) {
        let stale: Vec<(Ipv4Addr, Ipv4Addr)> = self
            .installed
            .iter()
            .filter(|(ip, gw)| !desired.contains(ip) || **gw != gateway)
            .map(|(ip, gw)| (*ip, *gw))
            .collect();
        for (ip, gw) in stale {
            self.remove_one(ip, gw).await;
        }
        for ip in desired {
            self.add_one(*ip, gateway).await;
        }
    }

    pub(crate) async fn add_one(&mut self, ip: Ipv4Addr, gateway: Ipv4Addr) {
        if let Some(old_gw) = self.installed.get(&ip).copied() {
            if old_gw == gateway {
                return;
            }
            if !self.remove_one(ip, old_gw).await {
                // the old entry could not be removed; keep it on the books
                // and let a later reconcile retry the gateway switch
                return;
            }
        }
        match self.ops.add_host_route_via_gateway(ip, gateway).await {
            Ok(()) => {
                tracing::info!(?ip, ?gateway, "added underlay bypass route");
                self.installed.insert(ip, gateway);
            }
            Err(e) => {
                // do NOT track failed adds: an EEXIST here may be another
                // VPN's bypass for the same endpoint, which must never be
                // deleted by us. The cost is that a /32 leaked by a
                // crashed previous run is not adopted; that leftover is
                // harmless (it points at the physical gateway) and clears
                // on reboot.
                tracing::warn!(
                    ?ip,
                    ?gateway,
                    ?e,
                    "failed to add bypass route, leaving any existing entry alone"
                );
            }
        }
    }

    /// Remove `ip`'s /32 if the table still shows the gateway we
    /// installed. Returns true when the entry is off our books (deleted,
    /// already gone, or replaced by a foreign route), false when deletion
    /// failed and should be retried by a later reconcile.
    async fn remove_one(&mut self, ip: Ipv4Addr, expected_gw: Ipv4Addr) -> bool {
        match self.ops.query_host_route(ip).await {
            HostRouteQuery::Missing => {
                self.installed.remove(&ip);
                true
            }
            HostRouteQuery::Entry { gateway: Some(gw) } if gw == expected_gw => {
                match self.ops.remove_host_route(ip).await {
                    Ok(()) => {
                        self.installed.remove(&ip);
                        true
                    }
                    Err(e) => {
                        tracing::warn!(
                            ?ip,
                            ?expected_gw,
                            ?e,
                            "failed to remove bypass route, will retry"
                        );
                        false
                    }
                }
            }
            HostRouteQuery::Entry { gateway } => {
                // not the entry we installed (replaced or foreign): drop
                // it from our books without deleting
                tracing::warn!(
                    ?ip,
                    ?expected_gw,
                    current_gateway = ?gateway,
                    "bypass route replaced by a foreign entry, leaving it alone"
                );
                self.installed.remove(&ip);
                true
            }
        }
    }

    /// Best-effort removal of everything we installed. Entries whose
    /// deletion fails stay on the books, so a later reconcile (or the
    /// next `clear`) retries them.
    pub(crate) async fn clear(&mut self) {
        let entries: Vec<(Ipv4Addr, Ipv4Addr)> =
            self.installed.iter().map(|(ip, gw)| (*ip, *gw)).collect();
        for (ip, gw) in entries {
            self.remove_one(ip, gw).await;
        }
        if !self.installed.is_empty() {
            tracing::warn!(remaining = ?self.installed, "some bypass routes could not be removed");
        }
    }
}

/// Resolve a URI to IPv4 addresses (supports IP literals and hostname DNS resolution)
async fn resolve_uri_to_ipv4s(uri: &url::Url) -> Vec<Ipv4Addr> {
    let mut ips = Vec::new();
    if let Some(host) = uri.host_str() {
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            ips.push(ip);
            return ips;
        }
        if let Ok(addrs) = tokio::net::lookup_host(format!("{}:0", host)).await {
            for addr in addrs {
                if let std::net::SocketAddr::V4(v4) = addr {
                    ips.push(*v4.ip());
                }
            }
        }
    }
    ips
}

/// Extract the underlay remote IPv4 of a peer connection's tunnel (the
/// on-wire endpoint, i.e. the hole-punched address for punched conns).
pub(crate) fn conn_remote_ipv4(tunnel: Option<&TunnelInfo>) -> Option<Ipv4Addr> {
    let url = tunnel?.effective_remote_addr()?;
    let parsed = url::Url::parse(&url.url).ok()?;
    // note: for non-special schemes like udp:// the url crate reports the
    // host as an opaque domain even when it is an IP literal, so parse the
    // host string instead of matching url::Host::Ipv4
    parsed.host_str()?.parse().ok()
}

/// IPv4 subnets directly connected on physical (non-TUN) interfaces. IPs
/// inside them are reachable via connected routes that already beat the /1
/// TUN routes, so they need no bypass.
pub(crate) fn physical_onlink_v4_subnets() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    use network_interface::NetworkInterfaceConfig;
    let mut subnets = vec![];
    let Ok(ifaces) = network_interface::NetworkInterface::show() else {
        return subnets;
    };
    for iface in ifaces {
        if iface.name.starts_with("utun") || iface.name.starts_with("lo") {
            continue;
        }
        for addr in iface.addr {
            if let network_interface::Addr::V4(v4) = addr
                && let Some(netmask) = v4.netmask
            {
                subnets.push((v4.ip, netmask));
            }
        }
    }
    subnets
}

fn is_in_any_v4_subnet(ip: Ipv4Addr, subnets: &[(Ipv4Addr, Ipv4Addr)]) -> bool {
    subnets
        .iter()
        .any(|(addr, mask)| u32::from(ip) & u32::from(*mask) == u32::from(*addr) & u32::from(*mask))
}

pub(crate) fn is_bypass_candidate_ipv4(
    ip: Ipv4Addr,
    onlink_subnets: &[(Ipv4Addr, Ipv4Addr)],
    global_ctx: &ArcGlobalCtx,
) -> bool {
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !is_in_any_v4_subnet(ip, onlink_subnets)
        && global_ctx
            .get_ipv4()
            .is_none_or(|net| !net.network().contains(&ip))
}

/// All remote IPv4 endpoints easytier's underlay may talk to: configured
/// peers and live peer connections (including hole-punched endpoints that
/// appear in no config). STUN probes need no bypass — their sockets are
/// interface-bound (see `super::underlay`).
async fn collect_underlay_bypass_ips(
    global_ctx: &ArcGlobalCtx,
    packet_plane: &Arc<CorePacketPlane>,
) -> BTreeSet<Ipv4Addr> {
    let mut ips = BTreeSet::new();

    for peer in global_ctx.config.get_peers() {
        let resolved = resolve_uri_to_ipv4s(&peer.uri).await;
        if resolved.is_empty() {
            tracing::warn!(uri = %peer.uri, "failed to resolve peer URI for bypass route");
        }
        ips.extend(resolved);
    }

    for conn in packet_plane.list_peer_conns().await {
        ips.extend(conn_remote_ipv4(conn.tunnel.as_ref()));
    }

    let onlink_subnets = physical_onlink_v4_subnets();
    ips.retain(|ip| is_bypass_candidate_ipv4(*ip, &onlink_subnets, global_ctx));
    ips
}

/// Reconcile bypass host routes with the current desired endpoint set. Only
/// active while broad TUN routes (prefix <= 1) are installed or about to be.
pub(crate) async fn sync_bypass_routes(
    bypass: &mut BypassRouteManager<impl RouteOps>,
    cur_proxy_cidrs: &BTreeSet<cidr::Ipv4Cidr>,
    pending_added: &[cidr::Ipv4Cidr],
    global_ctx: &ArcGlobalCtx,
    packet_plane: &Arc<CorePacketPlane>,
) {
    let full_tunnel = cur_proxy_cidrs
        .iter()
        .chain(pending_added.iter())
        .any(|c| c.network_length() <= 1);
    if !full_tunnel {
        bypass.clear().await;
        return;
    }

    let Some(route) = super::get_default_route_v4() else {
        // transient loss of the default route (e.g. wifi roaming) should not
        // tear down routes that may come back into effect
        tracing::warn!("no physical default route, skip bypass route sync");
        return;
    };
    let ips = collect_underlay_bypass_ips(global_ctx, packet_plane).await;
    tracing::debug!(?ips, gateway = ?route.gateway, "syncing underlay bypass routes");
    bypass.sync(&ips, route.gateway).await;
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeOps {
        // simulated routing table: ip -> gateway (None = non-gateway entry)
        table: Mutex<BTreeMap<Ipv4Addr, Option<Ipv4Addr>>>,
        fail_remove: Mutex<std::collections::BTreeSet<Ipv4Addr>>,
        removed: Mutex<Vec<Ipv4Addr>>,
    }

    impl RouteOps for FakeOps {
        async fn add_host_route_via_gateway(
            &self,
            ip: Ipv4Addr,
            gateway: Ipv4Addr,
        ) -> Result<(), Error> {
            let mut table = self.table.lock().unwrap();
            if table.contains_key(&ip) {
                return Err(Error::ShellCommandError("File exists".into()));
            }
            table.insert(ip, Some(gateway));
            Ok(())
        }
        async fn remove_host_route(&self, ip: Ipv4Addr) -> Result<(), Error> {
            if self.fail_remove.lock().unwrap().contains(&ip) {
                return Err(Error::ShellCommandError("simulated failure".into()));
            }
            self.table.lock().unwrap().remove(&ip);
            self.removed.lock().unwrap().push(ip);
            Ok(())
        }
        async fn query_host_route(&self, ip: Ipv4Addr) -> HostRouteQuery {
            match self.table.lock().unwrap().get(&ip) {
                None => HostRouteQuery::Missing,
                Some(gw) => HostRouteQuery::Entry { gateway: *gw },
            }
        }
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn eexist_add_is_not_tracked_and_never_deleted() {
        let ops = FakeOps::default();
        // someone else's /32 for the same endpoint pre-exists
        ops.table
            .lock()
            .unwrap()
            .insert(ip("1.2.3.4"), Some(ip("10.0.0.1")));
        let mut mgr = BypassRouteManager::new(ops);
        mgr.add_one(ip("1.2.3.4"), ip("192.168.0.1")).await;
        assert!(mgr.installed.is_empty());
        mgr.clear().await;
        assert!(mgr.ops.removed.lock().unwrap().is_empty());
        assert!(mgr.ops.table.lock().unwrap().contains_key(&ip("1.2.3.4")));
    }

    #[tokio::test]
    async fn failed_removal_is_retained_and_retried() {
        let mut mgr = BypassRouteManager::new(FakeOps::default());
        mgr.add_one(ip("1.2.3.4"), ip("192.168.0.1")).await;
        mgr.ops.fail_remove.lock().unwrap().insert(ip("1.2.3.4"));
        mgr.sync(&BTreeSet::new(), ip("192.168.0.1")).await;
        assert_eq!(mgr.installed.len(), 1); // kept for retry
        mgr.ops.fail_remove.lock().unwrap().clear();
        mgr.sync(&BTreeSet::new(), ip("192.168.0.1")).await;
        assert!(mgr.installed.is_empty());
        assert!(!mgr.ops.table.lock().unwrap().contains_key(&ip("1.2.3.4")));
    }

    #[tokio::test]
    async fn foreign_replacement_is_not_deleted() {
        let mut mgr = BypassRouteManager::new(FakeOps::default());
        mgr.add_one(ip("1.2.3.4"), ip("192.168.0.1")).await;
        // someone replaced our route with theirs
        mgr.ops
            .table
            .lock()
            .unwrap()
            .insert(ip("1.2.3.4"), Some(ip("10.9.9.9")));
        mgr.clear().await;
        assert!(mgr.installed.is_empty()); // off the books...
        assert!(mgr.ops.table.lock().unwrap().contains_key(&ip("1.2.3.4"))); // ...but not deleted
    }

    #[tokio::test]
    async fn gateway_change_reinstalls_route() {
        let mut mgr = BypassRouteManager::new(FakeOps::default());
        let desired: BTreeSet<Ipv4Addr> = [ip("1.2.3.4")].into();
        mgr.sync(&desired, ip("192.168.0.1")).await;
        mgr.sync(&desired, ip("192.168.5.1")).await;
        assert_eq!(mgr.installed.get(&ip("1.2.3.4")), Some(&ip("192.168.5.1")));
        assert_eq!(
            mgr.ops.table.lock().unwrap().get(&ip("1.2.3.4")),
            Some(&Some(ip("192.168.5.1")))
        );
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn test_is_in_any_v4_subnet() {
        let subnets = vec![
            (
                "192.168.0.20".parse().unwrap(),
                "255.255.255.0".parse().unwrap(),
            ),
            ("10.0.0.1".parse().unwrap(), "255.0.0.0".parse().unwrap()),
        ];
        assert!(is_in_any_v4_subnet(
            "192.168.0.1".parse().unwrap(),
            &subnets
        ));
        assert!(is_in_any_v4_subnet("10.200.1.1".parse().unwrap(), &subnets));
        assert!(!is_in_any_v4_subnet(
            "192.168.2.1".parse().unwrap(),
            &subnets
        ));
        assert!(!is_in_any_v4_subnet(
            "220.203.172.102".parse().unwrap(),
            &subnets
        ));
    }

    #[test]
    fn test_conn_remote_ipv4() {
        use crate::proto::common::Url;

        let mk = |remote: Option<&str>, resolved: Option<&str>| TunnelInfo {
            tunnel_type: "udp".to_string(),
            local_addr: None,
            remote_addr: remote.map(|u| Url { url: u.to_string() }),
            resolved_remote_addr: resolved.map(|u| Url { url: u.to_string() }),
        };

        // resolved addr (the on-wire endpoint) wins over the configured one
        assert_eq!(
            conn_remote_ipv4(Some(&mk(
                Some("udp://example.com:11010"),
                Some("udp://220.203.172.102:23456")
            ))),
            Some("220.203.172.102".parse().unwrap())
        );
        assert_eq!(
            conn_remote_ipv4(Some(&mk(Some("tcp://8.219.8.190:11010"), None))),
            Some("8.219.8.190".parse().unwrap())
        );
        // hostname-only url has no ipv4 to bypass
        assert_eq!(
            conn_remote_ipv4(Some(&mk(Some("tcp://example.com:11010"), None))),
            None
        );
        assert_eq!(conn_remote_ipv4(None), None);
    }
}
