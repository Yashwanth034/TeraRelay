import { useCallback, useRef } from 'react';
import { showFileDialogFallback, pickWithFallback, sanitizeFilename } from '../utils';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { useQueryClient } from '@tanstack/react-query';
import { toast } from 'sonner';
import { useConfirm } from '../context/ConfirmContext';
import { TelegramFile } from '../types';
import type { ChannelFolderNode } from '../channelFolderTree';
import { reserveBulkDownloadPath } from '../transferQueue';

export function useFileOperations(
    activeFolderId: number | null,
    selectedIds: number[],
    setSelectedIds: (ids: number[]) => void,
    displayedFiles: TelegramFile[],
    queueBulkDownload?: (files: TelegramFile[], folderId: number | null) => void,
) {
    const queryClient = useQueryClient();
    const { confirm } = useConfirm();

    // Refs to keep callbacks stable even when selection/file list changes
    const selectedIdsRef = useRef(selectedIds);
    selectedIdsRef.current = selectedIds;
    const displayedFilesRef = useRef(displayedFiles);
    displayedFilesRef.current = displayedFiles;

    const handleDelete = useCallback(async (id: number) => {
        if (!await confirm({ title: "Delete File", message: "Are you sure you want to delete this file?", confirmText: "Delete", variant: 'danger' })) return;
        try {
            await invoke('cmd_delete_file', { messageId: id, folderId: activeFolderId });
            await Promise.all([
                invoke('cmd_delete_image_thumbnail', { messageId: id, folderId: activeFolderId }).catch(() => {}),
                invoke('cmd_delete_preview_for_message', { messageId: id, folderId: activeFolderId }).catch(() => {}),
            ]);
            queryClient.invalidateQueries({ queryKey: ['files', activeFolderId] });
            queryClient.invalidateQueries({ queryKey: ['drive-channel-file-paths', activeFolderId] });
            toast.success("File deleted");
        } catch (e) {
            toast.error(`Delete failed: ${e}`);
        }
    }, [activeFolderId, confirm, queryClient]);

    const handleDeleteFolder = useCallback(async (folder: ChannelFolderNode) => {
        if (activeFolderId === null || !folder.directory_id) {
            toast.error('This folder is no longer available. Refresh the channel and try again.');
            return;
        }

        const collectFiles = (node: ChannelFolderNode): TelegramFile[] => [
            ...node.files,
            ...node.folders.flatMap(collectFiles),
        ];
        const files = collectFiles(folder);
        const uniqueFiles = [...new Map(files.map(file => [file.id, file])).values()];
        if (uniqueFiles.some(file => !!file.stack_id)) {
            toast.info('This folder contains version stacks. Manage those versions first, then delete the folder.');
            return;
        }

        const confirmed = await confirm({
            title: files.length > 0 ? 'Delete Folder and Contents' : 'Delete Empty Folder',
            message: files.length > 0
                ? `Delete "${folder.name}" and all ${uniqueFiles.length} files inside it, including nested folders? This permanently deletes those files from the selected Telegram channel and cannot be undone.`
                : `Delete the empty Drive folder "${folder.name}" and its empty nested folders?`,
            confirmText: files.length > 0 ? `Delete ${uniqueFiles.length} Files` : 'Delete Folder',
            variant: 'danger',
        });
        if (!confirmed) return;

        let deleted = 0;
        const failures: string[] = [];
        for (const file of uniqueFiles) {
            try {
                const ok = await invoke<boolean>('cmd_delete_file', {
                    messageId: file.id,
                    folderId: activeFolderId,
                });
                if (!ok) throw new Error('The delete command did not confirm completion');
                await Promise.all([
                    invoke('cmd_delete_image_thumbnail', { messageId: file.id, folderId: activeFolderId }).catch(() => {}),
                    invoke('cmd_delete_preview_for_message', { messageId: file.id, folderId: activeFolderId }).catch(() => {}),
                ]);
                deleted++;
            } catch (error) {
                failures.push(`${file.name}: ${String(error)}`);
            }
        }

        let folderError: string | null = null;
        if (failures.length === 0) {
            try {
                await invoke('cmd_drive_delete_empty_channel_folder_tree', {
                    folderId: activeFolderId,
                    directoryId: folder.directory_id,
                });
            } catch (error) {
                folderError = String(error);
            }
        }

        await Promise.all([
            queryClient.invalidateQueries({ queryKey: ['files', activeFolderId] }),
            queryClient.invalidateQueries({ queryKey: ['drive-channel-file-paths', activeFolderId] }),
        ]);
        setSelectedIds(selectedIdsRef.current.filter(id => !uniqueFiles.some(file => file.id === id)));

        if (failures.length > 0) {
            toast.error(`Deleted ${deleted} of ${uniqueFiles.length} files. ${failures.length} could not be deleted; the remaining folder contents were kept.`);
        } else if (folderError) {
            toast.error(`Deleted ${deleted} files, but could not remove the empty folder: ${folderError}`);
        } else {
            toast.success(
                files.length > 0
                    ? `Deleted folder "${folder.name}" and ${deleted} files.`
                    : `Deleted empty folder "${folder.name}".`,
            );
        }
    }, [activeFolderId, confirm, queryClient, setSelectedIds]);

    const handleBulkDelete = useCallback(async () => {
        const ids = selectedIdsRef.current;
        if (ids.length === 0) return;
        const stacked = displayedFilesRef.current.filter(file => ids.includes(file.id) && !!file.stack_id);
        if (stacked.length > 0) {
            toast.info('Version stacks must be deleted from Manage versions so you can choose whether to unstack or delete every version.');
            return;
        }
        if (!await confirm({ title: "Delete Files", message: `Are you sure you want to delete ${ids.length} files?`, confirmText: "Delete All", variant: 'danger' })) return;

        let success = 0;
        let fail = 0;
        for (const id of ids) {
            try {
                await invoke('cmd_delete_file', { messageId: id, folderId: activeFolderId });
                await Promise.all([
                    invoke('cmd_delete_image_thumbnail', { messageId: id, folderId: activeFolderId }).catch(() => {}),
                    invoke('cmd_delete_preview_for_message', { messageId: id, folderId: activeFolderId }).catch(() => {}),
                ]);
                success++;
            } catch {
                fail++;
            }
        }
        setSelectedIds([]);
        queryClient.invalidateQueries({ queryKey: ['files', activeFolderId] });
        queryClient.invalidateQueries({ queryKey: ['drive-channel-file-paths', activeFolderId] });
        if (success > 0) toast.success(`Deleted ${success} files.`);
        if (fail > 0) toast.error(`Failed to delete ${fail} files.`);
    }, [activeFolderId, confirm, queryClient, setSelectedIds]);

    const handleBulkDownload = useCallback(async () => {
        const ids = selectedIdsRef.current;
        if (ids.length === 0) return;
        const currentFiles = displayedFilesRef.current;
        // Ctrl-click selection order determines bulk download queue order.
        // The displayed list may be sorted differently from the click sequence.
        const byId = new Map(currentFiles.map(file => [file.id, file]));
        const targetFiles = ids.map(id => byId.get(id)).filter((file): file is TelegramFile => !!file);
        if (targetFiles.length === 0) return;
        if (queueBulkDownload) {
            queueBulkDownload(targetFiles, activeFolderId);
            setSelectedIds([]);
            return;
        }
        // Fallback: direct download if queue not provided
        const downloadToDir = async (dirPath: string) => {
            let successCount = 0;
            const usedPaths = new Set<string>();
            for (const file of targetFiles) {
                const filePath = reserveBulkDownloadPath(dirPath, sanitizeFilename(file.name), usedPaths);
                try {
                    await invoke('cmd_download_file', { req: { message_id: file.id, save_path: filePath, folder_id: activeFolderId, no_overwrite: true } });
                    successCount++;
                } catch (error) {
                    toast.error(`Download failed for ${file.name}: ${error}`);
                }
            }
            if (successCount > 0) toast.success(`Downloaded ${successCount} files.`);
            setSelectedIds([]);
        };
        try {
            const dirPath = await pickWithFallback(
                () => open({ directory: true, multiple: false, title: "Select Download Destination" }),
                () => handleBulkDownload(),
                {
                    errorTitle: 'Folder picker failed',
                    onBrowserPicker: async () => {
                        const paths = await showFileDialogFallback({ directory: true, multiple: false });
                        if (paths.length === 0) return null;
                        const sep = paths[0].includes('\\') ? '\\' : '/';
                        return paths[0].substring(0, paths[0].lastIndexOf(sep));
                    },
                },
            );
            if (!dirPath) return;
            await downloadToDir(dirPath);
        } catch (e) {
            toast.error(`Bulk download failed: ${e}`);
        }
    }, [activeFolderId, setSelectedIds, queueBulkDownload]);

    const handleBulkMove = useCallback(async (targetFolderId: number | null, onSuccess?: () => void) => {
        const ids = selectedIdsRef.current;
        if (ids.length === 0) return;
        const stacked = displayedFilesRef.current.filter(file => ids.includes(file.id) && !!file.stack_id);
        if (stacked.length > 0) {
            toast.info('Move version stacks individually so all versions move together safely.');
            return;
        }
        try {
            await invoke('cmd_move_files', {
                messageIds: ids,
                sourceFolderId: activeFolderId,
                targetFolderId: targetFolderId
            });
            if ((activeFolderId ?? 0) < 0 || (targetFolderId ?? 0) < 0) {
                try {
                    const sync = await invoke<{ dirty: boolean; source: string }>('cmd_drive_sync_metadata');
                    if (sync.dirty || sync.source === 'qa-local') {
                        toast.warning('File moved, but shared-channel Drive metadata still needs synchronization.');
                    }
                } catch (error) {
                    toast.warning('File moved, but shared-channel Drive synchronization needs retry: ' + error);
                }
            }
            // Clean up stale thumbnail and preview cache entries for the old message IDs.
            // After a move (forward+delete), the message gets a new ID in the
            // target folder, so old cached thumbnails are orphaned.
            await Promise.all(ids.flatMap(id => [
                invoke('cmd_delete_image_thumbnail', { messageId: id, folderId: activeFolderId }).catch(() => {}),
                invoke('cmd_delete_preview_for_message', { messageId: id, folderId: activeFolderId }).catch(() => {}),
            ]));
            toast.success(`Moved ${ids.length} files.`);
            queryClient.invalidateQueries({ queryKey: ['files', activeFolderId] });
            setSelectedIds([]);
            if (onSuccess) onSuccess();
        } catch (error) {
            toast.error(`Could not finish moving files: ${error}`);
        }
    }, [activeFolderId, queryClient, setSelectedIds]);

    const handleDownloadFolder = useCallback(async () => {
        const files = displayedFilesRef.current;
        if (files.length === 0) {
            toast.info("Folder is empty.");
            return;
        }
        if (queueBulkDownload) {
            queueBulkDownload(files, activeFolderId);
            return;
        }
        // Fallback: direct download if queue not provided
        const downloadToDir = async (dirPath: string) => {
            let successCount = 0;
            toast.info(`Downloading folder contents (${files.length} files)...`);
            const usedPaths = new Set<string>();
            for (const file of files) {
                const filePath = reserveBulkDownloadPath(dirPath, sanitizeFilename(file.name), usedPaths);
                try {
                    await invoke('cmd_download_file', { req: { message_id: file.id, save_path: filePath, folder_id: activeFolderId, no_overwrite: true } });
                    successCount++;
                } catch (error) {
                    toast.error(`Download failed for ${file.name}: ${error}`);
                }
            }
            if (successCount > 0) toast.success(`Folder Download Complete: ${successCount} files.`);
        };
        try {
            const dirPath = await pickWithFallback(
                () => open({
                    directory: true, multiple: false, title: "Download Folder To..."
                }),
                () => handleDownloadFolder(),
                {
                    errorTitle: 'Folder picker failed',
                    onBrowserPicker: async () => {
                        const paths = await showFileDialogFallback({ directory: true, multiple: false });
                        if (paths.length === 0) return null;
                        const sep = paths[0].includes('\\') ? '\\' : '/';
                        return paths[0].substring(0, paths[0].lastIndexOf(sep));
                    },
                },
            );
            if (!dirPath) return;
            await downloadToDir(dirPath);
        } catch (e) {
            toast.error("Error: " + e);
        }
    }, [activeFolderId, queueBulkDownload]);

    const handleGlobalSearch = useCallback(async (query: string) => {
        try {
            return await invoke<TelegramFile[]>('cmd_search_global', { query });
        } catch {
            return [];
        }
    }, []);

    return {
        handleDelete,
        handleDeleteFolder,
        handleBulkDelete,
        handleBulkDownload,
        handleBulkMove,
        handleDownloadFolder,
        handleGlobalSearch,
    };
}
