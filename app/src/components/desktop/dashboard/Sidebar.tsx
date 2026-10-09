import { useState } from 'react';
import { HardDrive, Folder, Plus, RefreshCw, LogOut, ChevronLeft, ChevronRight, Settings2, Trash2, Check, X, Eye, EyeOff, UserPlus } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { SidebarItem } from './SidebarItem';
import { TeraRelayBrand } from '../../shared/TeraRelayBrand';
import { BandwidthWidget } from './BandwidthWidget';
import { TelegramFolder, BandwidthStats, FolderGroup } from '../../../types';
import { useSettings } from '../../../context/SettingsContext';
import {
    PremiumBadge,
    PremiumButton,
    PremiumIconButton,
    PremiumSurface,
} from '../../ui/PremiumPrimitives';
import {
    DndContext,
    closestCenter,
    KeyboardSensor,
    PointerSensor,
    useSensor,
    useSensors,
    DragEndEvent,
} from '@dnd-kit/core';
import {
    arrayMove,
    SortableContext,
    sortableKeyboardCoordinates,
    verticalListSortingStrategy,
    horizontalListSortingStrategy,
    useSortable,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';

const PRESET_COLORS = [
    '#3B82F6', // Blue
    '#10B981', // Green
    '#8B5CF6', // Purple
    '#EC4899', // Pink
    '#F59E0B', // Orange
    '#14B8A6', // Teal
    '#06B6D4', // Cyan
    '#EF4444', // Red
];

interface GroupTabProps {
    id: string;
    groupId: number | null | 'all';
    label: string;
    colorHex?: string;
    active: boolean;
    onClick: () => void;
    onEdit?: () => void;
    isSortable?: boolean;
}

function GroupTab({ id, groupId, label, colorHex, active, onClick, onEdit, isSortable = true }: GroupTabProps) {
    const {
        attributes,
        listeners,
        setNodeRef,
        transform,
        transition,
        isDragging,
    } = useSortable({
        id,
        disabled: !isSortable,
    });

    const style = {
        transform: CSS.Transform.toString(transform),
        transition,
        opacity: isDragging ? 0.5 : undefined,
    };

    return (
        <div
            ref={setNodeRef}
            style={style}
            {...attributes}
            {...listeners}
            onClick={onClick}
            className={`tr-group-tab flex items-center gap-1.5 px-3 py-1.5 text-xs font-semibold select-none cursor-pointer flex-shrink-0 ${active ? 'tr-group-tab--active' : ''}`}
        >
            {colorHex && (
                <span
                    className="w-2 h-2 rounded-full flex-shrink-0"
                    style={{ backgroundColor: colorHex }}
                />
            )}
            <span className="truncate max-w-[80px]">{label}</span>
            {onEdit && active && groupId !== 'all' && groupId !== null && (
                <button
                    onClick={(e) => {
                        e.stopPropagation();
                        onEdit();
                    }}
                    className="tr-group-tab__edit"
                >
                    <Settings2 className="w-3 h-3" />
                </button>
            )}
        </div>
    );
}

interface SidebarProps {
    folders: TelegramFolder[];
    groups: FolderGroup[];
    activeFolderId: number | null;
    setActiveFolderId: (id: number | null) => void;
    onDrop: (e: React.DragEvent, folderId: number | null) => void;
    onDelete: (id: number, name: string) => void;
    onRename: (id: number, name: string) => void;
    onToggleVisibility: (id: number, name: string, isPublic: boolean) => void;
    onCreate: (name: string) => Promise<void>;
    onJoinChannel: () => void;
    isSyncing: boolean;
    isConnected: boolean;
    onSync: () => void;
    onLogout: () => void;
    bandwidth: BandwidthStats | null;
    onAssignFolderToGroup: (folderId: number, groupId: number | null) => Promise<void>;
    onReorderFolders: (reordered: TelegramFolder[]) => Promise<void>;
    onUpdateGroupOrder: (reorderedGroups: FolderGroup[]) => Promise<void>;
    onCreateGroup: (name: string, colorHex: string) => Promise<void>;
    onUpdateGroup: (groupId: number, name: string, colorHex: string) => Promise<void>;
    onDeleteGroup: (groupId: number) => Promise<void>;
}

export function Sidebar({
    folders, groups = [], activeFolderId, setActiveFolderId, onDrop, onDelete, onRename, onToggleVisibility, onCreate, onJoinChannel,
    isSyncing, isConnected, onSync, onLogout, bandwidth,
    onAssignFolderToGroup, onReorderFolders, onUpdateGroupOrder, onCreateGroup, onUpdateGroup, onDeleteGroup
}: SidebarProps) {
    const [showNewFolderInput, setShowNewFolderInput] = useState(false);
    const [newFolderName, setNewFolderName] = useState("");
    const { t } = useTranslation();
    const { settings, updateSetting } = useSettings();

    // Grouping States
    const [activeGroupId, setActiveGroupId] = useState<number | null | 'all'>('all');
    const [showGroupEditor, setShowGroupEditor] = useState(false);
    const [editingGroup, setEditingGroup] = useState<FolderGroup | null>(null); // null means creating
    const [groupName, setGroupName] = useState("");
    const [groupColor, setGroupColor] = useState("#3B82F6");

    // DND Kit Sensors
    const sensors = useSensors(
        useSensor(PointerSensor, {
            activationConstraint: {
                distance: 8,
            },
        }),
        useSensor(KeyboardSensor, {
            coordinateGetter: sortableKeyboardCoordinates,
        })
    );

    const handleDragEnd = (event: DragEndEvent) => {
        const { active, over } = event;
        if (!over) return;

        const activeId = active.id.toString();
        const overId = over.id.toString();

        if (activeId.startsWith('folder-')) {
            const activeFolderId = parseInt(activeId.replace('folder-', ''), 10);

            if (overId.startsWith('group-tab-')) {
                const overGroupIdStr = overId.replace('group-tab-', '');
                const overGroupId = overGroupIdStr === 'all'
                    ? null
                    : overGroupIdStr === 'unassigned'
                        ? null
                        : parseInt(overGroupIdStr, 10);
                onAssignFolderToGroup(activeFolderId, overGroupId);
            } else if (overId.startsWith('folder-')) {
                const overFolderId = parseInt(overId.replace('folder-', ''), 10);
                if (activeFolderId !== overFolderId) {
                    const oldIndex = folders.findIndex(f => f.id === activeFolderId);
                    const newIndex = folders.findIndex(f => f.id === overFolderId);
                    if (oldIndex !== -1 && newIndex !== -1) {
                        const reordered = arrayMove(folders, oldIndex, newIndex);
                        onReorderFolders(reordered);
                    }
                }
            }
        } else if (activeId.startsWith('group-tab-')) {
            const activeGroupId = parseInt(activeId.replace('group-tab-', ''), 10);

            if (overId.startsWith('group-tab-')) {
                const overGroupIdStr = overId.replace('group-tab-', '');
                if (overGroupIdStr !== 'all' && overGroupIdStr !== 'unassigned') {
                    const overGroupId = parseInt(overGroupIdStr, 10);
                    if (activeGroupId !== overGroupId) {
                        const oldIndex = groups.findIndex(g => g.id === activeGroupId);
                        const newIndex = groups.findIndex(g => g.id === overGroupId);
                        if (oldIndex !== -1 && newIndex !== -1) {
                            const reordered = arrayMove(groups, oldIndex, newIndex);
                            onUpdateGroupOrder(reordered);
                        }
                    }
                }
            }
        }
    };

    const submitCreate = async () => {
        if (!newFolderName.trim()) return;
        try {
            await onCreate(newFolderName);
            setNewFolderName("");
            setShowNewFolderInput(false);
        } catch {
            // handled by parent
        }
    };

    const handleSaveGroup = async () => {
        if (!groupName.trim()) return;
        if (editingGroup) {
            await onUpdateGroup(editingGroup.id, groupName, groupColor);
        } else {
            await onCreateGroup(groupName, groupColor);
        }
        setShowGroupEditor(false);
        setEditingGroup(null);
        setGroupName("");
        setGroupColor("#3B82F6");
    };

    const handleDeleteGroupClick = async (groupId: number) => {
        await onDeleteGroup(groupId);
        if (activeGroupId === groupId) {
            setActiveGroupId('all');
        }
        setShowGroupEditor(false);
        setEditingGroup(null);
        setGroupName("");
        setGroupColor("#3B82F6");
    };

    const filteredFolders = folders.filter(folder => {
        if (settings.hideGroups || activeGroupId === 'all') return true;
        if (activeGroupId === null) return folder.group_id === null || folder.group_id === undefined;
        return folder.group_id === activeGroupId;
    });

    return (
        <aside 
            className={`tr-sidebar transition-all duration-300 ${settings.sidebarCollapsed ? 'w-14' : 'w-64'} flex flex-col`}
            onClick={e => e.stopPropagation()}
        >
            <div className={`tr-sidebar-brand-row p-4 flex ${settings.sidebarCollapsed ? 'flex-col items-center gap-2' : 'items-center justify-between'} min-h-[64px]`}>
                <TeraRelayBrand size="sm" showName={!settings.sidebarCollapsed} />
                <PremiumIconButton
                    label={settings.sidebarCollapsed ? t('common.expand_sidebar') || "Expand Sidebar" : t('common.collapse_sidebar') || "Collapse Sidebar"}
                    onClick={() => updateSetting('sidebarCollapsed', !settings.sidebarCollapsed)}
                >
                    {settings.sidebarCollapsed ? <ChevronRight /> : <ChevronLeft />}
                </PremiumIconButton>
            </div>

            <DndContext
                sensors={sensors}
                collisionDetection={closestCenter}
                onDragEnd={handleDragEnd}
            >
                {!settings.sidebarCollapsed && (
                    <div className="px-3 py-3 border-b border-telegram-border flex flex-col gap-2.5">
                        <div className="flex items-center justify-between">
                            <span className="text-xs font-semibold text-telegram-subtext uppercase tracking-wider flex items-center gap-1.5">
                                {t('common.groups') || "Groups"}
                            </span>
                            <div className="flex items-center gap-1">
                                <PremiumIconButton
                                    label={settings.hideGroups ? t('common.show_groups') || "Show Groups" : t('common.hide_groups') || "Hide Groups"}
                                    onClick={() => updateSetting('hideGroups', !settings.hideGroups)}
                                >
                                    {settings.hideGroups ? <EyeOff /> : <Eye />}
                                </PremiumIconButton>
                                {!settings.hideGroups && (
                                    <PremiumIconButton
                                        label={t('common.create_group') || "Create Group"}
                                        tone="primary"
                                        onClick={() => {
                                            setEditingGroup(null);
                                            setGroupName("");
                                            setGroupColor("#3B82F6");
                                            setShowGroupEditor(true);
                                        }}
                                    >
                                        <Plus />
                                    </PremiumIconButton>
                                )}
                            </div>
                        </div>

                        {!settings.hideGroups && showGroupEditor && (
                            <PremiumSurface tone="soft" className="p-3 flex flex-col gap-3 animate-in fade-in slide-in-from-top-1 duration-150">
                                <div>
                                    <label className="text-[10px] font-semibold text-telegram-subtext uppercase tracking-wider block mb-1">
                                        {editingGroup ? t('common.edit_group_name') : t('common.new_group_name')}
                                    </label>
                                    <input
                                        autoFocus
                                        type="text"
                                        className="w-full bg-white/10 rounded px-2 py-1 text-xs text-telegram-text focus:outline-none focus:ring-1 focus:ring-telegram-primary"
                                        placeholder={t('common.enter_group_name')}
                                        value={groupName}
                                        onChange={e => setGroupName(e.target.value)}
                                    />
                                </div>

                                <div>
                                    <label className="text-[10px] font-semibold text-telegram-subtext uppercase tracking-wider block mb-1">
                                        {t('common.group_color', { defaultValue: 'Group Color' })}
                                    </label>
                                    <div className="flex flex-wrap gap-1.5">
                                        {PRESET_COLORS.map(color => (
                                            <button
                                                key={color}
                                                onClick={() => setGroupColor(color)}
                                                className={`w-5 h-5 rounded-full border transition-all ${
                                                    groupColor === color
                                                        ? 'border-white scale-110 shadow-md ring-1 ring-telegram-primary'
                                                        : 'border-transparent hover:scale-105'
                                                }`}
                                                style={{ backgroundColor: color }}
                                            />
                                        ))}
                                    </div>
                                </div>

                                <div className="flex gap-2 justify-end mt-1">
                                    {editingGroup && (
                                        <button
                                            onClick={() => handleDeleteGroupClick(editingGroup.id)}
                                            className="mr-auto p-1.5 text-red-500 hover:bg-red-500/10 rounded transition-colors"
                                            title={t('common.delete_group')}
                                        >
                                            <Trash2 className="w-3.5 h-3.5" />
                                        </button>
                                    )}
                                    <button
                                        onClick={() => {
                                            setShowGroupEditor(false);
                                            setEditingGroup(null);
                                        }}
                                        className="px-2 py-1 text-[11px] font-semibold text-telegram-subtext hover:bg-telegram-hover rounded transition-colors flex items-center gap-1"
                                    >
                                        <X className="w-3 h-3" />
                                        {t('common.cancel') || "Cancel"}
                                    </button>
                                    <button
                                        onClick={handleSaveGroup}
                                        disabled={!groupName.trim()}
                                        className="px-2.5 py-1 text-[11px] font-semibold bg-telegram-primary text-white hover:bg-telegram-primary/80 rounded transition-colors flex items-center gap-1 disabled:opacity-50"
                                    >
                                        <Check className="w-3 h-3" />
                                        {t('common.save') || "Save"}
                                    </button>
                                </div>
                            </PremiumSurface>
                        )}

                        {!settings.hideGroups && (
                            <div 
                                className="group-tabs-scroll flex items-center gap-2 overflow-x-auto py-1"
                                style={{ scrollbarWidth: 'none', msOverflowStyle: 'none' }}
                            >
                                <style>{`
                                    .group-tabs-scroll::-webkit-scrollbar {
                                        display: none;
                                    }
                                `}</style>
                                <GroupTab
                                    id="group-tab-all"
                                    groupId="all"
                                    label={t('common.all') || "All"}
                                    active={activeGroupId === 'all'}
                                    onClick={() => setActiveGroupId('all')}
                                    isSortable={false}
                                />
                                <GroupTab
                                    id="group-tab-unassigned"
                                    groupId={null}
                                    label={t('common.unassigned') || "Unassigned"}
                                    active={activeGroupId === null}
                                    onClick={() => setActiveGroupId(null)}
                                    isSortable={false}
                                />
                                <SortableContext
                                    items={groups.map(g => `group-tab-${g.id}`)}
                                    strategy={horizontalListSortingStrategy}
                                >
                                    {groups.map(group => (
                                        <GroupTab
                                            key={group.id}
                                            id={`group-tab-${group.id}`}
                                            groupId={group.id}
                                            label={group.name}
                                            colorHex={group.color_hex}
                                            active={activeGroupId === group.id}
                                            onClick={() => setActiveGroupId(group.id)}
                                            onEdit={() => {
                                                setEditingGroup(group);
                                                setGroupName(group.name);
                                                setGroupColor(group.color_hex || "#3B82F6");
                                                setShowGroupEditor(true);
                                            }}
                                        />
                                    ))}
                                </SortableContext>
                            </div>
                        )}
                    </div>
                )}

                {/* Scrollable folder list */}
                <nav className="flex-1 px-2 py-4 space-y-1 overflow-y-auto min-h-0">
                    <SidebarItem
                        icon={HardDrive}
                        label="Personal Vault"
                        active={activeFolderId === null}
                        onClick={() => setActiveFolderId(null)}
                        onDrop={(e: React.DragEvent) => onDrop(e, null)}
                        folderId={null}
                        collapsed={settings.sidebarCollapsed}
                    />
                    <SortableContext
                        items={filteredFolders.map(folder => `folder-${folder.id}`)}
                        strategy={verticalListSortingStrategy}
                    >
                        {filteredFolders.map(folder => (
                            <SidebarItem
                                key={folder.id}
                                icon={Folder}
                                label={folder.name}
                                active={activeFolderId === folder.id}
                                onClick={() => setActiveFolderId(folder.id)}
                                onDrop={(e: React.DragEvent) => {
                                    if (folder.role === 'member') {
                                        e.preventDefault();
                                        return;
                                    }
                                    onDrop(e, folder.id);
                                }}
                                onDelete={() => onDelete(folder.id, folder.name)}
                                onRename={() => onRename(folder.id, folder.name)}
                                onToggleVisibility={() => onToggleVisibility(folder.id, folder.name, !!(folder.is_public || folder.username))}
                                folderId={folder.id}
                                isPublic={!!(folder.is_public || folder.username)}
                                collapsed={settings.sidebarCollapsed}
                                groups={groups}
                                onAssignFolderToGroup={onAssignFolderToGroup}
                                role={folder.role}
                            />
                        ))}
                    </SortableContext>
                </nav>
            </DndContext>

            {/* Sticky channel actions — always visible above the footer */}
            {!settings.sidebarCollapsed && (
                <div className="px-2 pb-2 border-b border-telegram-border">
                    {showNewFolderInput ? (
                        <PremiumSurface tone="soft" className="p-3">
                            <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-telegram-subtext">
                                New channel
                            </div>
                            <div className="flex items-center gap-2">
                                <input
                                    autoFocus
                                    type="text"
                                    className="min-w-0 flex-1 rounded-lg border border-telegram-border bg-telegram-bg px-2.5 py-2 text-sm text-telegram-text outline-none transition focus:border-telegram-primary/60"
                                    placeholder={t('common.channel_name_placeholder', { defaultValue: 'Channel name' })}
                                    value={newFolderName}
                                    onChange={e => setNewFolderName(e.target.value)}
                                    onKeyDown={e => {
                                        if (e.key === 'Enter') {
                                            e.preventDefault();
                                            submitCreate();
                                        } else if (e.key === 'Escape') {
                                            e.preventDefault();
                                            setNewFolderName("");
                                            setShowNewFolderInput(false);
                                        }
                                    }}
                                />
                                <button
                                    type="button"
                                    onClick={() => {
                                        setNewFolderName("");
                                        setShowNewFolderInput(false);
                                    }}
                                    className="grid h-9 w-9 shrink-0 place-items-center rounded-lg text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                                    aria-label="Cancel channel creation"
                                    title="Cancel"
                                >
                                    <X className="h-4 w-4" />
                                </button>
                                <button
                                    type="button"
                                    onClick={submitCreate}
                                    disabled={!newFolderName.trim()}
                                    className="grid h-9 w-9 shrink-0 place-items-center rounded-lg bg-telegram-primary text-white transition hover:brightness-110 disabled:cursor-not-allowed disabled:opacity-40"
                                    aria-label="Create channel"
                                    title="Create channel"
                                >
                                    <Check className="h-4 w-4" />
                                </button>
                            </div>
                        </PremiumSurface>
                    ) : (
                        <div className="space-y-1.5">
                            <PremiumButton
                                variant="secondary"
                                className="w-full justify-start"
                                icon={<Plus />}
                                onClick={() => setShowNewFolderInput(true)}
                            >
                                {t('common.create_channel', { defaultValue: 'Create Channel' })}
                            </PremiumButton>
                            <PremiumButton
                                variant="ghost"
                                className="w-full justify-start"
                                icon={<UserPlus />}
                                onClick={onJoinChannel}
                            >
                                Join TeraRelay Channel
                            </PremiumButton>
                        </div>
                    )}
                </div>
            )}

            <div className={`tr-sidebar-footer p-3 border-t flex flex-col ${settings.sidebarCollapsed ? 'items-center gap-3' : 'gap-3'}`}>
                {settings.sidebarCollapsed ? (
                    <>
                        <PremiumBadge
                            tone={isConnected ? 'success' : 'danger'}
                            dot
                            className="w-7 justify-center px-0"
                            title={isConnected ? t('common.connected_telegram') : t('common.disconnected_telegram')}
                        />
                        <PremiumIconButton
                            label={isSyncing ? t('common.syncing') : t('common.sync')}
                            tone="primary"
                            onClick={onSync}
                            disabled={isSyncing}
                        >
                            <RefreshCw className={isSyncing ? 'animate-spin' : ''} />
                        </PremiumIconButton>
                        <PremiumIconButton
                            label={t('common.logout')}
                            tone="danger"
                            onClick={onLogout}
                        >
                            <LogOut />
                        </PremiumIconButton>
                    </>
                ) : (
                    <>
                        <PremiumBadge tone={isConnected ? 'success' : 'danger'} dot className="self-start">
                            {isConnected ? t('common.connected_telegram') : t('common.disconnected_telegram')}
                        </PremiumBadge>

                        <div className="flex gap-2">
                            <PremiumButton
                                variant="secondary"
                                size="sm"
                                className="flex-1"
                                icon={<RefreshCw className={isSyncing ? 'animate-spin' : ''} />}
                                onClick={onSync}
                                disabled={isSyncing}
                                title="Scan for existing folders"
                            >
                                {isSyncing ? t('common.syncing') : t('common.sync')}
                            </PremiumButton>
                            <PremiumButton
                                variant="danger"
                                size="sm"
                                className="flex-1"
                                icon={<LogOut />}
                                onClick={onLogout}
                                title="Sign Out"
                            >
                                {t('common.logout')}
                            </PremiumButton>
                        </div>

                        {bandwidth && <BandwidthWidget bandwidth={bandwidth} />}
                    </>
                )}
            </div>
        </aside>
    );
}
