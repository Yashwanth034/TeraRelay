#!/usr/bin/env python3
"""Run the production Drive HTTP range reader against a real loopback HTTP fixture."""
from pathlib import Path
import subprocess
import tempfile
root = Path(__file__).resolve().parent.parent
source = (root / "app/src-tauri/src/drive_mount.rs").read_text()
start = source.index("        fn fetch_stream_range(")
end = source.index("        fn fetch_encrypted_plain_chunk(", start)
method = source[start:end]
helper = ""
if "    fn read_range_response(" in source:
    a = source.index("    fn read_range_response(")
    b = source.index("    fn create_private_file(", a)
    helper = source[a:b]
deps = root / "app/src-tauri/target/debug/deps"
externs = []
for name in ["reqwest", "urlencoding"]:
    found = sorted(deps.glob("lib" + name + "-*.rlib"), key=lambda p: p.stat().st_mtime, reverse=True)
    assert found, "Compile development Rust tests before this check"
    externs += ["--extern", name + "=" + str(found[0])]
rust = r"""
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
struct DriveFileRecord { backing_channel_id: i64, first_message_id: i64 }
struct TeraRelayFs { http: reqwest::blocking::Client, stream_port: u16, stream_token: String }
""" + helper + "\nimpl TeraRelayFs {\n" + method + "\n}\n" + r"""
fn fixture(status: &str, headers: &str, total: usize, chunked: bool) -> (TeraRelayFs, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let sent = Arc::new(AtomicUsize::new(0));
    let counter = sent.clone();
    let prefix = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n");
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = server.accept().unwrap();
        socket.set_write_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut request = [0; 2048];
        let n = socket.read(&mut request).unwrap();
        assert!(String::from_utf8_lossy(&request[..n]).to_ascii_lowercase().contains("range: bytes=2-9"));
        if socket.write_all(prefix.as_bytes()).is_err() { return; }
        let payload = vec![7; 65536];
        let mut remaining = total;
        while remaining > 0 {
            let take = remaining.min(payload.len());
            if chunked && write!(socket, "{take:x}\r\n").is_err() { return; }
            if socket.write_all(&payload[..take]).is_err() { return; }
            counter.fetch_add(take, Ordering::SeqCst);
            if chunked && socket.write_all(b"\r\n").is_err() { return; }
            remaining -= take;
        }
        if chunked { let _ = socket.write_all(b"0\r\n\r\n"); }
    });
    let fs = TeraRelayFs { http: reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap(), stream_port: port, stream_token: "fixture-only".to_string() };
    (fs, sent, worker)
}
fn read(fs: &TeraRelayFs) -> Result<Vec<u8>, String> {
    fs.fetch_stream_range(&DriveFileRecord { backing_channel_id: 1, first_message_id: 1 }, 2, 9)
}
#[test] fn exact_partial_range_reads_the_requested_bytes() {
    let (fs, _, worker) = fixture("206 Partial Content", "Content-Range: bytes 2-9/20\r\nContent-Length: 8\r\n", 8, false);
    assert_eq!(read(&fs).unwrap(), vec![7; 8]); worker.join().unwrap();
}
#[test] fn a_server_ignoring_range_is_not_accepted() {
    let (fs, _, worker) = fixture("200 OK", "Content-Length: 8\r\n", 8, false);
    assert!(read(&fs).is_err(), "A full HTTP response was accepted for a partial read"); worker.join().unwrap();
}
#[test] fn a_wrong_content_range_is_not_accepted() {
    let (fs, _, worker) = fixture("206 Partial Content", "Content-Range: bytes 12-19/20\r\nContent-Length: 8\r\n", 8, false);
    assert!(read(&fs).is_err(), "Bytes from the wrong offset were accepted"); worker.join().unwrap();
}
#[test] fn a_short_response_cannot_be_returned_as_success() {
    let (fs, _, worker) = fixture("206 Partial Content", "Content-Range: bytes 2-9/20\r\nContent-Length: 8\r\n", 3, false);
    assert!(read(&fs).is_err()); worker.join().unwrap();
}
#[test] fn oversized_body_is_rejected_before_acquiring_the_entire_body() {
    let total = 64 * 1024 * 1024;
    let (fs, sent, worker) = fixture("206 Partial Content", &format!("Content-Range: bytes 2-9/10000000000000\r\nContent-Length: {total}\r\n"), total, false);
    assert!(read(&fs).is_err()); worker.join().unwrap();
    assert!(sent.load(Ordering::SeqCst) < total, "Reader acquired all 64 MiB for an 8-byte request");
}
#[test] fn unknown_length_body_is_still_bounded() {
    let total = 64 * 1024 * 1024;
    let (fs, sent, worker) = fixture("206 Partial Content", "Content-Range: bytes 2-9/10000000000000\r\nTransfer-Encoding: chunked\r\n", total, true);
    assert!(read(&fs).is_err()); worker.join().unwrap();
    assert!(sent.load(Ordering::SeqCst) < total, "Chunked response bypassed the read budget");
}
"""
rust += r"""
#[test]
fn connection_failure_does_not_expose_the_private_stream_token() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let fs = TeraRelayFs {
        http: reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(2)).build().unwrap(),
        stream_port: port,
        stream_token: "fixture-private-stream-token".into(),
    };
    let file = DriveFileRecord { backing_channel_id: 42, first_message_id: 7 };
    let error = fs.fetch_stream_range(&file, 2, 9).unwrap_err();
    assert!(!error.contains("fixture-private-stream-token"), "A private token escaped into a user-visible error");
    assert!(!error.contains("token="), "Authenticated URL escaped into a user-visible error");
}
"""
with tempfile.TemporaryDirectory(prefix="tr-drive-http-") as directory:
    test = Path(directory) / "range.rs"
    binary = Path(directory) / "range-test"
    test.write_text(rust)
    subprocess.run(["rustc", "--edition=2021", "--test", str(test), "-L", "dependency=" + str(deps), *externs, "-o", str(binary)], check=True)
    subprocess.run([str(binary), "--test-threads=1"], check=True)
