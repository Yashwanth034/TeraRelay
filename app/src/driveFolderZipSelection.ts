import {validateDriveSnapshot} from './deviceContinuity';
import {parseFolderInventory} from './driveFolderOperations';
import {validCloneName} from './driveFolderClone';
import {
  type BulkExportFile, MAX_BULK_EXPORT_BYTES, selectedTotalBytes,
} from './driveBulkExport';

/** Strict one-click recursive selection for the existing VERIFIED ZIP worker.
 * This covers all VISIBLE completed files within the folder. The native ZIP
 * worker separately revalidates content/mount state while streaming.
 * Recursive exports preserve empty directory entries even when the tree has
 * zero completed files. Symbolic links remain unsupported.
 */
export interface FolderZipSelection {
  folderId:string;
  folderPath:string;
  entryIds:string[];
  totalBytes:number;
  foldersCovered:number;
  preservedEmptyFolders:number;
  revision:number;
}
const asRow=(v:unknown):Record<string,unknown>|null=>
 v!==null&&typeof v==='object'&&!Array.isArray(v)?v as Record<string,unknown>:null;
const safeId=(v:unknown):v is string=>typeof v==='string'&&v.length>0&&v.length<=256
 &&v!=='.'&&v!=='..'&&!/[/\\\u0000-\u001f]/.test(v);

export function selectFolderForVerifiedZip(
 raw:unknown, visibleFiles:readonly BulkExportFile[], folderId:string,
):FolderZipSelection {
 const snapshot=validateDriveSnapshot(raw);
 const src=asRow(raw);
 if(!src||!Array.isArray(src.symlinks))
   throw Error('Drive link inventory is unavailable; cannot check folder ZIP safety.');
 const folderInventory=parseFolderInventory(raw);
 const byId=new Map(folderInventory.folders.map(d=>[d.id,d]));
 if(!safeId(folderId)||!byId.has(folderId))
   throw Error('Folder is missing from the current Drive inventory.');
 const folder=byId.get(folderId)!;
 const descendants=new Set<string>([folderId]);
 for(const f of folderInventory.folders){
   if(f.id===folderId||f.path.startsWith(folder.path+'/')){
     if(!validCloneName(f.name))
       throw Error('Folder contains a nonportable directory name; ZIP selection refused.');
     descendants.add(f.id);
   }
 }
 if(folderInventory.skipped)
   throw Error('Drive folders have unresolved metadata; refresh before folder ZIP selection.');
 if(descendants.size>512)
   throw Error('Folder ZIP supports at most 512 directories, including empty folders.');
 const seenRaw=new Set<string>(),selectedIds:string[]=[];
 const portableArchivePaths=new Set<string>();
 const visible=new Map<string,BulkExportFile>();
 for(const f of visibleFiles){
   if(visible.has(f.id))
     throw Error('Completed file inventory contains duplicate identities.');
   visible.set(f.id,f);
 }
 const duplicateScopeIds=new Set<string>();
 for(const item of snapshot.files){
   const f=asRow(item);
   if(!f)throw Error('Drive contains malformed file metadata.');
   if(!descendants.has(f.directory_id as string))continue;
   const id=f.entry_id;
   if(!safeId(id))
     throw Error('Selected folder has a malformed file identity.');
   if(seenRaw.has(id))duplicateScopeIds.add(id);
   else seenRaw.add(id);
   const file=visible.get(id as string);
   if(!file||typeof f.display_name!=='string'||!validCloneName(f.display_name))
     throw Error('Folder contains a completed file that cannot safely be exported.');
   const parent=byId.get(f.directory_id as string);
   if(!parent || file.path!==parent.path+'/'+f.display_name)
     throw Error('Drive folder and completed-file paths disagree.');
   if(new TextEncoder().encode('DriveFiles'+file.path).length>4096)
     throw Error('Drive folder includes a path too long for safe ZIP export.');
   const portableKey=file.path.normalize('NFC').toLocaleLowerCase();
   if(portableArchivePaths.has(portableKey))
     throw Error('Two files collide when extracted on case-insensitive filesystems.');
   portableArchivePaths.add(portableKey);
   selectedIds.push(id as string);
 }
 if(duplicateScopeIds.size)
   throw Error('Folder has ambiguous legacy/V2 file identities; ZIP selection refused.');
 for(const pending of snapshot.pending){
   const p=asRow(pending);
   if(!p)throw Error('Folder has malformed pending-upload metadata.');
   // Related COW aliases must block. An unrelated pending edit elsewhere
   // should not prevent backup selection; the native raw-entry completeness
   // preflight catches hidden bases whose parent is inside this subtree.
   if((typeof p.base_entry_id==='string'&&seenRaw.has(p.base_entry_id))
       ||descendants.has(p.directory_id as string)){
     throw Error('An unfinished Drive upload or copy-on-write edit may hide files. Finish or recover it before exporting the folder.');
   }
 }
 for(const link of src.symlinks){
   const symlink=asRow(link);
   if(!symlink)throw Error('Drive link inventory is malformed.');
   if((symlink.trashed_at===null||symlink.trashed_at===undefined)
      &&descendants.has(symlink.directory_id as string))
     throw Error('Folder includes symbolic links; ZIP export does not support them.');
 }
 if(selectedIds.length>100)
   throw Error('Folder has more than 100 completed files; ZIP export is limited to 100.');
 const totalBytes=selectedIds.length?selectedTotalBytes(visibleFiles,selectedIds):0;
 if(totalBytes>MAX_BULK_EXPORT_BYTES)
   throw Error('Folder contains more than 8 GiB of completed files.');
 // The native ZIP now preserves all folder entries, even ones without files.
 // Report the empty ones explicitly so backup coverage is clear.
 const ancestorsWithFiles=new Set<string>();
 for(const id of selectedIds){
   const file=visible.get(id)!;
   for(const d of folderInventory.folders){
     if(descendants.has(d.id)&&file.path.startsWith(d.path+'/'))
       ancestorsWithFiles.add(d.id);
   }
 }
 return {
   folderId,folderPath:folder.path,entryIds:selectedIds,totalBytes,
   foldersCovered:descendants.size,
   preservedEmptyFolders:descendants.size-ancestorsWithFiles.size,
   revision:snapshot.revision,
 };
}
