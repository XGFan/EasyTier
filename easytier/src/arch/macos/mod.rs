mod default_route;
pub(crate) mod underlay;
#[cfg(test)]
mod underlay_bind_tests;

pub use default_route::*;

// Full-tunnel route management. Only meaningful for the utun-based datapath:
// the NetworkExtension build (`macos-ne`) has the system manage its routes.
#[cfg(not(feature = "macos-ne"))]
pub(crate) mod bypass;
#[cfg(not(feature = "macos-ne"))]
pub(crate) mod route_hook;
#[cfg(not(feature = "macos-ne"))]
pub(crate) mod split_route;
