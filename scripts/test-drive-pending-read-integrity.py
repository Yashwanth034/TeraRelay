#!/usr/bin/env python3
"""Production pending-part reader: real HTTP, fixture journal metadata.
Tests integrity used for resumed writes; no live Telegram or encrypted-read claims.
"""
from pathlib import Path
import runpy
import subprocess
import tempfile
base=runpy.run_path(str(Path(__file__).with_name("test-drive-v2-read-recovery.py")))
root,deps,externs=base["root"],base["deps"],base["externs"]
source=(root/"app/src-tauri/src/drive_mount.rs").read_text()
a=source.index("        fn fetch_pending_uploaded_chunk(")
b=source.index("        fn materialize_pending_chunk(",a)
method=source[a:b]
a=source.index("        fn pending_chunk_len(")
b=source.index("        fn range_is_covered(",a)
length_method=source[a:b]
rust=base["rust"].split("#[test] fn verified_cached_bytes_do_not_acquire_remote_data")[0]+r'''
#[derive(Clone)]
struct PendingChunk { chunk_index:u64,message_id:i64,chunk_size:u64,sha256:String }
struct PendingPart { first_chunk_index:u64,chunk_count:u64,message_id:i64,backing_channel_id:i64 }
struct DrivePendingRecord { id:String,size:u64,backing_channel_id:i64,encryption_version:u32,crypto_id:Option<String> }
fn drive_pending_chunks(conn:&Location,id:&str)->Result<Vec<PendingChunk>,String> {
 if id != "pending-fixture" {
  return Ok(vec![PendingChunk{chunk_index:0,message_id:conn.part.message_id,chunk_size:conn.block.ciphertext_size,sha256:conn.block.sha256.clone()}]);
 }
 Ok(vec![
 PendingChunk{chunk_index:0,message_id:conn.part.message_id,chunk_size:2,sha256:"unused preceding block".into()},
 PendingChunk{chunk_index:1,message_id:conn.part.message_id,chunk_size:8,sha256:conn.block.sha256.clone()}
 ])
}
fn drive_pending_parts(conn:&Location,id:&str)->Result<Vec<PendingPart>,String> {
 if id != "pending-fixture" {return Ok(vec![]);}
 Ok(vec![PendingPart{first_chunk_index:0,chunk_count:2,message_id:conn.part.message_id,backing_channel_id:conn.part.backing_channel_id}])
}
'''+"\nimpl TeraRelayFs {\n"+length_method+method+"\n}\n"+r'''
fn pending()->DrivePendingRecord { DrivePendingRecord{id:"pending-fixture".into(),size:drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE+8,backing_channel_id:42,encryption_version:0,crypto_id:None} }
#[test]
fn verified_pending_part_bytes_can_be_used_for_safe_continuation() {
 let f=Fixture::new(vec![b"verified".to_vec()]);
 assert_eq!(f.fs.fetch_pending_uploaded_chunk(&pending(),1).unwrap(),Some(b"verified".to_vec()));
 assert_eq!(f.requests.load(Ordering::SeqCst),1);
}
#[test]
fn a_wrong_pending_part_checksum_cannot_silently_change_resumed_content() {
 let f=Fixture::new(vec![b"corrupt!".to_vec()]);
 let result=f.fs.fetch_pending_uploaded_chunk(&pending(),1);
 assert!(result.is_err(),"Corrupt uploaded bytes were accepted for a resumed write");
 assert!(result.unwrap_err().contains("checksum"),"Integrity failure must be clear");
}
#[test]
fn an_interrupted_pending_read_can_retry_without_changing_its_journal() {
 let f=Fixture::new(vec![b"ver".to_vec(),b"verified".to_vec()]);
 assert!(f.fs.fetch_pending_uploaded_chunk(&pending(),1).is_err());
 assert_eq!(f.fs.fetch_pending_uploaded_chunk(&pending(),1).unwrap(),Some(b"verified".to_vec()));
 assert_eq!(f.requests.load(Ordering::SeqCst),2);
 assert_eq!(f.fs.db.lock().unwrap().block.sha256,format!("{:x}",Sha256::digest(b"verified")));
}
'''
# Reuse the TCP fixture with ranges derived from the request for legacy offsets.
rust = "#![allow(dead_code)]\n" + rust
rust = rust.replace('assert!(request.contains("range: bytes=2-9"));', '''
let range=request.lines().find_map(|line|line.strip_prefix("range: bytes=")).unwrap();
let (start,end)=range.split_once('-').unwrap();
let start=start.parse::<u64>().unwrap();let end=end.parse::<u64>().unwrap();
assert!(start==0 || start==2);assert!(end>=start);
''')
rust = rust.replace('Content-Range: bytes 2-9/10\\r\\nContent-Length: 8', 'Content-Range: bytes {start}-{end}/{}\\r\\nContent-Length: {}')
rust = rust.replace('Connection: close\\r\\n\\r\\n").unwrap();', 'Connection: close\\r\\n\\r\\n",end+1,end-start+1).unwrap();')
rust += r'''
#[test] fn a_legacy_standalone_chunk_is_verified_with_an_exact_range() {
 let f=Fixture::new(vec![b"verified".to_vec()]);let mut p=pending();p.id="legacy".into();p.size=8;
 assert_eq!(f.fs.fetch_pending_uploaded_chunk(&p,0).unwrap(),Some(b"verified".to_vec()));
}
#[test] fn a_corrupt_legacy_standalone_chunk_is_rejected() {
 let f=Fixture::new(vec![b"corrupt!".to_vec()]);let mut p=pending();p.id="legacy".into();p.size=8;
 assert!(f.fs.fetch_pending_uploaded_chunk(&p,0).unwrap_err().contains("checksum"));
}
#[test] fn an_empty_file_sentinel_is_verified_and_returns_no_content() {
 let f=Fixture::new(vec![vec![0]]);let mut p=pending();p.id="empty".into();p.size=0;
 {let mut db=f.fs.db.lock().unwrap();db.block.ciphertext_size=1;db.block.sha256=format!("{:x}",Sha256::digest([0]));}
 assert_eq!(f.fs.fetch_pending_uploaded_chunk(&p,0).unwrap(),Some(vec![]));
}
#[test] fn an_invalid_journal_block_size_is_rejected_without_a_request() {
 let f=Fixture::new(vec![]);let mut p=pending();p.id="oversized".into();
 f.fs.db.lock().unwrap().block.ciphertext_size=drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE+46;
 assert!(f.fs.fetch_pending_uploaded_chunk(&p,0).is_err());
 assert_eq!(f.requests.load(Ordering::SeqCst),0);
}
'''
with tempfile.TemporaryDirectory(prefix="tr-pending-read-test-") as tmp:
    p=Path(tmp);(p/"test.rs").write_text(rust)
    subprocess.run(["rustc","--edition=2021","--test",str(p/"test.rs"),*externs,"-L","dependency="+str(deps),"-o",str(p/"test")],check=True)
    subprocess.run([str(p/"test"),"--test-threads=1"],check=True)
