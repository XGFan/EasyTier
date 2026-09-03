//! Platform seam for proxy-cidr route installation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crate::common::global_ctx::ArcGlobalCtx;
use crate::peers::peer_manager::PeerManager;
use crate::proto::api::instance::PeerConnInfo;

/// What a [`ProxyRouteHook`] did with one proxy cidr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteHookResult {
    /// The hook does not handle this cidr; the generic `ifcfg` path applies.
    NotHandled,
    /// The hook applied it; record the new state.
    Done,
    /// The hook owns this cidr but could not apply it, and left the routing
    /// table as it found it. Do not record it, so a later reconcile retries.
    Deferred,
}

/// Platform-specific handling of proxy-cidr routes.
///
/// Most platforms install every proxy cidr through the generic `ifcfg` path
/// and use [`NoopProxyRouteHook`]. macOS cannot: a 0.0.0.0/0 route on the TUN
/// collides with the system default route, and the broad routes that replace
/// it would also swallow easytier's own underlay traffic. See
/// [`crate::arch::macos::route_hook`].
pub(crate) trait ProxyRouteHook {
    fn new(global_ctx: ArcGlobalCtx, peer_mgr: Arc<PeerManager>, ifname: String) -> Self;

    /// How often [`Self::reconcile`] should run on its own, if at all.
    fn reconcile_interval(&self) -> Option<Duration>;

    async fn try_add_route(&mut self, cidr: &cidr::Ipv4Cidr) -> RouteHookResult;

    async fn try_remove_route(&mut self, cidr: &cidr::Ipv4Cidr) -> RouteHookResult;

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
pub(crate) struct NoopProxyRouteHook;

impl ProxyRouteHook for NoopProxyRouteHook {
    fn new(_global_ctx: ArcGlobalCtx, _peer_mgr: Arc<PeerManager>, _ifname: String) -> Self {
        Self
    }

    fn reconcile_interval(&self) -> Option<Duration> {
        None
    }

    async fn try_add_route(&mut self, _cidr: &cidr::Ipv4Cidr) -> RouteHookResult {
        RouteHookResult::NotHandled
    }

    async fn try_remove_route(&mut self, _cidr: &cidr::Ipv4Cidr) -> RouteHookResult {
        RouteHookResult::NotHandled
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
