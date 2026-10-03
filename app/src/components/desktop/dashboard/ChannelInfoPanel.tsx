import { useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useQuery } from '@tanstack/react-query';
import {
    ChevronDown,
    FileAudio,
    FileImage,
    FileText,
    Files,
    FileVideo,
    UserPlus,
    Users,
    X,
} from 'lucide-react';
import { TelegramFile } from '../../../types';
import { FileCategory, getFileCategory } from '../../../utils';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';

interface TeraChannelMember {
    display_name: string;
    username?: string | null;
}

interface ChannelInfoPanelProps {
    open: boolean;
    channelId: number;
    channelName: string;
    role: 'owner' | 'member';
    files: TelegramFile[];
    activeCategory: FileCategory;
    onSelectCategory: (category: FileCategory) => void;
    onInvite?: () => void;
    onClose: () => void;
}

const categoryRows: Array<{
    key: Exclude<FileCategory, 'all'>;
    label: string;
    icon: typeof FileVideo;
}> = [
    { key: 'video', label: 'Videos', icon: FileVideo },
    { key: 'image', label: 'Images', icon: FileImage },
    { key: 'document', label: 'Documents', icon: FileText },
    { key: 'audio', label: 'Audio', icon: FileAudio },
    { key: 'other', label: 'Other', icon: Files },
];

export function ChannelInfoPanel({
    open,
    channelId,
    channelName,
    role,
    files,
    activeCategory,
    onSelectCategory,
    onInvite,
    onClose,
}: ChannelInfoPanelProps) {
    const [showAllMembers, setShowAllMembers] = useState(false);
    useEscapeToClose(open, onClose);

    useEffect(() => {
        if (open) setShowAllMembers(false);
    }, [open, channelId]);

    const membersQuery = useQuery({
        queryKey: ['channel-members', channelId],
        queryFn: () => invoke<TeraChannelMember[]>('cmd_get_tera_channel_members', { folderId: channelId }),
        enabled: open,
        staleTime: 60_000,
        retry: 1,
    });

    const counts = useMemo(() => {
        const result: Record<Exclude<FileCategory, 'all'>, number> = {
            video: 0,
            image: 0,
            document: 0,
            audio: 0,
            other: 0,
        };
        for (const file of files) {
            result[getFileCategory(file.name)] += 1;
        }
        return result;
    }, [files]);

    if (!open) return null;

    const members = membersQuery.data ?? [];
    const visibleMembers = showAllMembers ? members : members.slice(0, 4);
    const canExpandMembers = members.length > 4;

    const selectCategory = (category: FileCategory) => {
        onSelectCategory(category);
        onClose();
    };

    return (
        <div className="fixed inset-0 z-[230] flex justify-end bg-black/35" onClick={onClose}>
            <aside
                className="h-full w-full max-w-[390px] overflow-y-auto border-l border-telegram-border bg-telegram-surface shadow-2xl"
                onClick={(event) => event.stopPropagation()}
                aria-label="Channel information"
            >
                <div className="sticky top-0 z-10 flex items-center justify-between border-b border-telegram-border bg-telegram-surface/95 px-5 py-4 backdrop-blur">
                    <div className="min-w-0">
                        <h2 className="truncate text-base font-semibold text-telegram-text">{channelName}</h2>
                        <p className="mt-0.5 text-xs font-medium text-telegram-subtext">
                            {role === 'owner' ? 'Owner' : 'Member'}
                        </p>
                    </div>
                    <button
                        type="button"
                        onClick={onClose}
                        className="rounded-lg p-2 text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                        aria-label="Close channel info"
                    >
                        <X className="h-4 w-4" />
                    </button>
                </div>

                <div className="space-y-6 p-5">
                    <section>
                        <div className="mb-3 flex items-center gap-2">
                            <Users className="h-4 w-4 text-telegram-subtext" />
                            <h3 className="text-sm font-semibold text-telegram-text">
                                Members{membersQuery.isSuccess ? ' · ' + members.length : ''}
                            </h3>
                        </div>

                        {membersQuery.isLoading ? (
                            <div className="space-y-2">
                                {[0, 1, 2].map((key) => (
                                    <div key={key} className="h-11 animate-pulse rounded-xl bg-telegram-hover/60" />
                                ))}
                            </div>
                        ) : membersQuery.isError ? (
                            <div className="rounded-xl border border-telegram-border bg-telegram-bg/50 px-3.5 py-3 text-xs leading-relaxed text-telegram-subtext">
                                Member list isn’t available for this account.
                            </div>
                        ) : members.length === 0 ? (
                            <div className="rounded-xl border border-telegram-border bg-telegram-bg/50 px-3.5 py-3 text-xs text-telegram-subtext">
                                No members found.
                            </div>
                        ) : (
                            <div className="space-y-1">
                                {visibleMembers.map((member, index) => (
                                    <div
                                        key={(member.username ?? member.display_name) + '-' + index}
                                        className="flex min-w-0 items-center gap-3 rounded-xl px-2 py-2"
                                    >
                                        <div className="grid h-9 w-9 shrink-0 place-items-center rounded-full bg-telegram-primary/10 text-sm font-semibold text-telegram-primary">
                                            {member.display_name.trim().charAt(0).toUpperCase() || '?'}
                                        </div>
                                        <div className="min-w-0">
                                            <div className="truncate text-sm font-medium text-telegram-text">
                                                {member.display_name}
                                            </div>
                                            {member.username && (
                                                <div className="truncate text-xs text-telegram-subtext">
                                                    @{member.username}
                                                </div>
                                            )}
                                        </div>
                                    </div>
                                ))}

                                {canExpandMembers && (
                                    <button
                                        type="button"
                                        onClick={() => setShowAllMembers((value) => !value)}
                                        className="mt-1 flex w-full items-center justify-center gap-1.5 rounded-xl py-2 text-xs font-medium text-telegram-primary transition hover:bg-telegram-hover"
                                    >
                                        {showAllMembers ? 'Show less' : 'Show more · ' + (members.length - 4)}
                                        <ChevronDown className={'h-3.5 w-3.5 transition-transform ' + (showAllMembers ? 'rotate-180' : '')} />
                                    </button>
                                )}
                            </div>
                        )}
                    </section>

                    <section>
                        <button
                            type="button"
                            onClick={() => selectCategory('all')}
                            className={'mb-2 flex w-full items-center justify-between rounded-xl px-3 py-2.5 text-left transition hover:bg-telegram-hover ' + (activeCategory === 'all' ? 'bg-telegram-primary/10' : '')}
                        >
                            <span className="flex items-center gap-2 text-sm font-semibold text-telegram-text">
                                <Files className="h-4 w-4 text-telegram-subtext" />
                                Files
                            </span>
                            <span className="text-xs font-medium text-telegram-subtext">{files.length}</span>
                        </button>

                        <div className="space-y-1">
                            {categoryRows.map(({ key, label, icon: Icon }) => (
                                <button
                                    key={key}
                                    type="button"
                                    onClick={() => selectCategory(key)}
                                    className={'flex w-full items-center justify-between rounded-xl px-3 py-2.5 text-left transition hover:bg-telegram-hover ' + (activeCategory === key ? 'bg-telegram-primary/10' : '')}
                                >
                                    <span className="flex items-center gap-2 text-sm text-telegram-text">
                                        <Icon className="h-4 w-4 text-telegram-subtext" />
                                        {label}
                                    </span>
                                    <span className="text-xs font-medium text-telegram-subtext">{counts[key]}</span>
                                </button>
                            ))}
                        </div>
                    </section>

                    {role === 'owner' && onInvite && (
                        <section className="border-t border-telegram-border pt-4">
                            <button
                                type="button"
                                onClick={onInvite}
                                className="flex w-full items-center justify-center gap-2 rounded-xl bg-telegram-primary py-2.5 text-sm font-semibold text-white transition hover:brightness-110"
                            >
                                <UserPlus className="h-4 w-4" />
                                Invite member
                            </button>
                        </section>
                    )}
                </div>
            </aside>
        </div>
    );
}
