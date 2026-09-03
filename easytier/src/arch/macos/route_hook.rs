use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::{bypass, split_route};
use crate::arch::route_hook::{ProxyRouteHook, RouteHookResult};
use crate::common::global_ctx::ArcGlobalCtx;
use crate::peers::peer_manager::PeerManager;
use crate::proto::api::instance::PeerConnInfo;

/// macOS full-tunnel routing: 0.0.0.0/0 on the TUN is replaced by the split
/// default route set, and easytier's own underlay endpoints are protected
/// from it by /32 bypass routes.
pub(crate) struct MacProxyRouteHook {
    global_ctx: ArcGlobalCtx,
    peer_mgr: Arc<PeerManager>,
    split_ops: split_route::SysSplitRouteOps,
    bypass: bypass::BypassRouteManager<bypass::SysRouteOps>,
}

impl ProxyRouteHook for MacProxyRouteHook {
    fn new(global_ctx: ArcGlobalCtx, peer_mgr: Arc<PeerManager>, ifname: String) -> Self {
        Self {
            global_ctx,
            peer_mgr,
            split_ops: split_route::SysSplitRouteOps { ifname },
            bypass: bypass::BypassRouteManager::new(bypass::SysRouteOps),
        }
    }

    fn reconcile_interval(&self) -> Option<Duration> {
        // also retries route changes that previously failed (e.g. a
        // rolled-back split default route set)
        Some(bypass::RECONCILE_INTERVAL)
    }

    async fn try_add_route(&mut self, cidr: &cidr::Ipv4Cidr) -> RouteHookResult {
        if !split_route::is_default_route(cidr) {
            return RouteHookResult::NotHandled;
        }
        let tun_ipv4 = self.global_ctx.get_ipv4().map(|ip| ip.address());
        if split_route::install(&self.split_ops, tun_ipv4).await {
            RouteHookResult::Done
        } else {
            RouteHookResult::Deferred
        }
    }

    async fn try_remove_route(&mut self, cidr: &cidr::Ipv4Cidr) -> RouteHookResult {
        if !split_route::is_default_route(cidr) {
            return RouteHookResult::NotHandled;
        }
        split_route::remove(&self.split_ops).await;
        RouteHookResult::Done
    }

    async fn reconcile(
        &mut self,
        cur: &BTreeSet<cidr::Ipv4Cidr>,
        pending_added: &[cidr::Ipv4Cidr],
    ) {
        bypass::sync_bypass_routes(
            &mut self.bypass,
            cur,
            pending_added,
            &self.global_ctx,
            &self.peer_mgr,
        )
        .await;
    }

    async fn on_peer_conn_added(&mut self, cur: &BTreeSet<cidr::Ipv4Cidr>, conn: &PeerConnInfo) {
        // a fresh underlay conn (e.g. a just-punched p2p endpoint) must be
        // protected before its traffic loops into the TUN
        if cur.iter().any(|c| c.network_length() <= 1)
            && let Some(ip) = bypass::conn_remote_ipv4(conn)
            && bypass::is_bypass_candidate_ipv4(
                ip,
                &bypass::physical_onlink_v4_subnets(),
                &self.global_ctx,
            )
            && let Some(route) = super::get_default_route_v4()
        {
            self.bypass.add_one(ip, route.gateway).await;
        }
    }

    async fn shutdown(&mut self) {
        self.bypass.clear().await;
    }
}
