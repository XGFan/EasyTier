//! Guard for the macOS full-tunnel fix.
//!
//! Every OS socket that carries underlay traffic must be created by the host
//! socket factories (`src/socket/{udp,tcp}.rs` → `tunnel::common::bind`),
//! because that is where `arch::macos::underlay` scopes peer-facing sockets
//! to the physical interface. A socket created anywhere else follows the
//! routing table, so under full tunnel it gets captured by the TUN and loops
//! back into easytier — the connection still "works" until it silently
//! doesn't, which is exactly the kind of regression a rebase onto a new
//! upstream release can reintroduce without any merge conflict.
//!
//! This test fails when a new raw socket creation appears in the scanned
//! trees. If the new call site really carries no underlay traffic, add it to
//! `KNOWN_EXEMPT` with the reason.

use std::path::{Path, PathBuf};

/// Trees whose sockets may talk to remote peers, relative to the workspace root.
const SCANNED: &[&str] = &[
    "easytier/src",
    "easytier-core/src/connectivity",
    "easytier-core/src/listener",
    "easytier-core/src/socket",
];

/// Raw OS socket constructors. `Socket::new(` only counts as a whole word, so
/// wrappers like `RuntimeTcpSocket::new(` do not match.
const CONSTRUCTORS: &[&str] = &[
    "UdpSocket::bind(",
    "TcpSocket::new_v4(",
    "TcpSocket::new_v6(",
    "TcpStream::connect(",
    "Socket::new(",
];

/// Production call sites knowingly outside the factories, with the reason.
const KNOWN_EXEMPT: &[(&str, &str)] = &[
    // the factories themselves
    (
        "easytier/src/tunnel/common.rs",
        "socket2::Socket::new(socket2::Domain::for_address(addr), B::TYPE, B::PROTOCOL)",
    ),
    // create_tcp_socket's connect-without-bind path; underlay purposes get a
    // bind_device first, which routes them through `bind` instead
    ("easytier/src/socket/tcp.rs", "TcpSocket::new_v4()"),
    ("easytier/src/socket/tcp.rs", "TcpSocket::new_v6()"),
    // route lookups: connect() on UDP sends nothing
    (
        "easytier/src/common/network.rs",
        r#"tokio::net::UdpSocket::bind("0.0.0.0:0")"#,
    ),
    (
        "easytier/src/common/network.rs",
        r#"tokio::net::UdpSocket::bind("[::]:0")"#,
    ),
    (
        "easytier/src/socket/fake_tcp/mod.rs",
        "UdpSocket::bind((bind_addr, 0))",
    ),
    // local address probe, never sends
    (
        "easytier/src/host_runtime.rs",
        r#"std::net::UdpSocket::bind(format!("{ip}:0"))"#,
    ),
    // FakeTCP's decoy socket only reserves a port; its payload is crafted
    // and injected below the socket layer. FakeTCP under full tunnel was
    // never supported by this fork.
    (
        "easytier/src/socket/fake_tcp/mod.rs",
        "tokio::net::TcpSocket::new_v4()",
    ),
    ("easytier/src/socket/fake_tcp/stack.rs", "Socket::new("),
    // linux netlink, not IP
    (
        "easytier/src/common/ifcfg/netlink.rs",
        r#"std::net::UdpSocket::bind("0:0")"#,
    ),
    (
        "easytier/src/common/ifcfg/netlink.rs",
        "Socket::new(NETLINK_ROUTE)",
    ),
    // TUN-facing services: magic DNS, ICMP proxy, Windows broadcast relay
    (
        "easytier/src/instance/dns_server/server.rs",
        "socket2::Socket::new(",
    ),
    ("easytier/src/gateway/icmp_proxy.rs", "Socket::new("),
    (
        "easytier/src/instance/windows_udp_broadcast/runtime.rs",
        "Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::UDP))",
    ),
];

fn is_test_file(rel: &str) -> bool {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    parts.contains(&"tests")
        || matches!(name, "tests.rs" | "testkit.rs" | "test_utils.rs")
        || name.starts_with("test_")
        || name.ends_with("_tests.rs")
}

/// `#[cfg(test)]`, `#[cfg(all(test, ...))]` — the attribute on a test module.
fn is_test_cfg(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test")
}

/// `mod x {`, `pub mod x {`, `pub(crate) mod x {` — the line after a test
/// cfg that opens an inline test module. An out-of-line `mod x;` does not
/// count: the rest of its file is still production code.
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
    rest.starts_with("mod ") && rest.trim_end().ends_with('{')
}

/// Whether `code` calls one of [`CONSTRUCTORS`] as a whole word.
fn creates_raw_socket(code: &str) -> bool {
    CONSTRUCTORS.iter().any(|ctor| {
        code.match_indices(ctor).any(|(at, _)| {
            !code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    })
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
fn underlay_sockets_are_created_by_the_host_factories() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut files = vec![];
    for scanned in SCANNED {
        let dir = root.join(scanned);
        assert!(dir.is_dir(), "{scanned} is gone; did the layout change?");
        rust_files(&dir, &mut files);
    }

    let mut violations = vec![];
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_file(&rel) {
            continue;
        }
        let source = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        for (no, line) in lines.iter().enumerate() {
            // unit tests may bind however they like; they run on loopback.
            // Only a test-cfg'd `mod` ends the scan, so an attribute on a
            // single item cannot silently hide the rest of the file.
            if is_test_cfg(line) && lines.get(no + 1).is_some_and(|next| opens_a_module(next)) {
                break;
            }
            let code = line.split("//").next().unwrap_or(line).trim();
            if !creates_raw_socket(code) {
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
        "raw socket creation outside the host socket factories:\n{}\n\n\
         Create the socket through easytier/src/socket/{{udp,tcp}}.rs with the \
         right purpose so arch::macos::underlay can keep it off the full-tunnel \
         TUN routes, or add the call site to KNOWN_EXEMPT in {} with the reason \
         it carries no underlay traffic.",
        violations.join("\n"),
        file!(),
    );
}

#[test]
fn constructor_match_is_whole_word() {
    assert!(creates_raw_socket(
        "let s = socket2::Socket::new(a, b, c)?;"
    ));
    assert!(creates_raw_socket("Socket::new(NETLINK_ROUTE)"));
    assert!(!creates_raw_socket("Ok(RuntimeTcpSocket::new(stream))"));
    assert!(!creates_raw_socket(
        "UdpPunchSocket::new(session, remote, layer)"
    ));
}

#[test]
fn only_an_inline_test_module_ends_the_scan() {
    assert!(opens_a_module("mod tests {"));
    assert!(opens_a_module("pub(crate) mod tests {"));
    assert!(!opens_a_module("mod tests;"));
    assert!(!opens_a_module("pub mod test_utils;"));
}
