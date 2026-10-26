//! Platform seam for proxy-cidr route installation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use easytier_core::instance::CorePacketPlane;

use crate::common::global_ctx::ArcGlobalCtx;
use crate::proto::api::instance::PeerConnInfo;

/// Platform-specific state that has to follow the proxy-cidr routes.
///
/// The routes themselves always go through the NIC backend. Most platforms
/// need nothing beyond that and use [`NoopProxyRouteHook`]. macOS does: the
/// broad routes that stand in for 0.0.0.0/0 there would also swallow
/// easytier's own underlay traffic, which needs routes of its own outside the
/// TUN. See [`crate::arch::macos::route_hook`].
pub(crate) trait ProxyRouteHook {
    fn new(global_ctx: ArcGlobalCtx, packet_plane: Arc<CorePacketPlane>) -> Self;

    /// How often [`Self::reconcile`] should run on its own, if at all.
    fn reconcile_interval(&self) -> Option<Duration>;

    /// Bring out-of-band state in line with the routes that are installed or
    /// about to be. Runs before every route change and on the interval above.
    async fn reconcile(&mut self, cur: &BTreeSet<cidr::Ipv4Cidr>, pending_added: &[cidr::Ipv4Cidr]);

    /// A new underlay connection appeared and may need protecting before its
    /// traffic meets the routes above.
    async fn on_peer_conn_added(&mut self, cur: &BTreeSet<cidr::Ipv4Cidr>, conn: &PeerConnInfo);

    /// Undo everything the hook installed.
    async fn shutdown(&mut self);
}

/// Hook for platforms where the generic route path is sufficient.
#[cfg_attr(all(target_os = "macos", not(feature = "macos-ne")), allow(dead_code))]
pub(crate) struct NoopProxyRouteHook;

impl ProxyRouteHook for NoopProxyRouteHook {
    fn new(_global_ctx: ArcGlobalCtx, _packet_plane: Arc<CorePacketPlane>) -> Self {
        Self
    }

    fn reconcile_interval(&self) -> Option<Duration> {
        None
    }

    async fn reconcile(
        &mut self,
        _cur: &BTreeSet<cidr::Ipv4Cidr>,
        _pending_added: &[cidr::Ipv4Cidr],
    ) {
    }

    async fn on_peer_conn_added(&mut self, _cur: &BTreeSet<cidr::Ipv4Cidr>, _conn: &PeerConnInfo) {}

    async fn shutdown(&mut self) {}
}

/// Fires on the hook's reconcile interval, or never when it has none, so the
/// route updater can `select!` on it without knowing the platform.
pub(crate) struct ReconcileTicker(Option<tokio::time::Interval>);

impl ReconcileTicker {
    pub(crate) fn new(interval: Option<Duration>) -> Self {
        Self(interval.map(|d| tokio::time::interval_at(tokio::time::Instant::now() + d, d)))
    }

    pub(crate) async fn tick(&mut self) {
        match self.0.as_mut() {
            Some(interval) => {
                interval.tick().await;
            }
            None => std::future::pending().await,
        }
    }
}

#[cfg(all(target_os = "macos", not(feature = "macos-ne")))]
pub(crate) type PlatformProxyRouteHook = super::macos::route_hook::MacProxyRouteHook;
#[cfg(not(all(target_os = "macos", not(feature = "macos-ne"))))]
pub(crate) type PlatformProxyRouteHook = NoopProxyRouteHook;
