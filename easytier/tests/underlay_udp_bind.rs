//! Guard for the macOS full-tunnel fix.
//!
//! Every UDP socket that carries underlay traffic (STUN probes, hole punch
//! sockets, direct-connect probes) must be created through
//! `tunnel::common::bind_underlay_udp_socket`. A plainly bound socket follows
//! the routing table, so under full tunnel it gets captured by the TUN and
//! loops back into easytier — the connection still "works" until it silently
//! doesn't, which is exactly the kind of regression a rebase onto a new
//! upstream release can reintroduce without any merge conflict.
//!
//! This test fails when a new raw `UdpSocket::bind` appears in those modules.
//! If the new call site really does not carry underlay traffic, add it to
//! `KNOWN_EXEMPT` with the reason.

use std::path::{Path, PathBuf};

/// Modules whose UDP sockets talk to remote peers over the underlay.
const SCANNED: &[&str] = &["src/common/stun.rs", "src/connector"];

/// Production call sites knowingly left on a plain bind, with the reason.
const KNOWN_EXEMPT: &[(&str, &str)] = &[
    // The full-tunnel fix is IPv4-only: it replaces 0.0.0.0/0 with the split
    // route set and scopes sockets via IP_BOUND_IF using the IPv4 default
    // route. IPv6 underlay sockets are out of scope; revisit these if IPv6
    // full tunnel is ever supported.
    (
        "src/common/stun.rs",
        r#"UdpSocket::bind("[::]:0".to_string())"#,
    ),
    (
        "src/connector/mod.rs",
        r#"tokio::net::UdpSocket::bind("[::]:0")"#,
    ),
    ("src/connector/direct.rs", r#"UdpSocket::bind("[::]:0")"#),
];

/// `mod x`, `pub mod x`, `pub(crate) mod x` — the line after a `#[cfg(test)]`
/// that opens a test module.
fn opens_a_module(line: &str) -> bool {
    let mut rest = line.trim_start();
    if let Some(after_pub) = rest.strip_prefix("pub") {
        rest = after_pub.trim_start();
        if let Some(after_paren) = rest.strip_prefix('(') {
            rest = after_paren
                .split_once(')')
                .map_or(rest, |(_, tail)| tail)
                .trim_start();
        }
    }
    rest.starts_with("mod ")
}

fn rust_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        if path.extension().is_some_and(|e| e == "rs") {
            out.push(path.to_path_buf());
        }
        return;
    }
    let mut entries: Vec<_> = std::fs::read_dir(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for entry in entries {
        rust_files(&entry, out);
    }
}

#[test]
fn underlay_udp_sockets_are_bound_through_the_helper() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![];
    for scanned in SCANNED {
        rust_files(&root.join(scanned), &mut files);
    }
    assert!(!files.is_empty(), "nothing scanned; did the layout change?");

    let mut violations = vec![];
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        for (no, line) in lines.iter().enumerate() {
            // unit tests may bind however they like; they run on loopback.
            // Only a `#[cfg(test)] mod` ends the scan, so an attribute on a
            // single item cannot silently hide the rest of the file.
            if line.trim_start().starts_with("#[cfg(test)]")
                && lines.get(no + 1).is_some_and(|next| opens_a_module(next))
            {
                break;
            }
            let code = line.split("//").next().unwrap_or(line).trim();
            if !code.contains("UdpSocket::bind(") {
                continue;
            }
            if KNOWN_EXEMPT
                .iter()
                .any(|(f, snippet)| *f == rel && code.contains(snippet))
            {
                continue;
            }
            violations.push(format!("  {}:{}  {}", rel, no + 1, code));
        }
    }

    assert!(
        violations.is_empty(),
        "raw UdpSocket::bind on an underlay path:\n{}\n\n\
         Use tunnel::common::bind_underlay_udp_socket so the socket escapes \
         the full-tunnel TUN routes, or add the call site to KNOWN_EXEMPT in \
         {} with the reason it carries no underlay traffic.",
        violations.join("\n"),
        file!(),
    );
}
