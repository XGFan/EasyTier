use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use easytier_core::instance::CorePacketPlane;

use super::bypass;
use crate::arch::route_hook::ProxyRouteHook;
use crate::common::global_ctx::ArcGlobalCtx;
use crate::proto::api::instance::PeerConnInfo;

/// macOS full-tunnel routing: the split default route set that stands in for
/// 0.0.0.0/0 on the TUN (installed by `MacIfConfiger`, see
/// [`super::split_route`]) would capture easytier's own underlay traffic, so
/// its live underlay endpoints are protected from it by /32 bypass routes.
pub(crate) struct MacProxyRouteHook {
    global_ctx: ArcGlobalCtx,
    packet_plane: Arc<CorePacketPlane>,
    bypass: bypass::BypassRouteManager<bypass::SysRouteOps>,
}

impl ProxyRouteHook for MacProxyRouteHook {
    fn new(global_ctx: ArcGlobalCtx, packet_plane: Arc<CorePacketPlane>) -> Self {
        Self {
            global_ctx,
            packet_plane,
            bypass: bypass::BypassRouteManager::new(bypass::SysRouteOps),
        }
    }

    fn reconcile_interval(&self) -> Option<Duration> {
        // also retries route changes that previously failed (e.g. a
        // rolled-back split default route set)
        Some(bypass::RECONCILE_INTERVAL)
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
            &self.packet_plane,
        )
        .await;
    }

    async fn on_peer_conn_added(&mut self, cur: &BTreeSet<cidr::Ipv4Cidr>, conn: &PeerConnInfo) {
        // a fresh underlay conn (e.g. a just-punched p2p endpoint) must be
        // protected before its traffic loops into the TUN
        if cur.iter().any(|c| c.network_length() <= 1)
            && let Some(ip) = bypass::conn_remote_ipv4(conn.tunnel.as_ref())
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

impl Drop for MacProxyRouteHook {
    fn drop(&mut self) {
        // Dropped without `shutdown`, e.g. the NIC start was cancelled during
        // the initial route sync. The bypass routes would outlive the process
        // and later be taken for someone else's, so remove them anyway.
        if self.bypass.is_empty() {
            return;
        }
        let mut bypass = std::mem::replace(
            &mut self.bypass,
            bypass::BypassRouteManager::new(bypass::SysRouteOps),
        );
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move { bypass.clear().await });
            }
            Err(_) => tracing::warn!("bypass routes left behind: no runtime to remove them"),
        }
    }
}
