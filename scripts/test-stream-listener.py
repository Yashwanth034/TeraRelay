#!/usr/bin/env python3
"""Exercise the production stream listener with an occupied real IPv4 socket."""
from pathlib import Path
import subprocess
import tempfile
root = Path(__file__).resolve().parent.parent
source = (root / "app/src-tauri/src/server.rs").read_text()
if "pub fn bind_stream_listener(" in source:
    a = source.index("pub fn bind_stream_listener(")
    b = source.index("pub async fn start_server(", a)
    helper = source[a:b]
else:
    a = source.index('    let ipv4_addr = format!("127.0.0.1:')
    b = source.index("    let server = HttpServer::new", a)
    helper = "fn bind_stream_listener(port: u16) -> std::io::Result<TcpListener> {\n" + source[a:b] + "\nOk(listener)\n}\n"
deps = root / "app/src-tauri/target/debug/deps"
logs = sorted(deps.glob("liblog-*.rlib"), key=lambda p: p.stat().st_mtime, reverse=True)
assert logs, "Compile development Rust tests first"
tests = r'''
use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
''' + helper + r'''
#[test]
fn an_occupied_ipv4_port_still_routes_to_the_owned_ipv4_listener() {
    let foreign = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied = foreign.local_addr().unwrap().port();
    let owned = bind_stream_listener(occupied).unwrap();
    let address = owned.local_addr().unwrap();
    assert!(address.is_ipv4(), "Drive IPv4 URLs cannot reach an IPv6-only listener");
    assert!(address.ip().is_loopback(), "Streaming must remain loopback-only");
    assert_ne!(address.port(), occupied, "Do not advertise a foreign listener");
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = owned.accept().unwrap();
        socket.write_all(b"owned-session").unwrap();
    });
    let mut client = TcpStream::connect(("127.0.0.1", address.port())).unwrap();
    client.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
    let mut result = Vec::new();
    client.read_to_end(&mut result).unwrap();
    assert_eq!(result, b"owned-session");
    worker.join().unwrap();
    drop(foreign);
}
#[test]
fn a_free_preferred_port_is_preserved_and_held() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let owned = bind_stream_listener(port).unwrap();
    assert_eq!(owned.local_addr().unwrap(), ("127.0.0.1".parse::<std::net::IpAddr>().unwrap(), port).into());
    assert_eq!(TcpListener::bind(("127.0.0.1", port)).unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
}
'''
with tempfile.TemporaryDirectory(prefix="terarelay-stream-listener-") as tmp:
    path = Path(tmp)
    (path / "test.rs").write_text(tests)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "test.rs"),
                    "--extern", "log=" + str(logs[0]), "-L", "dependency=" + str(deps),
                    "-o", str(path / "test")], check=True)
    subprocess.run([str(path / "test"), "--test-threads=1"], check=True)
