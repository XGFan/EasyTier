mod default_route;
pub(crate) mod underlay;

pub use default_route::*;

// Full-tunnel route management.
pub(crate) mod bypass;
pub(crate) mod route_hook;
pub(crate) mod split_route;
