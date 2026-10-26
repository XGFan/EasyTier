#[cfg(target_os = "windows")]
pub mod windows;

// full-tunnel support for the utun datapath; the NetworkExtension build
// (`macos-ne`) leaves routing and socket scoping to the system
#[cfg(all(target_os = "macos", not(feature = "macos-ne")))]
pub mod macos;

pub(crate) mod route_hook;
