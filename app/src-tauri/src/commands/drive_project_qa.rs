//! Development-only isolated manifest round-trip utility. Never performs network sync.
use super::DriveManifestV1;

pub fn qa_export_manifest(conn: &sqlite::Connection) -> Result<DriveManifestV1, String> {
    super::build_manifest(conn)
}

pub fn qa_restore_manifest(
    conn: &sqlite::Connection,
    manifest: &DriveManifestV1,
) -> Result<(), String> {
    super::apply_manifest(conn, manifest)
}
