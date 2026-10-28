#!/usr/bin/env python3
"""Production unencrypted V2 reader: real cache files and TCP responses.
Metadata and app/key access are fixture boundaries; no live Telegram, UI or
encrypted-read claims are made by this test.
"""
from pathlib import Path
import subprocess
import tempfile
root=Path(__file__).resolve().parent.parent
source=(root/"app/src-tauri/src/drive_mount.rs").read_text()
a=source.index("    fn reject_symlink_file(")
b=source.index("    fn create_private_file(",a)
helpers=source[a:b]
a=source.index("        fn fetch_v2_plain_crypto_chunk(")
b=source.index("        fn fetch_remote_block(",a)
method=source[a:b]
deps=root/"app/src-tauri/target/debug/deps"
externs=[]
for name in ["reqwest","urlencoding","sha2","libc"]:
    paths=sorted(deps.glob("lib"+name+"-*.rlib"),key=lambda p:p.stat().st_mtime,reverse=True)
    assert paths,"Compile development Rust tests first"
    externs+=["--extern",name+"="+str(paths[0])]
rust=r'''
use std::path::{Path,PathBuf};
use std::fs::{File,OpenOptions};
use std::os::unix::fs::{OpenOptionsExt,FileExt as UnixFileExt};
use std::io::{Read,Write};
use std::net::TcpListener;
use std::sync::{Arc,Mutex,atomic::{AtomicBool,AtomicUsize,Ordering}};
use sha2::{Digest,Sha256};
struct FixtureApp;
#[derive(Clone)] struct Block { ciphertext_size:u64,part_offset:u64,sha256:String }
#[derive(Clone)] struct Part { backing_channel_id:i64,message_id:i64,ciphertext_size:u64 }
struct Location { block:Block,part:Part }
fn drive_object_block_location(conn:&Location,_id:&str,index:u64)->Result<Option<(Block,Part)>,String>{
    Ok((index==0).then(|| (conn.block.clone(),conn.part.clone())))
}
// The production reader now uses a preinitialized database lookup for normal
// reads and optionally frozen locations for retained Linux file descriptors.
fn drive_object_block_location_preinitialized(conn:&Location,id:&str,index:u64)->Result<Option<(Block,Part)>,String>{
    drive_object_block_location(conn,id,index)
}
struct FrozenV2Locations { object_id:String,block:Block,part:Part }
impl FrozenV2Locations {
    fn location(&self,id:&str,index:u64)->Result<(Block,Part),String>{
        if id!=self.object_id || index!=0 {return Err("Frozen V2 block location is missing".into())}
        Ok((self.block.clone(),self.part.clone()))
    }
}
#[cfg(unix)]
fn positioned_read(file:&File,data:&mut [u8],offset:u64)->std::io::Result<usize>{
    UnixFileExt::read_at(file,data,offset)
}
mod drive_stream { pub const DRIVE_ENCRYPTED_CHUNK_OVERHEAD:u64=45; }
mod drive_crypto {
    pub const DRIVE_PLAINTEXT_CHUNK_SIZE:u64=16*1024*1024;
    pub const DRIVE_ENCRYPTION_VERSION:u32=1;
    pub fn load_verified_master_key_for_db(_app:&super::FixtureApp,_db:&super::Location)->Result<Option<Vec<u8>>,String>{
        panic!("This harness is explicitly unencrypted")
    }
    pub fn decrypt_chunk(_key:&[u8],_id:&str,_index:u64,_len:u64,_bytes:&[u8])->Result<Vec<u8>,String>{
        panic!("Encrypted readers require their separate crypto tests")
    }
}
struct DriveFileRecord { storage_version:u32,plaintext_size:Option<u64>,total_size:u64,file_id:String,encryption_version:u32,crypto_id:Option<String> }
struct TeraRelayFs { db:Arc<Mutex<Location>>,app:FixtureApp,cache_root:PathBuf,http:reqwest::blocking::Client,stream_port:u16,stream_token:String }
''' + helpers + "\nimpl TeraRelayFs {\nfn evict_cache_best_effort(&self) {}\n" + method + "\n}\n" + r'''
struct Fixture { fs:TeraRelayFs,file:DriveFileRecord,requests:Arc<AtomicUsize>,stop:Arc<AtomicBool>,worker:Option<std::thread::JoinHandle<()>>,root:PathBuf }
impl Fixture {
 fn new(responses:Vec<Vec<u8>>)->Self {
  let root=std::env::temp_dir().join(format!("tr-v2-read-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
  std::fs::create_dir(&root).unwrap();
  let listener=TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
  let port=listener.local_addr().unwrap().port();
  let requests=Arc::new(AtomicUsize::new(0));let count=requests.clone();
  let stop=Arc::new(AtomicBool::new(false));let stopped=stop.clone();
  let worker=std::thread::spawn(move || {
    let mut replies=responses.into_iter();
    while !stopped.load(Ordering::SeqCst) {
      match listener.accept() {
       Ok((mut socket,_))=>{
        socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut request_bytes=Vec::new();
        while !request_bytes.ends_with(b"\r\n\r\n") {
            let mut buf=[0;1024];let n=socket.read(&mut buf).unwrap();
            assert!(n>0 && request_bytes.len()+n<=16384,"Incomplete or oversized fixture request");
            request_bytes.extend_from_slice(&buf[..n]);
        }
        let request=String::from_utf8_lossy(&request_bytes).to_ascii_lowercase();
        assert!(request.contains("range: bytes=2-9"));
        assert!(request.contains("token=fixture-token"));
        count.fetch_add(1,Ordering::SeqCst);
        let body=replies.next().expect("Unexpected extra remote request");
        write!(socket,"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-9/10\r\nContent-Length: 8\r\nConnection: close\r\n\r\n").unwrap();
        socket.write_all(&body).unwrap();
       },
       Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>std::thread::sleep(std::time::Duration::from_millis(2)),
       Err(e)=>panic!("{e}")
      }
    }
  });
  let file=DriveFileRecord{storage_version:2,plaintext_size:Some(8),total_size:8,file_id:"fixture-object".into(),encryption_version:0,crypto_id:None};
  let location=Location{block:Block{ciphertext_size:8,part_offset:2,sha256:format!("{:x}",Sha256::digest(b"verified"))},part:Part{backing_channel_id:42,message_id:7,ciphertext_size:10}};
  let fs=TeraRelayFs{db:Arc::new(Mutex::new(location)),app:FixtureApp,cache_root:root.clone(),http:reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap(),stream_port:port,stream_token:"fixture-token".into()};
  Self{fs,file,requests,stop,worker:Some(worker),root}
 }
 fn cache(&self)->PathBuf {self.root.join("fixture-object/v2-cipher-0000000000000000.bin")}
 fn seed(&self,bytes:&[u8]) {let p=self.cache();std::fs::create_dir_all(p.parent().unwrap()).unwrap();std::fs::write(p,bytes).unwrap();}
 fn read(&self)->Result<Vec<u8>,String>{self.fs.fetch_v2_plain_crypto_chunk(&self.file,0,None)}
}
impl Drop for Fixture { fn drop(&mut self) {self.stop.store(true,Ordering::SeqCst);self.worker.take().unwrap().join().unwrap();std::fs::remove_dir_all(&self.root).unwrap();} }
#[test] fn verified_cached_bytes_do_not_acquire_remote_data() {
 let f=Fixture::new(vec![]);f.seed(b"verified");
 assert_eq!(f.read().unwrap(),b"verified");assert_eq!(f.requests.load(Ordering::SeqCst),0);
}
#[test] fn retained_descriptor_uses_frozen_v2_locations_not_mutated_live_metadata() {
 let f=Fixture::new(vec![]);f.seed(b"verified");
 let frozen={
  let db=f.fs.db.lock().unwrap();
  FrozenV2Locations {object_id:f.file.file_id.clone(),block:db.block.clone(),part:db.part.clone()}
 };
 {
  let mut db=f.fs.db.lock().unwrap();
  db.block.sha256="live metadata changed after rename".into();
  db.part.message_id=999;
 }
 assert_ne!(f.fs.db.lock().unwrap().block.sha256,frozen.block.sha256);
 assert_eq!(f.fs.fetch_v2_plain_crypto_chunk(&f.file,0,Some(&frozen)).unwrap(),b"verified");
 assert_eq!(f.requests.load(Ordering::SeqCst),0);
}
#[test] fn a_corrupt_cache_is_recovered_and_verified_in_the_same_read() {
 let f=Fixture::new(vec![b"verified".to_vec()]);f.seed(b"corrupt!");
 assert_eq!(f.read().expect("A disposable corrupt cache should fetch its verified replacement"),b"verified");
 assert_eq!(f.requests.load(Ordering::SeqCst),1);assert_eq!(std::fs::read(f.cache()).unwrap(),b"verified");
}
#[test] fn a_truncated_cache_is_replaced_by_complete_verified_remote_bytes() {
 let f=Fixture::new(vec![b"verified".to_vec()]);f.seed(b"bad");
 assert_eq!(f.read().unwrap(),b"verified");assert_eq!(std::fs::read(f.cache()).unwrap(),b"verified");
}
#[test] fn a_dropped_connection_never_publishes_partial_cache_and_next_read_recovers() {
 let f=Fixture::new(vec![b"ver".to_vec(),b"verified".to_vec()]);
 assert!(f.read().is_err());assert!(!f.cache().exists());
 assert_eq!(f.read().unwrap(),b"verified");assert_eq!(f.requests.load(Ordering::SeqCst),2);
 assert_eq!(std::fs::read(f.cache()).unwrap(),b"verified");
}
#[test] fn wrong_remote_content_is_rejected_without_a_successful_cache_entry() {
 let f=Fixture::new(vec![b"corrupt!".to_vec()]);
 assert!(f.read().unwrap_err().contains("checksum mismatch"));assert!(!f.cache().exists());
}
'''
if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="tr-v2-reader-test-") as tmp:
        p=Path(tmp);(p/"test.rs").write_text(rust)
        subprocess.run(["rustc","--edition=2021","--test",str(p/"test.rs"),*externs,"-L","dependency="+str(deps),"-o",str(p/"test")],check=True)
        subprocess.run([str(p/"test"),"--test-threads=1"],check=True)
