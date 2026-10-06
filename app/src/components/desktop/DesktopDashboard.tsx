import { useState, useEffect, useCallback, useRef } from 'react';
import { AnimatePresence } from 'framer-motion';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';

import { TelegramFile, BandwidthStats, ShareInfo } from '../../types';
import { formatBytes, getFileCategory, isMediaFile, isVideoFile, isPdfFile, isArchiveFile, nativeShareOrCopy, copyToClipboard, type FileCategory } from '../../utils';

// Components
import { Sidebar } from './dashboard/Sidebar';
import { TopBar } from './dashboard/TopBar';
import { FileExplorer } from './dashboard/FileExplorer';
import { ChannelFeed } from './dashboard/ChannelFeed';
import { UploadQueue } from './dashboard/UploadQueue';
import { DownloadQueue } from './dashboard/DownloadQueue';
import { MoveToFolderModal } from './dashboard/MoveToFolderModal';
import { PreviewModal } from './dashboard/PreviewModal';
import { MediaPlayer } from './dashboard/MediaPlayer';
import { ExternalDropBlocker } from './dashboard/ExternalDropBlocker';
import { PdfViewer } from './dashboard/PdfViewer';
import { ArchiveViewerModal } from './dashboard/ArchiveViewerModal';
import { SettingsModal } from './dashboard/SettingsModal';
import { ShareDialog } from './dashboard/ShareDialog';
import { RenameFolderModal } from './dashboard/RenameFolderModal';
import { RenameFileModal } from './dashboard/RenameFileModal';
import { RemoteUploadModal } from './dashboard/RemoteUploadModal';
import { JoinChannelModal } from './dashboard/JoinChannelModal';
import { ChannelInfoPanel } from './dashboard/ChannelInfoPanel';
import { Link, Copy, Check, X, Loader2, Share2 } from 'lucide-react';

// Hooks
import { useTelegramConnection } from '../../hooks/useTelegramConnection';
import { useFileOperations } from '../../hooks/useFileOperations';
import { useFileUpload } from '../../hooks/useFileUpload';
import { useFileDownload } from '../../hooks/useFileDownload';
import { useKeyboardShortcuts } from '../../hooks/useKeyboardShortcuts';
import { useSettings } from '../../context/SettingsContext';
import { useConfirm } from '../../context/ConfirmContext';

export function Dashboard({ onLogout }: { onLogout: () => void }) {
    const queryClient = useQueryClient();


    const {
        store, folders, groups, activeFolderId, setActiveFolderId, isSyncing, isConnected,
        handleLogout, handleSyncFolders, handleCreateFolder, handleFolderDelete,
        handleFolderRename, handleFolderToggleVisibility, handleExportFolderInvite, handleJoinChannel,
        handleCreateGroup, handleDeleteGroup, handleUpdateGroup, handleAssignFolderToGroup,
        handleReorderFolders, handleUpdateGroupOrder
    } = useTelegramConnection(onLogout);


    const { settings, updateSetting } = useSettings();
    const { confirm } = useConfirm();
    const viewMode = settings.viewMode;
    const setViewMode = (mode: 'grid' | 'list') => updateSetting('viewMode', mode);

    const [previewFile, setPreviewFile] = useState<TelegramFile | null>(null);
    const [selectedIds, setSelectedIds] = useState<number[]>([]);
    const [showMoveModal, setShowMoveModal] = useState(false);
    const [showSettings, setShowSettings] = useState(false);
    const [searchTerm, setSearchTerm] = useState("");
    const [searchResults, setSearchResults] = useState<TelegramFile[]>([]);
    const [isSearching, setIsSearching] = useState(false);
    const [cardScale, setCardScale] = useState(1.0);
    const internalDragRef = useRef<number[] | null>(null);

    const setInternalDragIds = (ids: number[] | null) => {
        internalDragRef.current = ids;
    };
    const [showRemoteUpload, setShowRemoteUpload] = useState(false);
    const [playingFile, setPlayingFile] = useState<TelegramFile | null>(null);
    const [pdfFile, setPdfFile] = useState<TelegramFile | null>(null);
    const [archiveViewFile, setArchiveViewFile] = useState<TelegramFile | null>(null);
    const [shareFile, setShareFile] = useState<TelegramFile | null>(null);
    const [bulkShareLinks, setBulkShareLinks] = useState<Array<{ file: TelegramFile; link: string }> | null>(null);
    const [bulkShareLoading, setBulkShareLoading] = useState(false);
    const [bulkShareCopied, setBulkShareCopied] = useState<Set<string>>(new Set());
    const [previewContextFiles, setPreviewContextFiles] = useState<TelegramFile[]>([]);
    const [previewContextIndex, setPreviewContextIndex] = useState(-1);
    const [renameFolder, setRenameFolder] = useState<{ id: number; name: string } | null>(null);
    const [showJoinChannel, setShowJoinChannel] = useState(false);
    const [showChannelInfo, setShowChannelInfo] = useState(false);
    const [channelFileCategory, setChannelFileCategory] = useState<FileCategory>('all');
    const [moveFileTarget, setMoveFileTarget] = useState<TelegramFile | null>(null);
    const [renameFileTarget, setRenameFileTarget] = useState<TelegramFile | null>(null);

    const activeChannel = activeFolderId === null
        ? null
        : folders.find(f => f.id === activeFolderId) || null;
    const currentFolderName = activeChannel?.name || "Personal Vault";
    const activeChannelReadOnly = activeChannel?.role === 'member';

    const { data: allFiles = [], isLoading, error } = useQuery({
        queryKey: ['files', activeFolderId],
        queryFn: () => invoke<Array<{ id: number; name: string; size: number; icon_type: string; folder_id: number | null; created_at: string; mime_type?: string; file_ext?: string; is_split?: boolean }>>('cmd_get_files', { folderId: activeFolderId }).then(res => res.map(f => ({
            ...f,
            sizeStr: formatBytes(f.size),
            type: (f.icon_type as TelegramFile['type']) || 'file'
        }))),
        enabled: !!store,
    });

    const displayedFiles = activeFolderId !== null
        ? allFiles.filter((f: TelegramFile) =>
            f.name.toLowerCase().includes(searchTerm.trim().toLowerCase())
            && (channelFileCategory === 'all' || getFileCategory(f.name) === channelFileCategory)
        )
        : searchTerm.length > 2
            ? searchResults
            : allFiles.filter((f: TelegramFile) =>
                f.name.toLowerCase().includes(searchTerm.trim().toLowerCase())
            );

    const activeCategoryLabel: Record<FileCategory, string | undefined> = {
        all: undefined,
        video: 'Videos',
        image: 'Images',
        document: 'Documents',
        audio: 'Audio',
        other: 'Other',
    };

    const { data: bandwidth } = useQuery({
        queryKey: ['bandwidth'],
        queryFn: () => invoke<BandwidthStats>('cmd_get_bandwidth'),
        refetchInterval: 5000,
        enabled: !!store
    });


    const {
        uploadQueue,
        handleManualUpload,
        handleFolderUpload,
        handleDropUpload,
        handleUrlUpload,
        cancelAll: cancelUploads,
        cancelItem: cancelUploadItem,
        pauseItem: pauseUploadItem,
        resumeItem: resumeUploadItem,
        retryItem: retryUploadItem,
    } = useFileUpload(activeFolderId, store);
    const {
        downloadQueue,
        queueDownload,
        queueBulkDownload,
        cancelAll: cancelDownloads,
        cancelItem: cancelDownloadItem,
        pauseItem: pauseDownloadItem,
        resumeItem: resumeDownloadItem,
        retryItem: retryDownloadItem,
    } = useFileDownload(store);

    const {
        handleDelete, handleBulkDelete, handleBulkDownload,
        handleBulkMove, handleDownloadFolder, handleGlobalSearch

    } = useFileOperations(activeFolderId, selectedIds, setSelectedIds, displayedFiles, queueBulkDownload);

    // Bulk share: generate links for all selected non-folder files
    const handleBulkShare = useCallback(async () => {
        if (activeChannelReadOnly) {
            toast.error('Members can download files but cannot create share links.');
            return;
        }
        const shareFiles = displayedFiles.filter(f => selectedIds.includes(f.id) && f.type !== 'folder' && !f.is_split);
        if (shareFiles.length === 0) {
            toast.info('No shareable files selected (folders cannot be shared)');
            return;
        }
        setBulkShareLinks([]);
        setBulkShareLoading(true);
        setBulkShareCopied(new Set());
        try {
            const results = await Promise.all(
                shareFiles.map(async (file) => {
                    try {
                        const info = await invoke<ShareInfo>('cmd_create_share', {
                            folderId: activeFolderId,
                            messageId: file.id,
                            fileName: file.name,
                            fileSize: file.size,
                            password: null,
                            expiryHours: 24,
                        });
                        return { file, link: info.link };
                    } catch (e) {
                        toast.error(`Failed to share ${file.name}: ${e}`);
                        return null;
                    }
                })
            );
            const valid = results.filter((r): r is { file: TelegramFile; link: string } => r !== null);
            if (valid.length > 0) {
                setBulkShareLinks(valid);
                setSelectedIds([]);
            } else {
                setBulkShareLinks(null);
                toast.error('Failed to generate any share links');
            }
        } finally {
            setBulkShareLoading(false);
        }
    }, [activeChannelReadOnly, activeFolderId, displayedFiles, selectedIds]);

    const handleCopyBulkLink = useCallback((link: string) => {
        navigator.clipboard.writeText(link);
        setBulkShareCopied(prev => new Set(prev).add(link));
        setTimeout(() => setBulkShareCopied(prev => {
            const next = new Set(prev);
            next.delete(link);
            return next;
        }), 2000);
    }, []);


    const handleSelectAll = useCallback(() => {
        setSelectedIds(displayedFiles.map(f => f.id));
    }, [displayedFiles]);

    const handleKeyboardDelete = useCallback(() => {
        if (activeChannelReadOnly) return;
        if (selectedIds.length > 0) {
            handleBulkDelete();
        }
    }, [activeChannelReadOnly, selectedIds, handleBulkDelete]);

    const handleEscape = useCallback(() => {
        lastClickedIndexRef.current = -1;
        setSelectedIds([]);
        setSearchTerm("");
        setPreviewFile(null);
        setPlayingFile(null);
        setPdfFile(null);
        setArchiveViewFile(null);
    }, []);

    const handleFocusSearch = useCallback(() => {
        const searchInput = document.querySelector(
            'input[aria-label^="Search files in "], input[placeholder="Search files..."]'
        ) as HTMLInputElement | null;
        if (searchInput) {
            searchInput.focus();
            searchInput.select();
        }
    }, []);

    const handleEnter = useCallback(() => {
        if (selectedIds.length === 1) {
            const selected = displayedFiles.find(f => f.id === selectedIds[0]);
            if (selected) {
                if (selected.type === 'folder') {
                    setActiveFolderId(selected.id);
                } else {
                    handlePreview(selected, displayedFiles);
                }
            }
        }
    }, [selectedIds, displayedFiles, setActiveFolderId]);


    useEffect(() => {
        lastClickedIndexRef.current = -1;
        setSelectedIds([]);
        setShowMoveModal(false);
        setSearchTerm("");
        setSearchResults([]);
        setPreviewFile(null);
        setPlayingFile(null);
        setPdfFile(null);
        setPreviewContextFiles([]);
        setPreviewContextIndex(-1);
        setArchiveViewFile(null);
        setShowChannelInfo(false);
        setChannelFileCategory('all');
    }, [activeFolderId]);


    useEffect(() => {
        // Channel search is intentionally local to the active logical channel.
        // Global Telegram search is only used outside channel mode.
        if (activeFolderId !== null) {
            setSearchResults([]);
            setIsSearching(false);
            return;
        }
        if (searchTerm.length <= 2) {
            setSearchResults([]);
            setIsSearching(false);
            return;
        }

        const timer = setTimeout(async () => {
            setIsSearching(true);
            const results = await handleGlobalSearch(searchTerm);
            setSearchResults(results);
            setIsSearching(false);
        }, 500);

        return () => clearTimeout(timer);
    }, [activeFolderId, searchTerm, handleGlobalSearch]);




    const lastClickedIndexRef = useRef<number>(-1);

    const clearSelection = useCallback(() => {
        lastClickedIndexRef.current = -1;
        setSelectedIds([]);
    }, []);

    const handleFileClick = (e: React.MouseEvent, id: number) => {
        e.stopPropagation();
        const currentIndex = displayedFiles.findIndex(f => f.id === id);

        if (e.shiftKey && lastClickedIndexRef.current >= 0) {
            // Shift+Click: range select from last clicked to current
            const start = Math.min(lastClickedIndexRef.current, currentIndex);
            const end = Math.max(lastClickedIndexRef.current, currentIndex);
            const rangeIds = displayedFiles.slice(start, end + 1).map(f => f.id);
            setSelectedIds(rangeIds);
        } else if (e.metaKey || e.ctrlKey) {
            // Ctrl/Cmd+Click: toggle individual file
            lastClickedIndexRef.current = currentIndex;
            setSelectedIds(ids => ids.includes(id) ? ids.filter(i => i !== id) : [...ids, id]);
        } else {
            // Plain click: select single file
            lastClickedIndexRef.current = currentIndex;
            setSelectedIds([id]);
        }
    }

    const handleToggleSelection = useCallback((id: number) => {
        setSelectedIds(ids => ids.includes(id) ? ids.filter(i => i !== id) : [...ids, id]);
    }, []);

    const handleFileMove = useCallback((file: TelegramFile) => {
        setMoveFileTarget(file);
        setShowMoveModal(true);
    }, []);

    const handleRename = useCallback((file: TelegramFile) => {
        setRenameFileTarget(file);
    }, []);

    const handleRenameSubmit = useCallback(async (newName: string) => {
        if (!renameFileTarget) return;
        try {
            await invoke('cmd_rename_file', {
                messageId: renameFileTarget.id,
                folderId: activeFolderId,
                newName,
            });
            queryClient.invalidateQueries({ queryKey: ['files', activeFolderId] });
            toast.success(`Renamed to "${newName}"`);
        } catch (e) {
            toast.error(`Failed to rename: ${e}`);
            throw e;
        }
    }, [renameFileTarget, activeFolderId, queryClient]);

    const handleKeyboardDownload = useCallback(() => {
        if (selectedIds.length > 0) {
            handleBulkDownload();
        }
    }, [selectedIds, handleBulkDownload]);

    const handleKeyboardShare = useCallback(() => {
        if (activeChannelReadOnly) return;
        if (selectedIds.length > 0) {
            handleBulkShare();
        }
    }, [activeChannelReadOnly, selectedIds, handleBulkShare]);

    const handleKeyboardRename = useCallback(() => {
        if (activeChannelReadOnly) return;
        if (selectedIds.length === 1) {
            const selected = displayedFiles.find(f => f.id === selectedIds[0]);
            if (selected && selected.type !== 'folder') {
                handleRename(selected);
            }
        }
    }, [activeChannelReadOnly, selectedIds, displayedFiles, handleRename]);

    useKeyboardShortcuts({
        onSelectAll: handleSelectAll,
        onDelete: handleKeyboardDelete,
        onEscape: handleEscape,
        onSearch: handleFocusSearch,
        onEnter: handleEnter,
        onDownload: handleKeyboardDownload,
        onShare: handleKeyboardShare,
        onRename: handleKeyboardRename,
        enabled: !previewFile && !playingFile && !pdfFile && !archiveViewFile && !showMoveModal
    });

    const handlePreview = (file: TelegramFile, orderedFiles?: TelegramFile[]) => {
        if (file.is_split && !isVideoFile(file.name)) {
            // Other split previews still require reconstruction before opening.
            toast.info('This file is stored in multiple parts. Download it to open it.');
            return;
        }
        const contextFiles = (orderedFiles || displayedFiles).filter((f) => f.type !== 'folder');
        const contextIndex = contextFiles.findIndex((f) => f.id === file.id);

        setPreviewContextFiles(contextFiles);
        setPreviewContextIndex(contextIndex);

        const isMedia = isMediaFile(file.name);
        const isPdf = isPdfFile(file.name);
        const isArchive = isArchiveFile(file.name);

        if (isArchive) {
            setArchiveViewFile(file);
            setPreviewFile(null);
            setPlayingFile(null);
            setPdfFile(null);
        } else if (isMedia) {
            setPlayingFile(file);
            setPreviewFile(null);
            setPdfFile(null);
            setArchiveViewFile(null);
        } else if (isPdf) {
            setPdfFile(file);
            setPreviewFile(null);
            setPlayingFile(null);
            setArchiveViewFile(null);
        } else {
            setPreviewFile(file);
            setPlayingFile(null);
            setPdfFile(null);
            setArchiveViewFile(null);
        }
    };

    const navigatePreview = useCallback((step: 1 | -1) => {
        if (previewContextFiles.length === 0) return;

        const currentFileId = previewFile?.id ?? playingFile?.id ?? pdfFile?.id ?? archiveViewFile?.id;
        if (!currentFileId) return;

        const currentIndex = previewContextFiles.findIndex((f) => f.id === currentFileId);
        if (currentIndex === -1) return;

        const nextIndex = (currentIndex + step + previewContextFiles.length) % previewContextFiles.length;
        const nextFile = previewContextFiles[nextIndex];
        if (!nextFile) return;

        setPreviewContextIndex(nextIndex);

        const isMedia = isMediaFile(nextFile.name);
        const isPdf = isPdfFile(nextFile.name);
        const isArchive = isArchiveFile(nextFile.name);

        if (isArchive) {
            setArchiveViewFile(nextFile);
            setPreviewFile(null);
            setPlayingFile(null);
            setPdfFile(null);
        } else if (isMedia) {
            setPlayingFile(nextFile);
            setPreviewFile(null);
            setPdfFile(null);
            setArchiveViewFile(null);
        } else if (isPdf) {
            setPdfFile(nextFile);
            setPreviewFile(null);
            setPlayingFile(null);
            setArchiveViewFile(null);
        } else {
            setPreviewFile(nextFile);
            setPlayingFile(null);
            setPdfFile(null);
            setArchiveViewFile(null);
        }
    }, [previewContextFiles, previewFile, playingFile, pdfFile, archiveViewFile]);

    const handleNextPreview = useCallback(() => {
        navigatePreview(1);
    }, [navigatePreview]);

    const handlePrevPreview = useCallback(() => {
        navigatePreview(-1);
    }, [navigatePreview]);

    const previewNeighborFiles = useCallback(() => {
        if (previewContextFiles.length === 0) {
            return { nextFile: null as TelegramFile | null, prevFile: null as TelegramFile | null };
        }

        const currentFileId = previewFile?.id ?? playingFile?.id ?? pdfFile?.id ?? archiveViewFile?.id;
        if (!currentFileId) {
            return { nextFile: null as TelegramFile | null, prevFile: null as TelegramFile | null };
        }

        const currentIdx = previewContextFiles.findIndex((f) => f.id === currentFileId);
        if (currentIdx === -1) {
            return { nextFile: null as TelegramFile | null, prevFile: null as TelegramFile | null };
        }

        const nextIdx = (currentIdx + 1) % previewContextFiles.length;
        const prevIdx = (currentIdx - 1 + previewContextFiles.length) % previewContextFiles.length;

        return {
            nextFile: previewContextFiles[nextIdx] || null,
            prevFile: previewContextFiles[prevIdx] || null,
        };
    }, [previewContextFiles, previewFile, playingFile, pdfFile, archiveViewFile]);

    const handleDropOnFolder = async (e: React.DragEvent, targetFolderId: number | null) => {
        e.preventDefault();
        e.stopPropagation();

        // Read multi-ID drag data (new format) or fall back to single-ID (legacy)
        let idsToMove: number[] | null = null;
        const rawIds = e.dataTransfer.getData("application/x-telegram-file-ids");
        if (rawIds) {
            try { idsToMove = JSON.parse(rawIds); } catch { /* ignore parse errors */ }
        }
        if (!idsToMove || idsToMove.length === 0) {
            const singleId = e.dataTransfer.getData("application/x-telegram-file-id");
            if (singleId) idsToMove = [parseInt(singleId)];
        }
        if (!idsToMove || idsToMove.length === 0) {
            idsToMove = internalDragRef.current;
        }
        if (!idsToMove || idsToMove.length === 0) return;

        if (activeFolderId === targetFolderId) {
            toast.info('File is already in this folder');
            return;
        }

        if (idsToMove.length >= 10) {
            const confirmed = await confirm({
                title: 'Bulk Move Confirmation',
                message: `You are about to move ${idsToMove.length} files. Are you sure?`,
                confirmText: `Move ${idsToMove.length} Files`,
                variant: 'info',
            });
            if (!confirmed) return;
        }

        try {
            await invoke('cmd_move_files', {
                messageIds: idsToMove,
                sourceFolderId: activeFolderId,
                targetFolderId: targetFolderId
            });
            // Clean up stale thumbnail and preview cache entries for the old message IDs
            await Promise.all(idsToMove.flatMap(id => [
                invoke('cmd_delete_image_thumbnail', { messageId: id, folderId: activeFolderId }).catch(() => {}),
                invoke('cmd_delete_preview_for_message', { messageId: id, folderId: activeFolderId }).catch(() => {}),
            ]));

            queryClient.invalidateQueries({ queryKey: ['files', activeFolderId] });
            setSelectedIds([]);
            toast.success(`Moved ${idsToMove.length} file(s).`);
            setInternalDragIds(null);
        } catch {
            toast.error(`Failed to move file(s).`);
        }
    }

    const handleRootDragOver = (e: React.DragEvent) => {
        // Accept our internal file drags (custom MIME type) so drops work anywhere
        const isInternalDrag = internalDragRef.current !== null ||
            e.dataTransfer.types.includes("application/x-telegram-file-id") ||
            e.dataTransfer.types.includes("application/x-telegram-file-ids");
        if (isInternalDrag) {
            e.preventDefault();
            e.stopPropagation();
            e.dataTransfer.dropEffect = 'move';
        }
    };

    const handleRootDragEnter = (e: React.DragEvent) => {
        const isInternalDrag = internalDragRef.current !== null ||
            e.dataTransfer.types.includes("application/x-telegram-file-id") ||
            e.dataTransfer.types.includes("application/x-telegram-file-ids");
        if (isInternalDrag) {
            e.preventDefault();
            e.stopPropagation();
            e.dataTransfer.dropEffect = 'move';
        }
    };

    const previewNeighbors = previewNeighborFiles();

    return (
        <div
            className="terarelay-shell flex h-screen w-full overflow-hidden bg-telegram-bg relative"
            onDragOver={handleRootDragOver}
            onDragEnter={handleRootDragEnter}
        >

            <ExternalDropBlocker
                onFilesDropped={handleDropUpload}
                onUploadClick={handleManualUpload}
                disabled={activeChannelReadOnly}
            />
            <AnimatePresence>
                {showMoveModal && (
                    <MoveToFolderModal
                        folders={folders}
                        fileName={moveFileTarget?.name}
                        onClose={() => { setShowMoveModal(false); setMoveFileTarget(null); }}
                        onSelect={async (targetFolderId: number | null) => {
                            if (moveFileTarget) {
                                try {
                                    await invoke('cmd_move_files', {
                                        messageIds: [moveFileTarget.id],
                                        sourceFolderId: activeFolderId,
                                        targetFolderId,
                                    });
                                    // Clean up stale thumbnail and preview cache for the old message ID
                                    await Promise.all([
                                        invoke('cmd_delete_image_thumbnail', { messageId: moveFileTarget.id, folderId: activeFolderId }).catch(() => {}),
                                        invoke('cmd_delete_preview_for_message', { messageId: moveFileTarget.id, folderId: activeFolderId }).catch(() => {}),
                                    ]);
                                    queryClient.invalidateQueries({ queryKey: ['files', activeFolderId] });
                                    toast.success(`Moved "${moveFileTarget.name}"`);
                                    setMoveFileTarget(null);
                                    setShowMoveModal(false);
                                } catch {
                                    toast.error('Failed to move file');
                                }
                            } else {
                                handleBulkMove(targetFolderId, () => setShowMoveModal(false));
                            }
                        }}
                        activeFolderId={activeFolderId}
                        key="move-modal"
                    />
                )}
                {playingFile && (
                    <MediaPlayer
                        file={playingFile}
                        onClose={() => setPlayingFile(null)}
                        onNext={handleNextPreview}
                        onPrev={handlePrevPreview}
                        currentIndex={previewContextIndex}
                        totalItems={previewContextFiles.length}
                        activeFolderId={activeFolderId}
                        key="media-player"
                    />
                )}
                {pdfFile && (
                    <PdfViewer
                        file={pdfFile}
                        onClose={() => setPdfFile(null)}
                        onNext={handleNextPreview}
                        onPrev={handlePrevPreview}
                        currentIndex={previewContextIndex}
                        totalItems={previewContextFiles.length}
                        activeFolderId={activeFolderId}
                        key="pdf-viewer"
                    />
                )}
                {showRemoteUpload && (
                    <RemoteUploadModal
                        isOpen={showRemoteUpload}
                        onClose={() => setShowRemoteUpload(false)}
                        folders={folders}
                        defaultFolderId={activeFolderId}
                        onUpload={handleUrlUpload}
                        key="remote-upload-modal"
                    />
                )}
            </AnimatePresence>

            <Sidebar
                folders={folders}
                groups={groups}
                activeFolderId={activeFolderId}
                setActiveFolderId={setActiveFolderId}
                onDrop={handleDropOnFolder}
                onDelete={handleFolderDelete}
                onRename={(id, name) => setRenameFolder({ id, name })}
                onToggleVisibility={async (id, _name, isPublic) => {
                    try {
                        await handleFolderToggleVisibility(id, !isPublic);
                        queryClient.invalidateQueries({ queryKey: ['folders'] });
                    } catch { /* toast handled in hook */ }
                }}
                onCreate={handleCreateFolder}
                onJoinChannel={() => setShowJoinChannel(true)}
                isSyncing={isSyncing}
                isConnected={isConnected}
                onSync={handleSyncFolders}
                onLogout={handleLogout}
                bandwidth={bandwidth || null}
                onAssignFolderToGroup={handleAssignFolderToGroup}
                onReorderFolders={handleReorderFolders}
                onUpdateGroupOrder={handleUpdateGroupOrder}
                onCreateGroup={handleCreateGroup}
                onUpdateGroup={handleUpdateGroup}
                onDeleteGroup={handleDeleteGroup}
            />

            <main className="tr-main-panel flex-1 flex flex-col">
                <TopBar
                    currentFolderName={currentFolderName}
                    selectedIds={selectedIds}
                    onShowMoveModal={() => setShowMoveModal(true)}
                    onBulkDownload={handleBulkDownload}
                    onBulkDelete={handleBulkDelete}
                    onBulkShare={handleBulkShare}
                    onDownloadFolder={handleDownloadFolder}
                    onClearSelection={clearSelection}
                    viewMode={viewMode}
                    setViewMode={setViewMode}
                    searchTerm={searchTerm}
                    onSearchChange={setSearchTerm}
                    onSettingsClick={() => setShowSettings(true)}
                    onRemoteUploadClick={() => setShowRemoteUpload(true)}
                    readOnly={activeChannelReadOnly}
                    channelMode={activeFolderId !== null}
                />
                {activeFolderId === null && searchTerm.length > 2 && (
                    <div className="px-6 pt-4 pb-0">
                        <h2 className="text-sm font-medium text-telegram-subtext">
                            Search Results for <span className="text-telegram-primary">"{searchTerm}"</span>
                        </h2>
                    </div>
                )}
                {activeFolderId !== null ? (
                    <ChannelFeed
                        channelName={currentFolderName}
                        files={displayedFiles}
                        loading={isLoading || isSearching}
                        error={error}
                        selectedIds={selectedIds}
                        activeFolderId={activeFolderId}
                        readOnly={activeChannelReadOnly}
                        searchTerm={searchTerm}
                        onSearchChange={setSearchTerm}
                        onFileClick={handleFileClick}
                        onToggleSelection={handleToggleSelection}
                        onDelete={handleDelete}
                        onDownload={(id, name) => queueDownload(id, name, activeFolderId)}
                        onPreview={handlePreview}
                        onManualUpload={handleManualUpload}
                        onFolderUpload={handleFolderUpload}
                        onChannelInfo={() => setShowChannelInfo(true)}
                        totalFileCount={allFiles.length}
                        activeCategoryLabel={activeCategoryLabel[channelFileCategory]}
                        showFolderUpload={true}
                        onDrop={handleDropOnFolder}
                        onShare={(f) => {
                            if (f.is_split) {
                                toast.info('Large logical files are shared at the channel level for now.');
                                return;
                            }
                            setShareFile(f);
                        }}
                        onRename={handleRename}
                        onFileMove={handleFileMove}
                    />
                ) : (
                    <FileExplorer
                        folders={folders}
                        files={displayedFiles}
                        loading={isLoading || isSearching}
                        error={error}
                        viewMode={viewMode}
                        selectedIds={selectedIds}
                        activeFolderId={activeFolderId}
                        onFileClick={handleFileClick}
                        onDelete={handleDelete}
                        onDownload={(id, name) => queueDownload(id, name, activeFolderId)}
                        onPreview={handlePreview}
                        onManualUpload={handleManualUpload}
                        onFolderUpload={handleFolderUpload}
                        showFolderUpload={true}
                        onToggleSelection={handleToggleSelection}
                        onDrop={handleDropOnFolder}
                        onDragStart={(ids) => setInternalDragIds(ids)}
                        onDragEnd={() => setTimeout(() => setInternalDragIds(null), 50)}
                        onShare={(f) => {
                            if (f.is_split) {
                                toast.info('Split files cannot be shared. Download to get the full file.');
                                return;
                            }
                            setShareFile(f);
                        }}
                        onRename={handleRename}
                        onFileMove={handleFileMove}
                        cardScale={cardScale}
                        onCardScaleChange={setCardScale}
                    />
                )}
            </main>

            {previewFile && (
                <PreviewModal
                    file={previewFile}
                    activeFolderId={activeFolderId}
                    onClose={() => setPreviewFile(null)}
                    onNext={handleNextPreview}
                    onPrev={handlePrevPreview}
                    currentIndex={previewContextIndex}
                    totalItems={previewContextFiles.length}
                    nextFile={previewNeighbors.nextFile}
                    prevFile={previewNeighbors.prevFile}
                />
            )}

            {archiveViewFile && (
                <ArchiveViewerModal
                    file={archiveViewFile}
                    activeFolderId={activeFolderId}
                    folders={folders}
                    onClose={() => setArchiveViewFile(null)}
                    onNext={handleNextPreview}
                    onPrev={handlePrevPreview}
                    currentIndex={previewContextIndex}
                    totalItems={previewContextFiles.length}
                    nextFile={previewNeighbors.nextFile}
                    prevFile={previewNeighbors.prevFile}
                />
            )}


            {(uploadQueue.length > 0 || downloadQueue.length > 0) && (
                <div className="fixed bottom-24 right-5 z-[100] flex max-h-[calc(100vh-8rem)] w-[380px] max-w-[calc(100vw-2.5rem)] flex-col gap-3 overflow-y-auto">
                    <UploadQueue
                        items={uploadQueue}
                        onCancelAll={cancelUploads}
                        onCancelItem={cancelUploadItem}
                        onPauseItem={pauseUploadItem}
                        onResumeItem={resumeUploadItem}
                        onRetryItem={retryUploadItem}
                    />
                    <DownloadQueue
                        items={downloadQueue}
                        onCancelAll={cancelDownloads}
                        onCancelItem={cancelDownloadItem}
                        onPauseItem={pauseDownloadItem}
                        onResumeItem={resumeDownloadItem}
                        onRetryItem={retryDownloadItem}
                    />
                </div>
            )}

            {activeChannel && activeFolderId !== null && (
                <ChannelInfoPanel
                    open={showChannelInfo}
                    channelId={activeFolderId}
                    channelName={currentFolderName}
                    role={activeChannel.role ?? 'owner'}
                    files={allFiles}
                    activeCategory={channelFileCategory}
                    onSelectCategory={setChannelFileCategory}
                    onClose={() => setShowChannelInfo(false)}
                    onInvite={activeChannelReadOnly ? undefined : async () => {
                        try {
                            const info = await handleExportFolderInvite(activeFolderId);
                            await copyToClipboard(info.link);
                            toast.success(`TeraRelay channel invite copied for "${currentFolderName}".`);
                        } catch {
                            // Backend/hook already reports the failure.
                        }
                    }}
                />
            )}

            <JoinChannelModal
                open={showJoinChannel}
                onClose={() => setShowJoinChannel(false)}
                onJoin={async (invite) => {
                    await handleJoinChannel(invite);
                    queryClient.invalidateQueries({ queryKey: ['files'] });
                }}
            />

            <SettingsModal
                isOpen={showSettings}
                onClose={() => setShowSettings(false)}
            />


            {shareFile && !activeChannelReadOnly && (
                <ShareDialog
                    file={shareFile}
                    folderId={activeFolderId}
                    onClose={() => setShareFile(null)}
                />
            )}

            {renameFolder && (
                <RenameFolderModal
                    folderId={renameFolder.id}
                    currentName={renameFolder.name}
                    onRename={handleFolderRename}
                    onClose={() => setRenameFolder(null)}
                />
            )}

            {renameFileTarget && (
                <RenameFileModal
                    fileName={renameFileTarget.name}
                    onRename={handleRenameSubmit}
                    onClose={() => setRenameFileTarget(null)}
                />
            )}

            {/* Bulk Share Results Modal */}
            {bulkShareLinks && (
                <div
                    className="fixed inset-0 z-[200] flex items-center justify-center bg-black/60 backdrop-blur-sm"
                    onClick={() => setBulkShareLinks(null)}
                >
                    <div
                        className="bg-telegram-surface border border-telegram-border rounded-xl w-[500px] max-h-[70vh] shadow-2xl overflow-hidden flex flex-col animate-in fade-in zoom-in-95 duration-150"
                        onClick={e => e.stopPropagation()}
                    >
                        <div className="p-4 border-b border-telegram-border flex items-center justify-between">
                            <h3 className="text-telegram-text font-medium flex items-center gap-2">
                                <Link className="w-5 h-5 text-telegram-primary" />
                                {bulkShareLinks.length} Share Link{bulkShareLinks.length !== 1 ? 's' : ''}
                            </h3>
                            <button onClick={() => setBulkShareLinks(null)} className="text-telegram-subtext hover:text-telegram-text">
                                <X className="w-5 h-5" />
                            </button>
                        </div>

                        {bulkShareLoading ? (
                            <div className="flex flex-col items-center justify-center py-16 space-y-3">
                                <Loader2 className="w-8 h-8 text-telegram-primary animate-spin" />
                                <p className="text-sm text-telegram-subtext">Generating share links...</p>
                            </div>
                        ) : (
                            <div className="flex-1 overflow-y-auto p-4 space-y-2 min-h-0">
                                {bulkShareLinks.map(({ file, link }) => {
                                    const isCopied = bulkShareCopied.has(link);
                                    return (
                                        <div
                                            key={file.id}
                                            className="p-3 rounded-lg bg-telegram-hover/30 border border-telegram-border/30 space-y-2"
                                        >
                                            <p className="text-xs font-semibold text-telegram-text truncate">{file.name}</p>
                                            <div className="flex gap-2">
                                                <input
                                                    type="text"
                                                    readOnly
                                                    value={link}
                                                    className="flex-1 bg-telegram-bg border border-telegram-border rounded-lg px-2.5 py-1.5 text-xs text-telegram-text focus:outline-none select-all truncate"
                                                />
                                                <button
                                                    onClick={() => handleCopyBulkLink(link)}
                                                    className={`px-2.5 py-1.5 rounded-lg border flex items-center justify-center transition-all flex-shrink-0 ${
                                                        isCopied
                                                            ? 'bg-emerald-500 border-emerald-500 text-white'
                                                            : 'bg-telegram-hover border-telegram-border text-telegram-text hover:bg-white/10'
                                                    }`}
                                                >
                                                    {isCopied ? <Check className="w-3.5 h-3.5" /> : <Copy className="w-3.5 h-3.5" />}
                                                </button>
                                                {typeof navigator !== 'undefined' && typeof navigator.share === 'function' && (
                                                    <button
                                                        onClick={() => nativeShareOrCopy(file.name, file.sizeStr, link, () => handleCopyBulkLink(link))}
                                                        className="px-2.5 py-1.5 rounded-lg bg-telegram-primary/20 hover:bg-telegram-primary/30 text-telegram-primary border border-telegram-primary/30 transition-all flex items-center justify-center flex-shrink-0"
                                                    >
                                                        <Share2 className="w-3.5 h-3.5" />
                                                    </button>
                                                )}
                                            </div>
                                        </div>
                                    );
                                })}
                            </div>
                        )}

                        <button
                            onClick={() => setBulkShareLinks(null)}
                            className="w-full px-4 py-2.5 border-t border-telegram-border bg-telegram-hover/20 hover:bg-telegram-hover/40 text-telegram-text text-sm font-medium transition-colors"
                        >
                            Done
                        </button>
                    </div>
                </div>
            )}
        </div>
    );
}
