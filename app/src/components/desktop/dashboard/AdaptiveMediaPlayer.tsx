import { useEffect, useRef, useState, useCallback, useMemo } from 'react';
import { X, AlertTriangle, Loader2, RefreshCw, StopCircle, Maximize2, Minimize2, Volume2, VolumeX, Volume1, Play, Pause, RotateCcw, RotateCw, AudioLines, Captions, Monitor, Download, Activity, Trash2, Zap, ExternalLink, ChevronDown } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { toast } from 'sonner';
import Hls from 'hls.js';
import { TelegramFile, StreamingQuality, TranscodePrepareResult, TranscodeJobPhase, TranscodeCapabilities, MediaTrackInfo, MediaTrackProbe, QUALITY_LABELS, HLS_QUALITIES } from '../../../types';
import { useAdaptiveStreaming } from '../../../hooks/useAdaptiveStreaming';

import { authorizeHlsChildRequestUrl, buildAuthenticatedHlsUrl, buildAuthenticatedRemuxUrl, buildAuthenticatedSubtitleUrl, chooseCompatibilityQuality, describePlaybackError, eligibleNativeMemoryMp4, fetchBoundedNativeMp4, nativeRemuxRevisionUrl, nativeProgressiveRevisionUrl, planPlaybackSeek, restoreCompatibilityPlayback, shouldDeferCompletedRemuxSeek, shouldRemuxAfterNativeDecoderFailure, shouldResumeAfterRemuxFinish, shiftSubtitleCues } from './playbackTimeline';

interface AdaptiveMediaPlayerProps {
    file: TelegramFile;
    activeFolderId: number | null;
    onClose: () => void;
    onNext?: () => void;
    onPrev?: () => void;
    onDownload?: () => void;
    currentIndex?: number;
    totalItems?: number;
    streamUrl: string;
    preferRemux?: boolean;
}

interface ExternalVideoPlayer {
    id: string;
    name: string;
}

// ── HLS Quality State ────────────────────────────────────────────────

const STREAM_BASE_KEY = '/stream/';

function formatPlayerClock(seconds: number): string {
    if (!Number.isFinite(seconds) || seconds < 0) return '00:00';
    const rounded = Math.floor(seconds);
    const hours = Math.floor(rounded / 3600);
    const minutes = Math.floor((rounded % 3600) / 60).toString().padStart(2, '0');
    const secs = (rounded % 60).toString().padStart(2, '0');
    return hours > 0 ? `${hours}:${minutes}:${secs}` : `${minutes}:${secs}`;
}

function mediaTrackLabel(track: MediaTrackInfo, ordinal: number, kind: 'Audio' | 'Subtitle'): string {
    const name = track.title?.trim() || track.language?.toUpperCase() || `${kind} ${ordinal}`;
    const details = [
        track.codec && track.codec !== 'unknown' ? track.codec.toUpperCase() : null,
        track.channels ? `${track.channels}ch` : null,
    ].filter(Boolean);
    return details.length > 0 ? `${name} · ${details.join(' · ')}` : name;
}

export function AdaptiveMediaPlayer({
    file,
    activeFolderId,
    onClose,
    onNext,
    onPrev,
    onDownload,
    currentIndex,
    totalItems,
    streamUrl,
    preferRemux = false,
}: AdaptiveMediaPlayerProps) {
    // ── Restart counter for MSE pipeline reinit ──────────────────────
    // Must be declared BEFORE restartStreamUrl useMemo
    const [restartNonce, setRestartNonce] = useState(0);
    // A native decoder-only retry must not reinitialize the MSE/remux hook.
    const [nativeDecoderRevision, setNativeDecoderRevision] = useState(0);
    const [memoryMedia, setMemoryMedia] = useState<{ source: string; url: string } | null>(null);
    // Source-identity gate prevents old private Blob bytes from being mounted
    // under a newly selected file before its cleanup effect has executed.
    const memoryMediaUrl = memoryMedia?.source === streamUrl ? memoryMedia.url : null;
    const [memoryLoading, setMemoryLoading] = useState(false);
    const memoryMediaUrlRef = useRef<string | null>(null);
    const memoryFetchRef = useRef<AbortController | null>(null);
    const memoryGenerationRef = useRef(0);
    const clearMemoryMedia = useCallback(() => {
        memoryGenerationRef.current += 1;
        memoryFetchRef.current?.abort();
        memoryFetchRef.current = null;
        if (memoryMediaUrlRef.current) URL.revokeObjectURL(memoryMediaUrlRef.current);
        memoryMediaUrlRef.current = null;
        setMemoryMedia(null);
        setMemoryLoading(false);
    }, []);

    // ── fMP4 remux state ────────────────────────────────────────────
    const [fmp4Remuxing, setFmp4Remuxing] = useState(false);
    const [fmp4RemuxError, setFmp4RemuxError] = useState<string | null>(null);
    const [fmp4StreamUrl, setFmp4StreamUrl] = useState<string | null>(null);
    const fmp4RemuxingRef = useRef(false);
    // Generation counter bumped on source change so stale async IIFEs
    // from a previous file don't set state on the current file.
    const remuxGenerationRef = useRef(0);
    const remuxWindowRef = useRef({ start: 0, complete: false, key: null as string | null });
    const pendingSeekRef = useRef<{ time: number; play: boolean } | null>(null);
    // User intent is not equivalent to HTMLVideoElement.paused: an incomplete
    // growing stream can pause spontaneously before reporting ended=true.
    const nativeUserPauseRef = useRef(false);
    const nativeOriginalRetryRef = useRef(0);
    const nativeCompletedRetryRef = useRef(0);
    const sourceDurationRef = useRef<number | null>(null);
    const [movieTime, setMovieTime] = useState(0);
    const [nativePaused, setNativePaused] = useState(false);

    // ── Effective stream URL: use fMP4 URL when available ────────────
    const effectiveStreamUrl = fmp4StreamUrl || streamUrl;

    // ── Original MSE streaming (for "original" quality) ──────────────
    // Changing restartNonce forces the hook to reinitialize by altering the streamUrl
    const restartStreamUrl = useMemo(() => {
        if (restartNonce > 0) {
            if (fmp4StreamUrl) {
                return nativeRemuxRevisionUrl(effectiveStreamUrl, restartNonce);
            }
            const sep = effectiveStreamUrl.includes('?') ? '&' : '?';
            return `${effectiveStreamUrl}${sep}_r=${restartNonce}`;
        }
        return effectiveStreamUrl;
    }, [effectiveStreamUrl, fmp4StreamUrl, restartNonce]);

    // ── Refs for late-bound values needed by progressive handler ─────
    const abortMseRef = useRef<(() => void) | null>(null);
    const logRef = useRef<((msg: string, ...args: unknown[]) => void) | null>(null);
    const transcodeCapsRef = useRef<TranscodeCapabilities | null>(null);
    const streamSessionRef = useRef<{ id: string; key: string; ready: Promise<void> } | null>(null);

    // ── Handle progressive MP4 detection — trigger fMP4 remux ──────
    const handleProgressiveDetected = useCallback((requestedTime?: number) => {
        if (fmp4RemuxingRef.current) return;
        if (!transcodeCapsRef.current?.available) return;
        clearMemoryMedia();
        fmp4RemuxingRef.current = true;
        // A different audio selection/seek window has its own bounded
        // decoder recovery allowance; do not inherit a previous window's
        // failed parser state.
        nativeCompletedRetryRef.current = 0;
        setFmp4Remuxing(true);
        setFmp4RemuxError(null);
        const gen = remuxGenerationRef.current;
        (async () => {
            try {
                abortMseRef.current?.();
                const video = fallbackVideoRef.current;
                const position = requestedTime ?? (video ? remuxWindowRef.current.start + video.currentTime : 0);
                const play = pendingSeekRef.current?.play ?? (video && (nativePlaybackStartedRef.current || video.currentTime > 0)
                    ? !video.paused : true);
                savedTimeRef.current = position;
                pendingSeekRef.current = { time: position, play };
                setMovieTime(position);
                setNativeAutoPlay(false);
                if (video) {
                    video.pause();
                    video.removeAttribute('src');
                    video.load();
                }
                const previousKey = remuxWindowRef.current.key;
                if (previousKey) await invoke('cmd_cancel_fmp4_stream', { fileKey: previousKey, sessionId: streamSessionRef.current?.id });
                const session = streamSessionRef.current;
                if (!session || session.key !== `${activeFolderId ?? 0}_${file.id}`) {
                    throw new Error('Player session closed before remux preparation.');
                }
                await session.ready;
                if (gen !== remuxGenerationRef.current || streamSessionRef.current !== session) return;
                const result = await invoke<{
                    url: string; output_file_key: string; status: string; window_start_secs?: number;
                }>('cmd_prepare_fmp4_stream', {
                    messageId: file.id,
                    folderId: activeFolderId,
                    audioStreamIndex: selectedAudioStreamRef.current,
                    startTimeSecs: position,
                    sessionId: session.id,
                });
                if (gen !== remuxGenerationRef.current) {
                    invoke('cmd_cancel_fmp4_stream', { fileKey: result.output_file_key, sessionId: session.id }).catch(() => {});
                    return;
                }
                remuxWindowRef.current = {
                    start: result.window_start_secs ?? 0,
                    complete: result.status === 'ready',
                    key: result.output_file_key,
                };
                const fullUrl = buildAuthenticatedRemuxUrl(
                    streamBaseRef.current, result.url, streamAuthRef.current,
                );
                if (!fullUrl) throw new Error('Invalid local fMP4 remux URL');
                if (result.status === 'ready') {
                    setNativeDecoderRevision(0);
                    setFmp4StreamUrl(fullUrl);
                    setFmp4Remuxing(false);
                    fmp4RemuxingRef.current = false;
                    setRestartNonce(n => n + 1);
                    return;
                }
                let playingPartial = false;
                for (let i = 0; playingPartial || i < 600; i++) {
                    await new Promise(r => setTimeout(r, 1000));
                    if (gen !== remuxGenerationRef.current) return;
                    const status = await invoke<{ status: string; error: string | null; complete?: boolean }>(
                        'cmd_get_fmp4_status', { fileKey: result.output_file_key },
                    );
                    if (gen !== remuxGenerationRef.current) return;
                    if (status.status === 'error') throw new Error(status.error || 'fMP4 remux failed');
                    if (status.status !== 'ready') continue;
                    const wasPlayingPartial = playingPartial;
                    if (!playingPartial) {
                        setNativeDecoderRevision(0);
                        setFmp4StreamUrl(fullUrl);
                        setFmp4Remuxing(false);
                        setRestartNonce(n => n + 1);
                        playingPartial = true;
                    }
                    if (status.complete !== false) {
                        remuxWindowRef.current.complete = true;
                        if (wasPlayingPartial) {
                            const activeVideo = fallbackVideoRef.current;
                            const time = pendingSeekRef.current?.time ?? (remuxWindowRef.current.start + (activeVideo?.currentTime ?? 0));
                            const resume = shouldResumeAfterRemuxFinish(
                                pendingSeekRef.current, activeVideo,
                                nativePlaybackStartedRef.current && !nativeUserPauseRef.current,
                            );
                            savedTimeRef.current = time;
                            pendingSeekRef.current = { time, play: resume };
                            // The finished HTTP-range stream can initially
                            // report zero duration/empty seekable ranges in
                            // WebKit. If autoplay stays disabled while the
                            // pending seek waits for media ranges, neither
                            // operation ever advances. Start the new stream
                            // only when playback was requested; the pending
                            // seek still restores the absolute position once
                            // the decoder reports valid ranges.
                            setNativeAutoPlay(resume);
                            // Explicitly detach the *growing* media pipeline
                            // before mounting the completed HTTP-range source.
                            // A keyed React replacement alone may leave
                            // WebKit/GStreamer consuming the old live stream.
                            // The seek/play state above survives this teardown.
                            if (activeVideo) {
                                activeVideo.pause();
                                activeVideo.removeAttribute('src');
                                activeVideo.load();
                            }
                            setRestartNonce(n => n + 1);
                        }
                        fmp4RemuxingRef.current = false;
                        return;
                    }
                }
                throw new Error('fMP4 remux timed out after 10 minutes');
            } catch (error) {
                if (gen !== remuxGenerationRef.current) return;
                setFmp4RemuxError(describePlaybackError(error));
                setFmp4Remuxing(false);
                fmp4RemuxingRef.current = false;
            }
        })();
    }, [file.id, activeFolderId, clearMemoryMedia]);

    // ── progressive callback for useAdaptiveStreaming ────────────────
    const progressiveCallback = useMemo(
        () => handleProgressiveDetected,
        [handleProgressiveDetected],
    );

    // Native HTTP ranges let the decoder fetch and seek without a JavaScript
    // downloader that can throttle original media below its playback bitrate.
    const {
        videoRef: mseVideoRef,
        phase: msePhase,
        error: mseError,
        tracks,
        loadProgress,
        setQuality,
        adaptiveMode,
        setAdaptiveMode,
        measuredKbps,
        useFallback,
        fallbackUrl,
        abort: abortMse,
    } = useAdaptiveStreaming(restartStreamUrl, file.name, progressiveCallback, true);

    // A native decoder retry changes ONLY the <video> URL and React key,
    // never useAdaptiveStreaming's pipeline URL. fMP4 aliases must be
    // derived from the canonical key: stacking '__refresh' suffixes would
    // defeat the server's single bounded alias-to-file resolution.
    const nativeMediaUrl = useMemo(() => {
        if (memoryMediaUrl && !fmp4StreamUrl) return memoryMediaUrl;
        if (nativeDecoderRevision === 0) return fallbackUrl;
        if (fmp4StreamUrl) {
            return nativeRemuxRevisionUrl(fmp4StreamUrl, restartNonce + nativeDecoderRevision);
        }
        return nativeProgressiveRevisionUrl(fallbackUrl, nativeDecoderRevision);
    }, [fallbackUrl, fmp4StreamUrl, memoryMediaUrl, restartNonce, nativeDecoderRevision]);

    // ── HLS transcode state ──────────────────────────────────────────
    // playbackMode is the single source of truth: 'original' or 'hls'
    const [playbackMode, setPlaybackMode] = useState<'original' | 'hls'>('original');
    useEffect(() => {
        if (playbackMode === 'hls') clearMemoryMedia();
    }, [playbackMode, clearMemoryMedia]);
    const [hlsQuality, setHlsQuality] = useState<StreamingQuality | null>(null);
    const [hlsPhase, setHlsPhase] = useState<TranscodeJobPhase>('idle');
    const [hlsProgress, setHlsProgress] = useState(0);
    const [hlsError, setHlsError] = useState<string | null>(null);
    const [hlsPlaylistUrl, setHlsPlaylistUrl] = useState<string | null>(null);
    const [transcodeCapabilities, setTranscodeCapabilities] = useState<TranscodeCapabilities | null>(null);
    const [hlsVariantStates, setHlsVariantStates] = useState<Record<string, TranscodeJobPhase>>({});

    const hlsRef = useRef<Hls | null>(null);
    const hlsVideoRef = useRef<HTMLVideoElement>(null);
    const fallbackVideoRef = useRef<HTMLVideoElement>(null);
    const nativePlaybackStartedRef = useRef(false);
    const [nativePhase, setNativePhase] = useState<'loading' | 'ready' | 'playing' | 'ended' | 'error'>('loading');
    const [nativeError, setNativeError] = useState<string | null>(null);
    const [nativeAutoPlay, setNativeAutoPlay] = useState(true);

    useEffect(() => {
        nativePlaybackStartedRef.current = false;
        setNativePhase('loading');
        setNativeError(null);
    }, [nativeMediaUrl]);

    // Decoder errors may arrive before the asynchronous FFmpeg capability
    // check. Retry once capabilities arrive instead of losing that fallback.
    useEffect(() => {
        if (nativePhase === 'error' && transcodeCapabilities?.available
            && !fmp4StreamUrl && !fmp4RemuxError && !fmp4RemuxingRef.current
            // Opted-in private-memory playback must not silently convert
            // an ordinary MP4 if its decoder reports another error.
            && !memoryMediaUrl && !memoryLoading
            // Ordinary MP4 is opt-in after native recovery fails. An
            // explicit remux preference already authorizes the fallback.
            && shouldRemuxAfterNativeDecoderFailure(preferRemux, nativeOriginalRetryRef.current)) {
            handleProgressiveDetected();
        }
    }, [nativePhase, transcodeCapabilities, preferRemux,
        fmp4StreamUrl, fmp4RemuxError, memoryMediaUrl, memoryLoading, handleProgressiveDetected]);
    const [mediaTracks, setMediaTracks] = useState<MediaTrackProbe>({ audio_tracks: [], subtitle_tracks: [] });
    const sourceDuration = mediaTracks.duration_secs != null && Number.isFinite(mediaTracks.duration_secs) && mediaTracks.duration_secs > 0
        ? mediaTracks.duration_secs : null;
    sourceDurationRef.current = sourceDuration;
    const [selectedAudioStream, setSelectedAudioStream] = useState<number | null>(null);
    const selectedAudioStreamRef = useRef<number | null>(null);
    const [selectedSubtitleStream, setSelectedSubtitleStream] = useState<number | null>(null);
    const [subtitleUrl, setSubtitleUrl] = useState<string | null>(null);
    const [subtitleLoading, setSubtitleLoading] = useState(false);
    const showSubtitleTrack = useCallback((element: HTMLTrackElement | null) => {
        if (element) element.track.mode = 'showing';
    }, []);
    // File changes and rapid track switches must invalidate late subtitle IPC.
    const subtitleSelectionGenerationRef = useRef(0);
    const pollTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
    const savedTimeRef = useRef<number>(0);
    const savedPlaybackRef = useRef(true);
    const hlsQualityRef = useRef<StreamingQuality | null>(null);
    // Late FFmpeg preparation or polling replies must not resurrect an HLS
    // playlist after a file/quality switch or a player unmount.
    const hlsGenerationRef = useRef(0);
    // The Rust transcoder reuses job IDs by folder/file/quality. A stale
    // response must not cancel an identical job requested by a newer intent.
    const hlsDesiredJobRef = useRef<string | null>(null);
    // A live HLS EVENT playlist is playable before full conversion. Keep
    // polling until ENDLIST is durable; never cache it as completed early.
    const liveHlsRef = useRef(false);
    const streamAuthRef = useRef<string>('');
    const currentJobIdRef = useRef<string | null>(null);
    const streamBaseRef = useRef<string>('');
    const [externalVideoPlayers, setExternalVideoPlayers] = useState<ExternalVideoPlayer[]>([]);
    const [externalPlayerMenuOpen, setExternalPlayerMenuOpen] = useState(false);
    const [openingExternalPlayer, setOpeningExternalPlayer] = useState<string | null>(null);
    const containerRef = useRef<HTMLDivElement>(null);
    const [isFullscreen, setIsFullscreen] = useState(false);
    const isFullscreenRef = useRef(false);
    const [controlsVisible, setControlsVisible] = useState(true);
    const controlsIdleTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const [playerDuration, setPlayerDuration] = useState(0);
    // Delay expensive native remux seeking until a timeline drag is released.
    const [scrubTime, setScrubTime] = useState<number | null>(null);
    const [playbackRate, setPlaybackRate] = useState(1);
    const revealControls = useCallback(() => {
        setControlsVisible(true);
        if (controlsIdleTimerRef.current !== null) clearTimeout(controlsIdleTimerRef.current);
        if (isFullscreen) {
            controlsIdleTimerRef.current = setTimeout(() => setControlsVisible(false), 2700);
        }
    }, [isFullscreen]);
    useEffect(() => {
        if (!isFullscreen) {
            setControlsVisible(true);
            if (controlsIdleTimerRef.current !== null) clearTimeout(controlsIdleTimerRef.current);
        } else {
            revealControls();
        }
        return () => {
            if (controlsIdleTimerRef.current !== null) clearTimeout(controlsIdleTimerRef.current);
        };
    }, [isFullscreen, revealControls]);
    // HLS video ready flag for retry-safe attach
    const [hlsVideoReady, setHlsVideoReady] = useState(false);
    // Callback ref: fires when HLS video element mounts
    const hlsVideoCallbackRef = useCallback((el: HTMLVideoElement | null) => {
        (hlsVideoRef as React.MutableRefObject<HTMLVideoElement | null>).current = el;
        setHlsVideoReady(!!el);
    }, []);

    // ── Logging helpers ─────────────────────────────────────────────
    const log = useCallback((_msg: string, ..._args: unknown[]) => {}, []);
    // Wire late-bound refs for the progressive handler
    logRef.current = log;

    // ── Volume state ─────────────────────────────────────────────────
    const [volume, setVolume] = useState(1);
    const [isMuted, setIsMuted] = useState(false);
    const volumeBeforeMute = useRef(1);

    // ── Source / playing resolution tracking ──────────────────────────
    const [sourceResolution, setSourceResolution] = useState<{ w: number; h: number } | null>(null);
    const [playingResolution, setPlayingResolution] = useState<{ w: number; h: number } | null>(null);

    // ── Source height for upscale prevention ──────────────────────────
    const sourceHeight = useMemo(() => {
        // From MSE tracks (mp4box metadata)
        const videoTrack = tracks.find(t => t.type === 'video');
        if (videoTrack?.height) return videoTrack.height;
        // From resolution badge (HLS metadata or video element)
        if (sourceResolution?.h) return sourceResolution.h;
        return null;
    }, [tracks, sourceResolution]);

    // ── Debug overlay ───────────────────────────────────────────────
    const [debugOverlay, setDebugOverlay] = useState(() => {
        try { return localStorage.getItem('debug_overlay') === '1'; } catch { return false; }
    });
    const [debugBufferedSecs, setDebugBufferedSecs] = useState(0);

    const toggleDebugOverlay = useCallback(() => {
        setDebugOverlay(prev => {
            const next = !prev;
            try { localStorage.setItem('debug_overlay', next ? '1' : '0'); } catch {}
            return next;
        });
    }, []);

    // Clear transcode cache for current file
    const [clearingCache, setClearingCache] = useState(false);
    const fileKey = `${activeFolderId ?? 0}_${file.id}`;
    useEffect(() => {
        // Distinct player mounts must never share an identity, including React
        // StrictMode's effect cleanup/restart cycle.
        const random = new Uint8Array(16);
        crypto.getRandomValues(random);
        const id = Array.from(random, byte => byte.toString(16).padStart(2, '0')).join('');
        const ready = invoke<void>('cmd_begin_stream_session', { fileKey, sessionId: id });
        const session = { id, key: fileKey, ready };
        streamSessionRef.current = session;
        return () => {
            if (streamSessionRef.current === session) streamSessionRef.current = null;
            // Release only after registration completes. If the player closed
            // during preparation, the Rust session gate rejects the late job.
            void ready.then(
                () => invoke<void>('cmd_end_stream_session', { fileKey, sessionId: id }),
                () => undefined,
            ).catch(error => {
                console.warn('TeraRelay could not discard its temporary streaming cache:', error);
            });
        };
    }, [fileKey]);

    const handleClearTranscodeCache = useCallback(async () => {
        setClearingCache(true);
        try {
            const msg = await invoke<string>('cmd_clear_transcode_cache', { fileKey });
            toast.success(msg);
            log('cleared transcode cache for', fileKey);
        } catch (e) {
            toast.error(`Failed to clear cache: ${e}`);
        } finally {
            setClearingCache(false);
        }
    }, [fileKey, log]);

    // Poll buffered seconds
    useEffect(() => {
        if (!debugOverlay) return;
        const interval = setInterval(() => {
            const video = (playbackMode === 'hls' ? hlsVideoRef.current : mseVideoRef.current);
            if (video && video.buffered.length > 0) {
                setDebugBufferedSecs(video.buffered.end(video.buffered.length - 1) - video.currentTime);
            }
        }, 1000);
        return () => clearInterval(interval);
    }, [debugOverlay, playbackMode]);

    // Track resolution from MSE track info
    useEffect(() => {
        const videoTrack = tracks.find(t => t.type === 'video');
        if (videoTrack?.width && videoTrack?.height && !sourceResolution) {
            setSourceResolution({ w: videoTrack.width, h: videoTrack.height });
        }
    }, [tracks, sourceResolution]);

    // Poll active video element for current playing resolution
    useEffect(() => {
        const interval = setInterval(() => {
            const video = (playbackMode === 'hls' ? hlsVideoRef.current : mseVideoRef.current);
            if (video && video.videoWidth > 0 && video.videoHeight > 0) {
                const pw = video.videoWidth;
                const ph = video.videoHeight;
                setPlayingResolution(prev => {
                    if (!prev || prev.w !== pw || prev.h !== ph) return { w: pw, h: ph };
                    return prev;
                });
            }
        }, 2000);
        return () => clearInterval(interval);
    }, [playbackMode]);

    const getActiveVideo = useCallback((): HTMLVideoElement | null => {
        if (playbackMode === 'hls') return hlsVideoRef.current;
        if (useFallback) return fallbackVideoRef.current;
        return mseVideoRef.current;
    }, [playbackMode, useFallback]);

    // Detect only supported players when this in-app video player opens.
    // The file browser and the separate Drive external-open flow never call
    // these commands or render this control.
    useEffect(() => {
        let active = true;
        invoke<ExternalVideoPlayer[]>('cmd_list_external_video_players')
            .then(players => {
                if (active) setExternalVideoPlayers(Array.isArray(players) ? players : []);
            })
            .catch(error => {
                if (active) {
                    setExternalVideoPlayers([]);
                    console.warn('Could not detect external video players:', error);
                }
            });
        return () => { active = false; };
    }, []);

    useEffect(() => {
        setExternalPlayerMenuOpen(false);
    }, [file.id, activeFolderId]);

    const handleOpenExternalPlayer = useCallback(async (playerId: string) => {
        if (openingExternalPlayer !== null) return;
        const player = externalVideoPlayers.find(candidate => candidate.id === playerId);
        if (!player) return;

        setOpeningExternalPlayer(playerId);
        try {
            await invoke<void>('cmd_open_video_in_external_player', {
                playerId,
                messageId: file.id,
                folderId: activeFolderId,
            });
            // Pause only after the native launcher confirms it started, so a
            // failed launch never interrupts the built-in player.
            getActiveVideo()?.pause();
            setExternalPlayerMenuOpen(false);
            toast.success(`Opening in ${player.name}…`);
        } catch (error) {
            toast.error(`Could not open ${file.name} in ${player.name}: ${String(error)}`);
        } finally {
            setOpeningExternalPlayer(null);
        }
    }, [openingExternalPlayer, externalVideoPlayers, file.id, file.name, activeFolderId, getActiveVideo]);

    // Sync volume to whichever video element is actually visible.
    const applyVolume = useCallback((v: number, muted: boolean) => {
        const video = getActiveVideo();
        if (video) {
            video.volume = muted ? 0 : v;
            video.muted = muted;
        }
    }, [getActiveVideo]);

    const handleVolumeChange = useCallback((newVolume: number) => {
        const clamped = Math.max(0, Math.min(1, newVolume));
        setVolume(clamped);
        setIsMuted(clamped === 0);
        applyVolume(clamped, clamped === 0);
        if (clamped > 0) volumeBeforeMute.current = clamped;
    }, [applyVolume]);

    // Wire abortMse ref for progressive handler
    abortMseRef.current = abortMse;

    const toggleMute = useCallback(() => {
        if (isMuted) {
            setIsMuted(false);
            setVolume(volumeBeforeMute.current);
            applyVolume(volumeBeforeMute.current, false);
        } else {
            volumeBeforeMute.current = volume || 1;
            setIsMuted(true);
            setVolume(volume);
            applyVolume(volume, true);
        }
    }, [isMuted, volume, applyVolume]);

    // Re-apply volume when the visible video element changes.
    useEffect(() => {
        const video = getActiveVideo();
        if (video) {
            video.volume = isMuted ? 0 : volume;
            video.muted = isMuted;
        }
    }, [isMuted, volume, hlsPhase, msePhase, playbackMode, useFallback, getActiveVideo]);

    const getMoviePosition = useCallback(() => {
        const remux = useFallback && playbackMode === 'original' && !!fmp4StreamUrl;
        if (playbackMode === 'original' && pendingSeekRef.current) return pendingSeekRef.current.time;
        const video = getActiveVideo();
        return video ? video.currentTime + (remux ? remuxWindowRef.current.start : 0) : movieTime;
    }, [getActiveVideo, useFallback, playbackMode, fmp4StreamUrl, movieTime]);

    const requestRemuxAt = useCallback((time: number) => {
        const video = getActiveVideo();
        const play = pendingSeekRef.current?.play ?? (video && (nativePlaybackStartedRef.current || video.currentTime > 0)
            ? !video.paused : true);
        pendingSeekRef.current = { time, play };
        setMovieTime(time);
        setNativePaused(!play);
        remuxGenerationRef.current += 1;
        fmp4RemuxingRef.current = false;
        setFmp4RemuxError(null);
        handleProgressiveDetected(time);
    }, [getActiveVideo, handleProgressiveDetected]);

    const leaveHls = useCallback(() => {
        hlsGenerationRef.current += 1;
        hlsDesiredJobRef.current = null;
        liveHlsRef.current = false;
        const jobId = currentJobIdRef.current;
        currentJobIdRef.current = null;
        if (jobId) invoke('cmd_cancel_transcode', {
            jobId, sessionId: streamSessionRef.current?.id,
        }).catch(() => {});
        if (pollTimerRef.current) clearInterval(pollTimerRef.current);
        pollTimerRef.current = null;
        setHlsPlaylistUrl(null);
        setHlsPhase('idle');
        setHlsQuality(null);
        setQuality('original');
        setPlaybackMode('original');
    }, [setQuality]);

    const seekTo = useCallback((time: number) => {
        if (playbackMode === 'hls' && liveHlsRef.current) {
            const video = getActiveVideo();
            const seekable = video?.seekable;
            if (time > 0 && (!seekable?.length || time > seekable.end(seekable.length - 1))) {
                if (transcodeCapsRef.current?.available) {
                    requestRemuxAt(time);
                    leaveHls();
                } else {
                    toast.info('This quality is still converting. More of the video will be seekable shortly.');
                }
                return;
            }
        }
        const remuxComplete = playbackMode === 'original' && !!fmp4StreamUrl
            && remuxWindowRef.current.complete;
        if (playbackMode === 'original' && pendingSeekRef.current) {
            const duration = sourceDurationRef.current;
            const target = Math.max(0, Math.min(duration ?? Infinity, time));
            if (!fmp4StreamUrl && useFallback
                && !remuxWindowRef.current.key && !fmp4RemuxingRef.current) {
                // The native original decoder has an unresolved seek (it may
                // report an incorrect 20 ms duration). Repeated user seeks
                // must update the pending target, never launch a remux just
                // because decoder recovery has not started yet.
                pendingSeekRef.current.time = target;
                setMovieTime(target);
                return;
            }
            if (!remuxComplete || target < remuxWindowRef.current.start) {
                requestRemuxAt(target);
                return;
            }
            // Seeking again while the *completed* video initializes should
            // update the saved absolute position, not start a fresh remux.
            pendingSeekRef.current.time = target;
            setMovieTime(target);
        }
        const video = getActiveVideo();
        if (!video) return;
        const remux = useFallback && playbackMode === 'original' && !!fmp4StreamUrl;
        const plan = planPlaybackSeek(video, remuxWindowRef.current.start, sourceDurationRef.current,
            time, remux, remux && remuxWindowRef.current.complete);
        const truncatedOriginal = !remux && useFallback && playbackMode === 'original'
            && sourceDurationRef.current !== null && sourceDurationRef.current > 3
            && (!Number.isFinite(video.duration)
                || video.duration < sourceDurationRef.current - 2);
        if (plan.kind === 'local' && plan.time > 0
            && ((remux && shouldDeferCompletedRemuxSeek(
                video, remuxWindowRef.current.complete, plan.time))
                || (truncatedOriginal && shouldDeferCompletedRemuxSeek(
                    video, true, plan.time)))) {
            const play = pendingSeekRef.current?.play ?? !nativeUserPauseRef.current;
            pendingSeekRef.current = { time, play };
            setMovieTime(time);
            return;
        }
        if (plan.kind === 'remux') requestRemuxAt(plan.time);
        else video.currentTime = plan.time;
    }, [getActiveVideo, useFallback, playbackMode, fmp4StreamUrl, requestRemuxAt, leaveHls]);

    const seekBy = useCallback((seconds: number) => {
        seekTo(getMoviePosition() + seconds);
    }, [getMoviePosition, seekTo]);

    const restoreNativePosition = useCallback((video: HTMLVideoElement) => {
        const pending = pendingSeekRef.current;
        if (!pending) return;
        const localTime = Math.max(0, pending.time - remuxWindowRef.current.start);
        const plan = planPlaybackSeek(video, remuxWindowRef.current.start, sourceDurationRef.current,
            pending.time, !!remuxWindowRef.current.key, remuxWindowRef.current.complete);
        const truncatedOriginal = !remuxWindowRef.current.key
            && sourceDurationRef.current !== null && sourceDurationRef.current > 3
            && (!Number.isFinite(video.duration)
                || video.duration < sourceDurationRef.current - 2);
        if (localTime > 0 && (plan.kind !== 'local'
            || shouldDeferCompletedRemuxSeek(
                video, remuxWindowRef.current.complete, localTime,
            )
            || (truncatedOriginal && shouldDeferCompletedRemuxSeek(
                video, true, localTime,
            )))) return;
        video.currentTime = localTime;
        pendingSeekRef.current = null;
        savedTimeRef.current = 0;
        setNativeAutoPlay(pending.play);
        setNativePaused(!pending.play);
        if (pending.play) video.play().catch(() => {});
        else video.pause();
    }, []);

    // Manual recovery after the one automatic retry is opt-in. Reload only
    // the native URI, preserve play/seek intent and leave FFmpeg/MSE alone.
    const retryNativePlayback = useCallback(() => {
        const video = fallbackVideoRef.current;
        const resume = pendingSeekRef.current?.play ?? !nativeUserPauseRef.current;
        pendingSeekRef.current = {
            time: pendingSeekRef.current?.time
                ?? (remuxWindowRef.current.start + (video?.currentTime ?? 0)),
            play: resume,
        };
        nativeOriginalRetryRef.current = 0;
        nativeCompletedRetryRef.current = 0;
        setNativeAutoPlay(resume);
        setNativeError(null);
        setNativePhase('loading');
        if (video) {
            video.pause();
            video.removeAttribute('src');
            video.load();
        }
        setNativeDecoderRevision(n => n + 1);
    }, []);

    // Explicitly requested, bounded, private-memory compatibility for small
    // MP4s. The normal HTTP path stays streaming, and no FFmpeg job is started.
    const loadMemoryPlayback = useCallback(async () => {
        if (memoryFetchRef.current || !eligibleNativeMemoryMp4(file.name, file.size, !!file.is_split, streamUrl)) return;
        const controller = new AbortController();
        memoryFetchRef.current = controller;
        const generation = ++memoryGenerationRef.current;
        setMemoryLoading(true);
        try {
            const blob = await fetchBoundedNativeMp4(streamUrl, file.size, controller.signal);
            if (controller.signal.aborted || generation !== memoryGenerationRef.current) return;
            const objectUrl = URL.createObjectURL(blob);
            if (memoryMediaUrlRef.current) URL.revokeObjectURL(memoryMediaUrlRef.current);
            memoryMediaUrlRef.current = objectUrl;
            const video = fallbackVideoRef.current;
            const resume = pendingSeekRef.current?.play ?? !nativeUserPauseRef.current;
            pendingSeekRef.current = {
                time: pendingSeekRef.current?.time ?? (video?.currentTime ?? 0),
                play: resume,
            };
            nativeOriginalRetryRef.current = 0;
            setNativeAutoPlay(resume);
            if (video) {
                video.pause();
                video.removeAttribute('src');
                video.load();
            }
            setNativeError(null);
            setNativePhase('loading');
            setMemoryMedia({ source: streamUrl, url: objectUrl });
            setNativeDecoderRevision(n => n + 1);
        } catch {
            if (!controller.signal.aborted && generation === memoryGenerationRef.current) {
                // Never log or display the authenticated local source URL.
                setNativeError('In-memory MP4 recovery failed. Retry the stream or choose compatibility playback.');
                setNativePhase('error');
            }
        } finally {
            if (generation === memoryGenerationRef.current) {
                memoryFetchRef.current = null;
                setMemoryLoading(false);
            }
        }
    }, [file.name, file.size, file.is_split, streamUrl]);

    // The original MP4 can encounter the same WebKit/GStreamer 20ms decoder
    // truncation without any remux being involved. Give the *original*
    // URL its own single bounded native restart rather than treating a
    // valid file as a permanently unplayable clip. Do not start FFmpeg.
    useEffect(() => {
        if (fmp4StreamUrl || !useFallback || playbackMode !== 'original'
            || fmp4RemuxingRef.current || nativePhase === 'error') return;
        const video = fallbackVideoRef.current;
        if (!video) return;
        const generation = remuxGenerationRef.current;
        const timer = setTimeout(() => {
            if (generation !== remuxGenerationRef.current
                || fallbackVideoRef.current !== video || fmp4RemuxingRef.current
                || remuxWindowRef.current.key) return;
            const expected = sourceDurationRef.current;
            if (expected === null || expected <= 3 || !Number.isFinite(video.duration)
                || video.duration >= expected - 2) return;
            if (nativeOriginalRetryRef.current >= 1) {
                // WebKit may misread a valid moov-at-end HTTP MP4 as a
                // 20-200ms clip even after a fresh decoder restart. Once
                // confirmed twice, use the existing streamed fMP4 recovery
                // rather than telling the user to retry the same broken URL.
                if (transcodeCapsRef.current?.available && !memoryMediaUrl) {
                    handleProgressiveDetected();
                } else {
                    setNativeError('The video decoder could not load the complete file. Use Retry playback.');
                    setNativePhase('error');
                }
                return;
            }
            nativeOriginalRetryRef.current += 1;
            const resume = pendingSeekRef.current?.play ?? !nativeUserPauseRef.current;
            pendingSeekRef.current = {
                time: pendingSeekRef.current?.time ?? video.currentTime,
                play: resume,
            };
            setNativeAutoPlay(resume);
            video.pause();
            video.removeAttribute('src');
            video.load();
            setNativeDecoderRevision(n => n + 1);
        }, 2500);
        return () => clearTimeout(timer);
    }, [fmp4StreamUrl, useFallback, playbackMode, nativeDecoderRevision, nativePhase,
        memoryMediaUrl, handleProgressiveDetected]);

    // A native decoder can keep the first 200ms media fragment as the entire
    // duration even after fetching the fully completed, independently ranged
    // fMP4. One bounded *decoder* retry is safer than silently freezing or
    // triggering another expensive/duplicate Telegram remux.
    // The completed stream increments restartNonce without changing its URL,
    // so the retry guard must re-arm when that new player is mounted.
    useEffect(() => {
        if (!fmp4StreamUrl || !useFallback || playbackMode !== 'original'
            || !remuxWindowRef.current.complete) return;
        const video = fallbackVideoRef.current;
        if (!video) return;
        const generation = remuxGenerationRef.current;
        const timer = setTimeout(() => {
            if (generation !== remuxGenerationRef.current
                || fallbackVideoRef.current !== video || !remuxWindowRef.current.complete) return;
            const expected = sourceDurationRef.current == null ? null
                : sourceDurationRef.current - remuxWindowRef.current.start;
            if (expected === null || expected <= 3
                || !Number.isFinite(video.duration) || video.duration >= expected - 2) return;
            if (nativeCompletedRetryRef.current >= 1) {
                setNativeError('The media decoder could not load the completed video. Use Retry playback.');
                setNativePhase('error');
                return;
            }
            nativeCompletedRetryRef.current += 1;
            // Respect the user's latest play/pause choice and pending seek.
            const resume = pendingSeekRef.current?.play ?? !nativeUserPauseRef.current;
            pendingSeekRef.current = {
                time: pendingSeekRef.current?.time ?? remuxWindowRef.current.start + video.currentTime,
                play: resume,
            };
            setNativeAutoPlay(resume);
            video.pause();
            video.removeAttribute('src');
            video.load();
            setNativeDecoderRevision(n => n + 1);
        }, 2500);
        return () => clearTimeout(timer);
    }, [fmp4StreamUrl, useFallback, playbackMode, nativeDecoderRevision, restartNonce]);

    useEffect(() => {
        const audio = mediaTracks.audio_tracks.find(track => track.index === selectedAudioStream)
            ?? mediaTracks.audio_tracks[0];
        const video = fallbackVideoRef.current;
        if (audio?.codec === 'eac3' && video
            && !video.canPlayType('audio/mp4; codecs="ec-3"')
            && transcodeCapabilities?.available && useFallback && playbackMode === 'original'
            && nativePhase !== 'playing' && !fmp4StreamUrl && !fmp4RemuxError
            && !fmp4RemuxingRef.current) {
            handleProgressiveDetected();
        }
    }, [mediaTracks, selectedAudioStream, transcodeCapabilities, useFallback, playbackMode,
        nativePhase, fmp4StreamUrl, fmp4RemuxError, handleProgressiveDetected]);

    const togglePlayback = useCallback(() => {
        const pending = pendingSeekRef.current;
        if (pending) {
            pending.play = !pending.play;
            nativeUserPauseRef.current = !pending.play;
            setNativePaused(!pending.play);
            return;
        }
        const video = getActiveVideo();
        if (video) {
            nativeUserPauseRef.current = !video.paused;
            if (video.paused) video.play().catch(() => {});
            else video.pause();
        }
    }, [getActiveVideo]);

    const handleAudioTrackChange = useCallback((streamIndex: number) => {
        if (streamIndex === selectedAudioStreamRef.current) return;

        if (!transcodeCapsRef.current?.available) {
            toast.error('FFmpeg is required to switch embedded audio tracks.');
            return;
        }

        const moviePosition = getMoviePosition();
        savedTimeRef.current = moviePosition;

        selectedAudioStreamRef.current = streamIndex;
        setSelectedAudioStream(streamIndex);

        requestRemuxAt(moviePosition);
        if (playbackMode === 'hls') leaveHls();
    }, [getMoviePosition, requestRemuxAt, playbackMode, leaveHls]);

    const handleSubtitleTrackChange = useCallback(async (streamIndex: number | null) => {
        const generation = ++subtitleSelectionGenerationRef.current;
        setSelectedSubtitleStream(streamIndex);
        setSubtitleUrl(null);
        if (streamIndex === null) {
            setSubtitleLoading(false);
            return;
        }

        setSubtitleLoading(true);
        try {
            const relativeUrl = await invoke<string>('cmd_prepare_subtitle_track', {
                messageId: file.id,
                folderId: activeFolderId,
                streamIndex,
            });
            if (generation !== subtitleSelectionGenerationRef.current) return;
            const fullUrl = buildAuthenticatedSubtitleUrl(
                streamBaseRef.current, relativeUrl, streamAuthRef.current,
            );
            if (!fullUrl) throw new Error('Invalid local subtitle URL');
            setSubtitleUrl(fullUrl);
        } catch (error) {
            if (generation !== subtitleSelectionGenerationRef.current) return;
            toast.error('Subtitle preparation failed: ' + String(error));
            setSelectedSubtitleStream(null);
        } finally {
            if (generation === subtitleSelectionGenerationRef.current) setSubtitleLoading(false);
        }
    }, [file.id, activeFolderId]);

    // Extract stream token and base URL for each source, never retaining an
    // earlier file's local bearer token across a source switch or parse error.
    useEffect(() => {
        clearMemoryMedia();
        streamAuthRef.current = '';
        streamBaseRef.current = '';
        try {
            const url = new URL(streamUrl);
            streamAuthRef.current = url.searchParams.get('token') || '';
            // Store origin for constructing HLS URLs
            const streamIdx = streamUrl.indexOf(STREAM_BASE_KEY);
            if (streamIdx !== -1) {
                streamBaseRef.current = streamUrl.substring(0, streamIdx);
            } else {
                streamBaseRef.current = url.origin;
            }
        } catch { /* Invalid source has no reusable authentication or origin. */ }

        // Reset fMP4 remux state when switching files.
        // Prevents stale error / loading state from a previous progressive
        // MP4 from leaking into the current file (especially fragmented MP4s
        // that never trigger the remux pipeline).
        // Also bump the generation counter so any in-flight async invokes
        // from the previous file discard their results instead of setting
        // state on the new file.
        setFmp4Remuxing(false);
        setFmp4RemuxError(null);
        setFmp4StreamUrl(null);
        setNativeAutoPlay(true);
        fmp4RemuxingRef.current = false;
        remuxGenerationRef.current += 1;
        hlsGenerationRef.current += 1;
        hlsDesiredJobRef.current = null;
        liveHlsRef.current = false;
        if (pollTimerRef.current) {
            clearInterval(pollTimerRef.current);
            pollTimerRef.current = null;
        }
        // Temporary HLS cleanup is owned by the session release. A stale
        // source effect must never cancel a different player's stream.
        currentJobIdRef.current = null;
        remuxWindowRef.current = { start: 0, complete: false, key: null };
        pendingSeekRef.current = null;
        savedTimeRef.current = 0;
        savedPlaybackRef.current = true;
        nativeUserPauseRef.current = false;
        nativeOriginalRetryRef.current = 0;
        nativeCompletedRetryRef.current = 0;
        setNativeDecoderRevision(0);
        setMovieTime(0);
        setNativePaused(false);
        return () => {
            clearMemoryMedia();
            remuxGenerationRef.current += 1;
            hlsGenerationRef.current += 1;
            hlsDesiredJobRef.current = null;
            const fileKey = remuxWindowRef.current.key;
            if (fileKey) invoke('cmd_cancel_fmp4_stream', { fileKey, sessionId: streamSessionRef.current?.id }).catch(() => {});
        };
    }, [streamUrl, clearMemoryMedia]);

    // Probe embedded audio/subtitle tracks from the authenticated local
    // stream. Failure is non-fatal: playback still works without selectors.
    useEffect(() => {
        let cancelled = false;
        subtitleSelectionGenerationRef.current += 1;
        setMediaTracks({ audio_tracks: [], subtitle_tracks: [] });
        setSelectedAudioStream(null);
        selectedAudioStreamRef.current = null;
        setSelectedSubtitleStream(null);
        setSubtitleUrl(null);
        setSubtitleLoading(false);

        invoke<MediaTrackProbe>('cmd_probe_media_tracks', {
            messageId: file.id,
            folderId: activeFolderId,
        })
            .then(result => {
                if (cancelled) return;
                setMediaTracks(result);
                const defaultAudio = result.audio_tracks[0]?.index ?? null;
                setSelectedAudioStream(defaultAudio);
                selectedAudioStreamRef.current = defaultAudio;
            })
            .catch(() => {
                // FFprobe is optional. Keep playback functional and simply hide
                // track selectors when probing is unavailable.
            });

        return () => {
            cancelled = true;
            subtitleSelectionGenerationRef.current += 1;
        };
    }, [file.id, activeFolderId, streamUrl]);

    // ── Fetch transcode capabilities ──────────────────────────────────
    useEffect(() => {
        invoke<TranscodeCapabilities>('cmd_get_transcode_capabilities')
            .then(caps => {
                log('transcodeCapabilities', caps);
                setTranscodeCapabilities(caps);
                transcodeCapsRef.current = caps;
            })
            .catch(() => {
                const fallback = { available: false, variants: [], mode: 'original' as const };
                setTranscodeCapabilities(fallback);
                transcodeCapsRef.current = fallback;
            });
    }, []);

    useEffect(() => {
        if (!useFallback || playbackMode !== 'original' || fmp4StreamUrl || fmp4RemuxError
            || fmp4RemuxingRef.current
            || (nativePhase !== 'loading' && nativePhase !== 'ready')) return;
        const video = fallbackVideoRef.current;
        if (!video || (nativeUserPauseRef.current && !video.seeking)) return;
        const generation = remuxGenerationRef.current;
        const alreadyPlaying = nativePlaybackStartedRef.current;
        const observedTime = video.currentTime;
        const timer = setTimeout(() => {
            if (generation !== remuxGenerationRef.current || fallbackVideoRef.current !== video
                || fmp4RemuxingRef.current || (nativeUserPauseRef.current && !video.seeking)
                || Math.abs(video.currentTime - observedTime) > 0.25
                || (!video.seeking && video.readyState >= (alreadyPlaying
                    ? HTMLMediaElement.HAVE_FUTURE_DATA : HTMLMediaElement.HAVE_CURRENT_DATA))) return;
            if (transcodeCapsRef.current?.available) {
                logRef.current?.('Native video stalled; trying compatibility playback');
                handleProgressiveDetected();
            } else if (!alreadyPlaying) {
                setNativeError('Video did not start. Compatibility playback requires FFmpeg.');
                setNativePhase('error');
            }
        }, alreadyPlaying ? 8_000 : 20_000);
        return () => clearTimeout(timer);
    }, [streamUrl, useFallback, playbackMode, nativePhase, fmp4StreamUrl,
        fmp4RemuxError, transcodeCapabilities, handleProgressiveDetected]);

    // Non-MP4 containers get a direct native WebKit/GStreamer attempt first.
    // If the native element actually errors, the fallback handler below starts
    // the no-reencode fMP4 remux. This avoids downloading/remuxing multi-GB MKVs
    // before giving the platform decoder a chance to play them directly.

    // ── Poll transcode status ────────────────────────────────────────
    const pollTranscodeStatus = useCallback((jobId: string, quality: StreamingQuality) => {
        if (pollTimerRef.current) clearInterval(pollTimerRef.current);
        const generation = hlsGenerationRef.current;
        let consecutiveFailures = 0;
        let statusRequestInFlight = false;

        const intervalId = setInterval(async () => {
            if (statusRequestInFlight) return;
            statusRequestInFlight = true;
            let statusTimeout: ReturnType<typeof setTimeout> | null = null;
            try {
                const status = await Promise.race([
                    invoke<{
                        job_id: string;
                        status: string;
                        progress: number;
                        error: string | null;
                        playlist_url: string | null;
                    }>('cmd_get_transcode_status', { jobId }),
                    new Promise<never>((_, reject) => {
                        // A hung native IPC should not strand the quality
                        // spinner forever or issue overlapping poll requests.
                        statusTimeout = setTimeout(() => reject(new Error('HLS status timeout')), 8000);
                    }),
                ]);
                if (generation !== hlsGenerationRef.current
                    || currentJobIdRef.current !== jobId
                    || pollTimerRef.current !== intervalId) return;
                consecutiveFailures = 0;

                log('cmd_get_transcode_status result', { jobId, status: status.status, progress: status.progress });

                const q = quality;
                switch (status.status) {
                    case 'caching':
                        setHlsPhase('caching');
                        setHlsProgress(status.progress);
                        break;
                    case 'transcoding':
                        if (!liveHlsRef.current) setHlsPhase('transcoding');
                        setHlsProgress(status.progress);
                        break;
                    case 'streaming': {
                        // FFmpeg has already written three verified HLS
                        // segments; start playback while it continues.
                        if (!liveHlsRef.current) {
                            const liveUrl = buildAuthenticatedHlsUrl(
                                streamBaseRef.current, status.playlist_url || '', streamAuthRef.current,
                            );
                            if (!liveUrl) {
                                setHlsPhase('failed');
                                setHlsError('Invalid streaming playlist URL.');
                                clearInterval(intervalId);
                                pollTimerRef.current = null;
                                return;
                            }
                            liveHlsRef.current = true;
                            setHlsPlaylistUrl(liveUrl);
                            setHlsPhase('ready');
                        }
                        setHlsProgress(status.progress);
                        break;
                    }
                    case 'ready':
                        liveHlsRef.current = false;
                        clearInterval(intervalId);
                        pollTimerRef.current = null;
                        currentJobIdRef.current = null;
                        const readyPlaylistUrl = buildAuthenticatedHlsUrl(
                            streamBaseRef.current, status.playlist_url || '', streamAuthRef.current,
                        );
                        if (!readyPlaylistUrl) {
                            // Never publish a ready player without a valid,
                            // authenticated, same-origin HLS media URL.
                            setHlsPhase('failed');
                            setHlsError('Transcode completed without a valid playlist. Retry conversion.');
                            setHlsVariantStates(prev => ({ ...prev, [q]: 'failed' }));
                            break;
                        }
                        setHlsPhase('ready');
                        setHlsProgress(1);
                        setHlsPlaylistUrl(readyPlaylistUrl);
                        setHlsVariantStates(prev => ({ ...prev, [q]: 'ready' }));
                        break;
                    case 'error':
                        clearInterval(intervalId);
                        pollTimerRef.current = null;
                        currentJobIdRef.current = null;
                        setHlsPhase('failed');
                        setHlsError(status.error || 'Transcode failed');
                        setHlsVariantStates(prev => ({ ...prev, [q]: 'failed' }));
                        break;
                    case 'cancelled':
                        clearInterval(intervalId);
                        pollTimerRef.current = null;
                        currentJobIdRef.current = null;
                        setHlsPhase('failed');
                        setHlsError('Previous conversion is stopping. Select Retry to start it again.');
                        setHlsVariantStates(prev => ({ ...prev, [q]: 'failed' }));
                        break;
                }
            } catch {
                // Keep transient IPC errors retryable, but do not leave an HLS
                // quality and spinner stuck forever when status IPC is down.
                if (generation !== hlsGenerationRef.current
                    || currentJobIdRef.current !== jobId
                    || pollTimerRef.current !== intervalId) return;
                consecutiveFailures += 1;
                if (consecutiveFailures >= 5) {
                    clearInterval(intervalId);
                    pollTimerRef.current = null;
                    currentJobIdRef.current = null;
                    setHlsPhase('failed');
                    setHlsError('Could not check conversion status. Retry conversion.');
                    setHlsVariantStates(prev => ({ ...prev, [quality]: 'failed' }));
                }
            } finally {
                if (statusTimeout !== null) clearTimeout(statusTimeout);
                statusRequestInFlight = false;
            }
        }, 1000);
        pollTimerRef.current = intervalId;
    }, []);

    // ── Start HLS transcode for a quality ───────────────────────────
    const startTranscode = useCallback(async (quality: StreamingQuality) => {
        if (quality === 'original' || !HLS_QUALITIES.includes(quality)) return;
        const generation = ++hlsGenerationRef.current;
        // Matches TranscodeKey::job_id in src-tauri/src/transcode.rs.
        const desiredJobId = `${activeFolderId ?? 0}_${file.id}_${quality}`;
        hlsDesiredJobRef.current = desiredJobId;
        liveHlsRef.current = false;
        if (pollTimerRef.current) {
            clearInterval(pollTimerRef.current);
            pollTimerRef.current = null;
        }
        const previousJobId = currentJobIdRef.current;
        currentJobIdRef.current = null;
        if (previousJobId && previousJobId !== desiredJobId) {
            invoke('cmd_cancel_transcode', {
                jobId: previousJobId, sessionId: streamSessionRef.current?.id,
            }).catch(() => {});
        }

        log('startTranscode', { quality, activeFolderId, messageId: file.id });

        // Save current playback time from whichever player is visible.
        const video = getActiveVideo();
        savedTimeRef.current = getMoviePosition();
        savedPlaybackRef.current = pendingSeekRef.current?.play
            ?? (nativeUserPauseRef.current ? false : !video?.paused);
        pendingSeekRef.current = null;
        remuxGenerationRef.current += 1;
        fmp4RemuxingRef.current = false;
        setFmp4Remuxing(false);
        const fileKey = remuxWindowRef.current.key;
        if (fileKey) invoke('cmd_cancel_fmp4_stream', { fileKey, sessionId: streamSessionRef.current?.id }).catch(() => {});

        hlsQualityRef.current = quality;
        setPlaybackMode('hls');
        setHlsQuality(quality);
        setHlsPhase('preparing');
        setHlsProgress(0);
        setHlsError(null);
        setHlsPlaylistUrl(null);

        // Abort the original MSE download to save bandwidth
        abortMse();

        // Destroy existing HLS instance
        if (hlsRef.current) {
            hlsRef.current.destroy();
            hlsRef.current = null;
        }

        try {
            const session = streamSessionRef.current;
            if (!session || session.key !== `${activeFolderId ?? 0}_${file.id}`) {
                throw new Error('Player session is no longer active.');
            }
            await session.ready;
            if (generation !== hlsGenerationRef.current || streamSessionRef.current !== session) return;
            // The backend checks this session at entry, then keeps the shared
            // preparation alive while any viewer still needs the source.
            const result = await invoke<TranscodePrepareResult>('cmd_prepare_transcoded_stream', {
                messageId: file.id,
                folderId: activeFolderId,
                quality,
                fileName: file.name,
                sessionId: session.id,
            });

            // If a different movie/quality took over, only cancel the job
            // owned by this obsolete request; never display its playlist.
            if (generation !== hlsGenerationRef.current) {
                // Rust keys transcodes by folder/file/quality, not request
                // generation. Two requests can share a job. Never cancel the
                // replacement's matching job, nor a finished cached variant.
                if (result.job_id && result.status !== 'ready'
                    && result.job_id !== hlsDesiredJobRef.current) {
                    invoke('cmd_cancel_transcode', {
                        jobId: result.job_id, sessionId: session.id,
                    }).catch(() => {});
                }
                return;
            }
            log('cmd_prepare_transcoded_stream result', { status: result.status });

            if (result.status === 'ready') {
                const fullUrl = buildAuthenticatedHlsUrl(
                    streamBaseRef.current, result.playlist_url || '', streamAuthRef.current,
                );
                if (!fullUrl) {
                    // A disk/cache race or unexpected URL must never publish
                    // an unusable/cross-origin media source carrying local auth.
                    setHlsPhase('failed');
                    setHlsError('Transcode completed without a valid playlist. Retry conversion.');
                    setHlsVariantStates(prev => ({ ...prev, [quality]: 'failed' }));
                    return;
                }
                setHlsPhase('ready');
                setHlsProgress(1);
                setHlsVariantStates(prev => ({ ...prev, [quality]: 'ready' }));
                // The URL carries a local stream auth token; don't log it.
                log('hlsPlaylistReady', { quality });
                setHlsPlaylistUrl(fullUrl);
            } else if (result.status === 'error' || result.status === 'cancelled') {
                setHlsPhase('failed');
                setHlsError(result.status === 'cancelled'
                    ? 'Previous conversion is stopping. Select Retry to start it again.'
                    : 'Transcode failed to start');
                setHlsVariantStates(prev => ({ ...prev, [quality]: 'failed' }));
            } else {
                // Job started, begin polling
                currentJobIdRef.current = result.job_id;
                setHlsVariantStates(prev => ({ ...prev, [quality]: 'preparing' }));
                pollTranscodeStatus(result.job_id, quality);
            }
        } catch (e: any) {
            if (generation !== hlsGenerationRef.current) return;
            log('startTranscode error', String(e));
            setHlsPhase('failed');
            setHlsError(String(e));
            setHlsVariantStates(prev => ({ ...prev, [quality]: 'failed' }));
        }
    }, [file.id, file.name, activeFolderId, pollTranscodeStatus, abortMse, log, getActiveVideo, getMoviePosition]);

    // ── Handle quality change ────────────────────────────────────────
    const handleQualityChange = useCallback((quality: StreamingQuality) => {
        log('handleQualityChange', { quality, currentPlaybackMode: playbackMode, transcodeAvailable: transcodeCapabilities?.available });

        // Always update quality first for immediate UI feedback
        setQuality(quality);

        if (quality === 'original') {
            hlsGenerationRef.current += 1;
            hlsDesiredJobRef.current = null;
            const previousJobId = currentJobIdRef.current;
            currentJobIdRef.current = null;
            if (previousJobId) invoke('cmd_cancel_transcode', {
                jobId: previousJobId, sessionId: streamSessionRef.current?.id,
            }).catch(() => {});
            const restoreRemux = playbackMode === 'hls' && !!fmp4StreamUrl;
            const moviePosition = restoreRemux ? getMoviePosition() : 0;
            // Clean up HLS if switching back from transcode mode
            if (hlsRef.current) {
                hlsRef.current.destroy();
                hlsRef.current = null;
            }
            if (pollTimerRef.current) {
                clearInterval(pollTimerRef.current);
                pollTimerRef.current = null;
            }
            hlsQualityRef.current = null;
            currentJobIdRef.current = null;
            setPlaybackMode('original');
            setHlsPhase('idle');
            setHlsQuality(null);
            setHlsPlaylistUrl(null);
            setHlsError(null);
            setHlsVideoReady(false);
            // Force re-init of the MSE pipeline by bumping restartNonce.
            // This causes useAdaptiveStreaming to reinitialize with a new streamUrl.
            if (restoreRemux) requestRemuxAt(moviePosition);
            else {
                log('Switching back to original — restarting MSE pipeline');
                setRestartNonce(n => n + 1);
            }
        } else if (transcodeCapabilities?.available) {
            // FFmpeg available — start real transcode for HLS playback
            startTranscode(quality);
        } else {
            // FFmpeg not available — log and fall through to throttle mode
            log('FFmpeg unavailable, using throttle mode for', quality);
        }
        // If FFmpeg not available: setQuality already applied the bandwidth throttle
    }, [setQuality, startTranscode, transcodeCapabilities, playbackMode, log, fmp4StreamUrl, getMoviePosition, requestRemuxAt]);

    // ── Fullscreen helpers (DOM/video fullscreen first, Tauri fallback) ──
    const enterFullscreen = useCallback(async () => {
        // Fullscreen the complete player shell, NEVER the native <video>.
        // Fullscreening the video discards our audio/quality/seek controls.
        let fullscreenSuccess = false;
        try {
            if (containerRef.current?.requestFullscreen) {
                await containerRef.current.requestFullscreen({ navigationUI: 'hide' });
                fullscreenSuccess = true;
            }
        } catch { /* DOM fullscreen not supported or denied */ }
        // Fall back to Tauri window fullscreen if DOM fullscreen failed
        if (!fullscreenSuccess) {
            try {
                await getCurrentWindow().setFullscreen(true);
            } catch {}
        }
        setIsFullscreen(true);
    }, []);

    const exitFullscreen = useCallback(async () => {
        if (document.fullscreenElement) {
            await document.exitFullscreen().catch(() => {});
        }
        try {
            await getCurrentWindow().setFullscreen(false);
        } catch {}
        setIsFullscreen(false);
    }, []);

    const toggleFullscreen = useCallback(async () => {
        if (isFullscreenRef.current) {
            await exitFullscreen();
        } else {
            await enterFullscreen();
        }
    }, [enterFullscreen, exitFullscreen]);

    // Sync isFullscreen ref (avoids re-registering event listeners on every toggle)
    useEffect(() => {
        isFullscreenRef.current = isFullscreen;
    }, [isFullscreen]);

    // Sync isFullscreen from both Tauri window + DOM fullscreen sources.
    // isFullscreen is true if EITHER source reports fullscreen.
    useEffect(() => {
        let mounted = true;
        let unlistenFn: (() => void) | undefined;

        // Track each source independently so we can OR them
        let tauriFs = false;
        let domFs = false;
        const sync = () => { if (mounted) setIsFullscreen(tauriFs || domFs); };

        getCurrentWindow().onResized(async () => {
            if (!mounted) return;
            try {
                tauriFs = await getCurrentWindow().isFullscreen();
                sync();
            } catch {}
        }).then(fn => { if (mounted) unlistenFn = fn; });

        const onFsChange = () => {
            domFs = !!document.fullscreenElement;
            sync();
        };
        document.addEventListener('fullscreenchange', onFsChange);

        return () => {
            mounted = false;
            unlistenFn?.();
            document.removeEventListener('fullscreenchange', onFsChange);
        };
    }, []);

    // ── Initialize hls.js when playlist is ready ─────────────────────
    useEffect(() => {
        if (playbackMode !== 'hls' || !hlsPlaylistUrl || hlsPhase !== 'ready') return;
        if (!hlsVideoReady) {
            log('hlsVideoRef not ready yet, waiting for callback ref...');
            return;
        }
        const video = hlsVideoRef.current;
        if (!video) return;

        // Playlist URL contains the local bearer token: log no URL or query.
        log('Initializing HLS playback');

        // Seek to saved position
        const savedTime = savedTimeRef.current;

        // Log HLS metadata once the video loads
        const onHlsMetadata = () => {
            const v = hlsVideoRef.current;
            if (v) {
                log('HLS metadata', {
                    width: v.videoWidth,
                    height: v.videoHeight,
                });
                // Capture source resolution for the badge
                if (v.videoWidth > 0 && v.videoHeight > 0) {
                    setSourceResolution(prev => prev || { w: v.videoWidth, h: v.videoHeight });
                }
            }
        };

        if (video.canPlayType('application/vnd.apple.mpegurl')) {
            const onLoadedMetadata = () => {
                onHlsMetadata();
                restoreCompatibilityPlayback(video, savedTime, savedPlaybackRef.current);
            };
            video.addEventListener('loadedmetadata', onLoadedMetadata, { once: true });
            video.src = hlsPlaylistUrl;
            return () => {
                video.removeEventListener('loadedmetadata', onLoadedMetadata);
                video.removeAttribute('src');
                video.load();
            };
        } else if (Hls.isSupported()) {
            const authValue = streamAuthRef.current;
            const hls = new Hls({
                enableWorker: true,
                lowLatencyMode: false,
                // Keep memory bounded while downloading multi-GB originals
                // and use the first segments, not the live edge, for movies.
                backBufferLength: 45,
                maxBufferLength: 24,
                startPosition: 0,
                liveSyncDurationCount: 2,
                // Authenticate only this player's local playlist/segment URLs.
                // Never send a local bearer token to an external HLS URI.
                xhrSetup: (xhr, url) => {
                    const allowedUrl = authorizeHlsChildRequestUrl(hlsPlaylistUrl, url, authValue);
                    if (!allowedUrl) {
                        // hls.js otherwise falls through to its original URL.
                        // Do not contact an unexpected off-origin child URI.
                        // The backend refuses this unauthenticated local path.
                        xhr.open('GET', new URL('/hls/_rejected', hlsPlaylistUrl).toString(), true);
                    } else if (allowedUrl !== url) {
                        xhr.open('GET', allowedUrl, true);
                    }
                },
            });
            hlsRef.current = hls;

            hls.loadSource(hlsPlaylistUrl);
            hls.attachMedia(video);

            hls.on(Hls.Events.MANIFEST_PARSED, () => {
                log('HLS MANIFEST_PARSED, seeking to', savedTime);
                onHlsMetadata();
                restoreCompatibilityPlayback(video, savedTime, savedPlaybackRef.current);
            });

            hls.on(Hls.Events.ERROR, (_event, data) => {
                if (data.fatal) {
                    log('HLS fatal error', data.type, data.details);
                    console.error('[HLS] Fatal error:', data.type, data.details);
                    setHlsError(`HLS playback error: ${data.details}`);
                    setHlsPhase('failed');
                    hls.destroy();
                    hlsRef.current = null;
                } else {
                    log('HLS non-fatal error', data.type, data.details);
                }
            });
            return () => {
                if (hlsRef.current === hls) {
                    hls.destroy();
                    hlsRef.current = null;
                }
            };
        } else {
            log('HLS not supported in this browser');
            setHlsError('HLS playback not supported in this browser');
            setHlsPhase('failed');
        }

    }, [playbackMode, hlsPlaylistUrl, hlsPhase, hlsVideoReady, log]);

    // ── Cancel ongoing transcode ─────────────────────────────────────
    const cancelTranscode = useCallback(() => {
        log('cancelTranscode');
        hlsGenerationRef.current += 1;
        hlsDesiredJobRef.current = null;
        const jobId = currentJobIdRef.current;
        currentJobIdRef.current = null;
        // Invalidate the old player immediately. Never await cancellation:
        // a slow/stale response must not wipe a newer user's quality choice.
        if (jobId) invoke('cmd_cancel_transcode', {
            jobId, sessionId: streamSessionRef.current?.id,
        }).catch(() => {});
        if (pollTimerRef.current) {
            clearInterval(pollTimerRef.current);
            pollTimerRef.current = null;
        }
        if (hlsRef.current) {
            hlsRef.current.destroy();
            hlsRef.current = null;
        }
        hlsQualityRef.current = null;
        setPlaybackMode('original');
        setHlsPhase('idle');
        setHlsQuality(null);
        setHlsPlaylistUrl(null);
        setHlsError(null);
        setHlsVideoReady(false);
        setQuality('original');
        // Force re-init of the MSE pipeline
        log('cancelTranscode — restarting MSE pipeline');
        setRestartNonce(n => n + 1);
    }, [setQuality, log]);

    // ── Retry failed transcode ───────────────────────────────────────
    const retryTranscode = useCallback(() => {
        if (hlsQuality) startTranscode(hlsQuality);
    }, [hlsQuality, startTranscode]);

    // ── Cleanup ──────────────────────────────────────────────────────
    useEffect(() => {
        return () => {
            hlsGenerationRef.current += 1;
            currentJobIdRef.current = null;
            // Last-viewer release handles cancellation + removal AFTER FFmpeg
            // exits. Do not cancel another viewer sharing this quality here.
            if (hlsRef.current) {
                hlsRef.current.destroy();
                hlsRef.current = null;
            }
            if (pollTimerRef.current) {
                clearInterval(pollTimerRef.current);
                pollTimerRef.current = null;
            }
        };
    }, []);

    // ── Keyboard shortcuts ───────────────────────────────────────────
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            const target = e.target as HTMLElement;
            // Select menus, inputs, and accessible sliders own their own arrow keys.
            // Do not seek the video while a keyboard user adjusts one of them.
            if (target?.tagName === 'INPUT' || target?.tagName === 'TEXTAREA'
                || target?.tagName === 'SELECT' || target?.isContentEditable
                || target?.getAttribute?.('role') === 'slider'
                || target?.getAttribute?.('role') === 'combobox') return;
            if ((target?.tagName === 'BUTTON' || target?.tagName === 'VIDEO') && e.key === ' ') return;
            const key = e.key.toLowerCase();
            if (e.shiftKey && e.key === 'ArrowRight') { e.preventDefault(); onNext?.(); }
            else if (e.shiftKey && e.key === 'ArrowLeft') { e.preventDefault(); onPrev?.(); }
            else if (e.key === 'ArrowRight' || key === 'l') { e.preventDefault(); seekBy(10); }
            else if (e.key === 'ArrowLeft' || key === 'j') { e.preventDefault(); seekBy(-10); }
            else if (e.key === 'Escape') {
                e.preventDefault();
                // Exit fullscreen first, then close
                if (isFullscreenRef.current) {
                    toggleFullscreen();
                } else {
                    onClose();
                }
            }
            else if (key === 'f') { e.preventDefault(); toggleFullscreen(); }
            else if (key === 'm') { e.preventDefault(); toggleMute(); }
            else if (key === 'd') { e.preventDefault(); toggleDebugOverlay(); }
            else if (e.key === ' ') {
                e.preventDefault();
                togglePlayback();
            }
        };
        window.addEventListener('keydown', handleKeyDown);
        return () => window.removeEventListener('keydown', handleKeyDown);
    }, [onClose, onNext, onPrev, seekBy, toggleFullscreen, toggleMute, toggleDebugOverlay, togglePlayback]);

    // Keep keyboard focus in the modal and return it to the invoking control
    // when the player closes. Native media controls still receive their keys.
    useEffect(() => {
        const previous = document.activeElement;
        const dialog = containerRef.current;
        dialog?.querySelector<HTMLButtonElement>('[data-player-close]')?.focus();
        const containFocus = (event: KeyboardEvent) => {
            if (event.key !== 'Tab' || !dialog) return;
            const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(
                'button:not([disabled]), select:not([disabled]), input:not([disabled]), video[controls], [tabindex]:not([tabindex="-1"])',
            )).filter(element => element.getClientRects().length > 0);
            if (!focusable.length) return;
            const first = focusable[0];
            const last = focusable[focusable.length - 1];
            if (!dialog.contains(document.activeElement)) {
                event.preventDefault();
                (event.shiftKey ? last : first).focus();
            } else if (event.shiftKey && document.activeElement === first) {
                event.preventDefault();
                last.focus();
            } else if (!event.shiftKey && document.activeElement === last) {
                event.preventDefault();
                first.focus();
            }
        };
        window.addEventListener('keydown', containFocus);
        return () => {
            window.removeEventListener('keydown', containFocus);
            if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
        };
    }, []);

    // One custom transport bar controls every playback engine (MSE, HLS,
    // and native/WebKit). Keep progress and pause state synchronized without
    // polling or touching the decoder on every React render.
    useEffect(() => {
        const video = getActiveVideo();
        if (!video) return;
        const update = () => {
            const offset = playbackMode === 'original' && useFallback && fmp4StreamUrl
                ? remuxWindowRef.current.start : 0;
            if (!pendingSeekRef.current && Number.isFinite(video.currentTime)) {
                const position = offset + video.currentTime;
                setMovieTime(previous => Math.abs(previous - position) >= 0.2 ? position : previous);
            }
            if (Number.isFinite(video.duration) && video.duration > 0) {
                const duration = offset + video.duration;
                setPlayerDuration(previous => Math.abs(previous - duration) > 0.4 ? duration : previous);
            }
            setNativePaused(video.paused);
        };
        video.playbackRate = playbackRate;
        for (const event of ['timeupdate', 'loadedmetadata', 'durationchange', 'seeking', 'seeked', 'playing', 'pause', 'ended']) {
            video.addEventListener(event, update);
        }
        update();
        return () => {
            for (const event of ['timeupdate', 'loadedmetadata', 'durationchange', 'seeking', 'seeked', 'playing', 'pause', 'ended']) {
                video.removeEventListener(event, update);
            }
        };
    }, [getActiveVideo, playbackMode, useFallback, fmp4StreamUrl, hlsVideoReady,
        nativeMediaUrl, msePhase, hlsPhase, playbackRate]);

    useEffect(() => {
        setPlayerDuration(0);
    }, [streamUrl]);

    // ── Determine current display state ──────────────────────────────
    const isHlsMode = playbackMode === 'hls';
    const isMseLoading = msePhase === 'loading' || msePhase === 'initializing';
    const isHlsLoading = hlsPhase === 'preparing' || hlsPhase === 'caching' || hlsPhase === 'transcoding';
    const displayPhase: string = isHlsMode ? hlsPhase : fmp4RemuxError ? 'error' : useFallback ? nativePhase : (isMseLoading ? 'loading' : msePhase);
    const displayError: string | null = isHlsMode ? hlsError : fmp4RemuxError || (useFallback ? nativeError : mseError);
    // An opt-in alternate playback engine for a native decoder failure.
    // Respect the backend's supported quality list and never upscale an
    // independently probed lower-resolution source.
    const compatibilityQuality = transcodeCapabilities?.available
        ? chooseCompatibilityQuality(transcodeCapabilities.variants, sourceHeight)
        : null;
    const showOriginalVideo = !isHlsMode && !useFallback;

    // Build the effective quality label
    const effectiveQuality: StreamingQuality = isHlsMode ? (hlsQuality || 'original') : 'original';

    const durationToShow = sourceDuration && sourceDuration > 0 ? sourceDuration : playerDuration;
    const scrubPosition = scrubTime ?? movieTime;
    const playedPercent = durationToShow > 0 ? Math.min(100, Math.max(0, scrubPosition / durationToShow * 100)) : 0;
    const chromeHidden = isFullscreen && !controlsVisible;
    const commitScrub = (position: number) => {
        if (Number.isFinite(position) && position >= 0) seekTo(position);
        setScrubTime(null);
    };

    return (
        <div
            className={'fixed inset-0 z-[200] flex items-center justify-center bg-black/85 backdrop-blur-md ' + (isFullscreen ? 'p-0' : 'p-3 sm:p-6')}
            onClick={onClose}
        >
            <div ref={containerRef} role="dialog" aria-modal="true" aria-label={'Video player: ' + file.name}
                data-testid="terarelay-stream-player"
                className={'tr-stream-player relative flex flex-col overflow-hidden bg-[#10101b] text-white border border-violet-400/30 shadow-[0_24px_100px_rgba(0,0,0,.85)] ' +
                    (isFullscreen ? 'fixed inset-0 h-screen w-screen rounded-none border-0' : 'w-[94vw] max-w-[1400px] max-h-[96vh] rounded-2xl')}
                onClick={e => e.stopPropagation()} onMouseMove={revealControls}
                onTouchStart={revealControls} onPointerDown={revealControls}
                onFocusCapture={() => {
                    setControlsVisible(true);
                    if (controlsIdleTimerRef.current !== null) clearTimeout(controlsIdleTimerRef.current);
                }}
                onBlurCapture={revealControls}
                style={{ cursor: chromeHidden ? 'none' : undefined }}
            >
                {/* One toolbar, for all playback engines: no duplicate or native video controls. */}
                <header className={'tr-stream-chrome z-40 flex shrink-0 items-center gap-3 bg-[#151522] px-4 sm:px-6 py-3.5 transition-opacity duration-300 ' +
                    (isFullscreen ? 'absolute inset-x-0 top-0 bg-gradient-to-b from-black/90 to-black/30 ' : 'relative border-b border-white/10 ') +
                    (chromeHidden ? 'opacity-0 pointer-events-none' : 'opacity-100')}>
                    <span aria-hidden="true" className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-violet-600 text-sm font-black shadow-[0_0_20px_rgba(124,58,237,.25)]">TR</span>
                    <div className="min-w-0 flex-1">
                        <h2 title={file.name} className="truncate text-sm sm:text-base font-semibold">{file.name}</h2>
                        {typeof currentIndex === 'number' && typeof totalItems === 'number' && totalItems > 1 &&
                            <span className="block text-[10px] text-white/40">{currentIndex + 1} of {totalItems}</span>}
                    </div>
                    <label className="tr-stream-select inline-flex shrink-0 items-center gap-2 rounded-xl border border-violet-400/60 bg-violet-500/15 px-2.5 py-2 text-xs font-medium focus-within:ring-2 focus-within:ring-violet-400" title="Stream quality">
                        <Monitor className="h-4 w-4 text-violet-300" aria-hidden="true" />
                        <select aria-label="Stream quality" data-testid="stream-quality"
                            value={adaptiveMode ? 'auto' : effectiveQuality}
                            onChange={event => {
                                const chosen = event.target.value;
                                if (chosen === 'auto') {
                                    setAdaptiveMode(true);
                                } else {
                                    if (adaptiveMode) setAdaptiveMode(false);
                                    handleQualityChange(chosen as StreamingQuality);
                                }
                                revealControls();
                            }}
                            className="max-w-[80px] cursor-pointer bg-transparent text-white outline-none">
                            {transcodeCapabilities?.available && <option value="auto">Auto</option>}
                            <option value="360p" disabled={!transcodeCapabilities?.available || (sourceHeight !== null && sourceHeight < 360)}>360p</option>
                            <option value="480p" disabled={!transcodeCapabilities?.available || (sourceHeight !== null && sourceHeight < 480)}>480p</option>
                            <option value="720p" disabled={!transcodeCapabilities?.available || (sourceHeight !== null && sourceHeight < 720)}>{QUALITY_LABELS['720p']}{hlsVariantStates['720p'] === 'transcoding' ? ' · preparing' : ''}</option>
                            <option value="1080p" disabled={!transcodeCapabilities?.available || (sourceHeight !== null && sourceHeight < 1080)}>1080p</option>
                            <option value="original">Original</option>
                        </select>
                    </label>
                    <label className="tr-stream-select hidden sm:inline-flex items-center gap-1.5 rounded-xl border border-white/15 bg-white/5 px-2.5 py-2 text-xs text-white/90 focus-within:ring-2 focus-within:ring-violet-400" title="Subtitles">
                        <Captions className="h-4 w-4 shrink-0" aria-hidden="true"/>
                        <select aria-label="Subtitles" value={selectedSubtitleStream ?? ''}
                            disabled={subtitleLoading || mediaTracks.subtitle_tracks.length === 0}
                            onChange={event => {
                                const id = event.target.value;
                                void handleSubtitleTrackChange(id === '' ? null : Number(id));
                            }}
                            className="w-[72px] max-w-[100px] cursor-pointer bg-transparent text-white outline-none disabled:opacity-60">
                            <option value="">{subtitleLoading ? 'Loading' : 'Subtitles'}</option>
                            {mediaTracks.subtitle_tracks.map((track, index) =>
                                <option value={track.index} key={track.index}>{mediaTrackLabel(track, index + 1, 'Subtitle')}</option>)}
                        </select>
                    </label>
                    <label className="tr-stream-select hidden sm:inline-flex items-center gap-1.5 rounded-xl border border-white/15 bg-white/5 px-2.5 py-2 text-xs text-white/90 focus-within:ring-2 focus-within:ring-violet-400" title="Audio">
                        <AudioLines className="h-4 w-4 shrink-0" aria-hidden="true"/>
                        <select aria-label="Audio track" value={selectedAudioStream ?? ''}
                            disabled={mediaTracks.audio_tracks.length < 2}
                            onChange={event => { if (event.target.value) handleAudioTrackChange(Number(event.target.value)); }}
                            className="w-[65px] max-w-[115px] cursor-pointer bg-transparent text-white outline-none disabled:opacity-60">
                            <option value="">Audio</option>
                            {mediaTracks.audio_tracks.map((track, index) =>
                                <option value={track.index} key={track.index}>{mediaTrackLabel(track, index + 1, 'Audio')}</option>)}
                        </select>
                    </label>
                    <button type="button" onClick={onDownload} disabled={!onDownload} aria-label="Download video"
                        className="hidden sm:flex items-center gap-2 rounded-xl border border-white/15 bg-white/5 px-3 py-2 text-xs font-medium hover:bg-white/10 disabled:opacity-40">
                        <Download className="h-4 w-4"/> Download
                    </button>
                    <button type="button" data-player-close onClick={onClose} aria-label="Close video player"
                        title="Close (Esc)" className="ml-1 rounded-lg p-2 text-white/80 transition hover:bg-white/10 hover:text-white">
                        <X className="h-5 w-5"/>
                    </button>
                </header>

                <div className={'tr-stream-video relative flex min-h-0 items-center justify-center overflow-hidden bg-black ' +
                    (isFullscreen ? 'h-full w-full' : 'w-full aspect-video')}>
                    {/* fMP4 remux loading overlay */}
                    {fmp4Remuxing && (
                        <div role="status" className="flex flex-col items-center gap-4 text-white absolute inset-0 bg-black/80 z-10">
                            <Zap className="w-10 h-10 text-telegram-primary animate-pulse" />
                            <div className="flex flex-col items-center gap-1">
                                <p className="text-sm font-medium">Converting to streaming format...</p>
                                <p className="text-[11px] text-white/30 mt-1">
                                    Preparing video and audio the decoder can play.
                                </p>
                            </div>
                        </div>
                    )}

                    {/* Error display */}
                    {!fmp4Remuxing && (displayPhase === 'error' || displayPhase === 'failed') && (
                        <div role="alert" className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-3 bg-black/80 text-white px-8">
                            <AlertTriangle className="w-10 h-10 text-red-400" />
                            <p className="text-sm text-red-400 font-medium">Playback Error</p>
                            <p className="text-xs text-white/40 text-center max-w-md">{displayError || 'Unknown error'}</p>
                            {!isHlsMode && fmp4RemuxError && (
                                <button type="button" onClick={() => handleProgressiveDetected()}
                                    className="mt-2 flex items-center gap-2 px-3 py-1.5 bg-white/10 hover:bg-white/20 rounded-lg text-xs font-medium"
                                    aria-label="Retry video stream">
                                    <RefreshCw className="w-3.5 h-3.5" />
                                    Retry video stream
                                </button>
                            )}
                            {!isHlsMode && useFallback && !fmp4RemuxError && (
                                <button type="button" onClick={retryNativePlayback} disabled={memoryLoading}
                                    className="mt-2 flex items-center gap-2 px-3 py-1.5 bg-white/10 hover:bg-white/20 rounded-lg text-xs font-medium disabled:opacity-40"
                                    aria-label="Retry video playback">
                                    <RefreshCw className="w-3.5 h-3.5" />
                                    Retry playback
                                </button>
                            )}
                            {!isHlsMode && useFallback && !fmp4StreamUrl && !fmp4RemuxError
                                && !memoryMediaUrl && eligibleNativeMemoryMp4(file.name, file.size, !!file.is_split, streamUrl) && (
                                <button type="button" onClick={memoryLoading ? clearMemoryMedia : loadMemoryPlayback}
                                    aria-label={memoryLoading ? 'Cancel in-memory MP4 playback download' : 'Try in-memory MP4 playback'}
                                    className="mt-2 flex items-center gap-2 px-3 py-1.5 bg-white/10 hover:bg-white/20 rounded-lg text-xs font-medium">
                                    {memoryLoading ? <StopCircle className="w-3.5 h-3.5" /> : <Play className="w-3.5 h-3.5" />}
                                    {memoryLoading ? 'Cancel in-memory MP4 download' : 'Try in-memory MP4 (max 8 MB, no FFmpeg)'}
                                </button>
                            )}
                            {!isHlsMode && useFallback && !fmp4RemuxError && compatibilityQuality && (
                                <button type="button" disabled={memoryLoading}
                                    onClick={() => startTranscode(compatibilityQuality)}
                                    aria-label="Try compatibility playback"
                                    className="mt-2 flex items-center gap-2 px-3 py-1.5 bg-white/10 hover:bg-white/20 rounded-lg text-xs font-medium disabled:opacity-40">
                                    <Play className="w-3.5 h-3.5" />
                                    Try compatibility playback ({compatibilityQuality}, FFmpeg)
                                </button>
                            )}
                            {isHlsMode && (
                                <button onClick={retryTranscode} className="mt-2 flex items-center gap-2 px-3 py-1.5 bg-white/10 hover:bg-white/20 rounded-lg text-xs font-medium transition-colors">
                                    <RefreshCw className="w-3.5 h-3.5" />
                                    Retry
                                </button>
                            )}
                        </div>
                    )}

                    {/* Loading overlay */}
                    {isHlsLoading && (
                        <div role="status" className="flex flex-col items-center gap-4 text-white absolute inset-0 bg-black/80 z-10">
                            <Loader2 className="w-10 h-10 text-telegram-primary animate-spin" />
                            <div className="flex flex-col items-center gap-1">
                                <p className="text-sm font-medium">
                                    {hlsPhase === 'preparing' ? `Preparing ${hlsQuality}...` :
                                     hlsPhase === 'caching' ? 'Downloading source...' :
                                     hlsPhase === 'transcoding' ? `Transcoding to ${hlsQuality}...` : ''}
                                </p>
                                {hlsProgress > 0 && (
                                    <div className="flex items-center gap-2">
                                        <div role="progressbar" aria-label="Video transcoding progress" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(hlsProgress * 100)} className="w-32 h-1.5 bg-white/10 rounded-full overflow-hidden">
                                            <div className="h-full bg-telegram-primary rounded-full transition-all duration-300" style={{ width: `${Math.round(hlsProgress * 100)}%` }} />
                                        </div>
                                        <span className="text-[11px] text-white/40">{Math.round(hlsProgress * 100)}%</span>
                                    </div>
                                )}
                                {hlsPhase === 'preparing' && <p className="text-[11px] text-white/30 mt-1">Starting transcode job...</p>}
                            </div>
                            <button
                                onClick={cancelTranscode}
                                className="mt-3 flex items-center gap-1.5 px-3 py-1.5 bg-white/10 hover:bg-red-500/20 text-white/60 hover:text-red-400 rounded-lg text-xs font-medium transition-all border border-white/10 hover:border-red-500/30"
                                title="Cancel transcode"
                            >
                                <StopCircle className="w-3.5 h-3.5" />
                                Cancel
                            </button>
                        </div>
                    )}

                    {/* MSE loading overlay (original mode) */}
                    {showOriginalVideo && isMseLoading && (
                        <div role="status" className="flex flex-col items-center gap-4 text-white absolute inset-0 bg-black/80 z-10">
                            <Loader2 className="w-10 h-10 text-telegram-primary animate-spin" />
                            <div className="flex flex-col items-center gap-1">
                                <p className="text-sm font-medium">Loading video</p>
                                {loadProgress > 0 && (
                                    <div className="flex items-center gap-2">
                                        <div role="progressbar" aria-label="Video loading progress" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(loadProgress)} className="w-32 h-1.5 bg-white/10 rounded-full overflow-hidden">
                                            <div className="h-full bg-telegram-primary rounded-full transition-all duration-300" style={{ width: `${loadProgress}%` }} />
                                        </div>
                                        <span className="text-[11px] text-white/40">{loadProgress}%</span>
                                    </div>
                                )}
                                <p className="text-[11px] text-white/30 mt-1">
                                    {msePhase === 'initializing' ? 'Initializing decoder...' : 'Parsing video metadata...'}
                                </p>
                            </div>
                        </div>
                    )}

                    {/* Fallback: native playback with a source-duration timeline for remux windows. */}
                    {useFallback && !isHlsMode && (
                        <video
                            // A completed fMP4 source must create a fresh
                            // WebKit/GStreamer player. Updating src on the
                            // existing element can leave its former growing
                            // response's zero-duration parser attached even
                            // after the final ranged URL is selected.
                            // pendingSeekRef restores position/play intent.
                            key={`${nativeDecoderRevision}:${nativeMediaUrl}`}
                            ref={fallbackVideoRef}
                            crossOrigin="anonymous"
                            aria-label={`Video: ${file.name}`}
                            onClick={togglePlayback}
                            src={fmp4Remuxing ? undefined : nativeMediaUrl}
                            controls={false}
                            controlsList="nodownload"
                            // WebKitGTK's automatic HTTP preload enables GStreamer
                            // downloadbuffer, whose /var/tmp cache path can fail
                            // in constrained native runtimes. Seekable media
                            // remains requested on demand during playback.
                            preload="metadata"
                            autoPlay={nativeAutoPlay}
                            className="w-full h-full object-contain"
                            onLoadedMetadata={(event) => {
                                setNativePhase('ready');
                                setNativeError(null);
                                const video = event.currentTarget;
                                restoreNativePosition(video);
                                video.volume = isMuted ? 0 : volume;
                                video.muted = isMuted;
                            }}
                            onProgress={event => restoreNativePosition(event.currentTarget)}
                            onDurationChange={event => restoreNativePosition(event.currentTarget)}
                            onCanPlay={event => restoreNativePosition(event.currentTarget)}
                            onTimeUpdate={event => {
                                restoreNativePosition(event.currentTarget);
                                if (!pendingSeekRef.current) setMovieTime(remuxWindowRef.current.start + event.currentTarget.currentTime);
                            }}
                            onPlay={() => setNativePaused(false)}
                            onPause={() => setNativePaused(true)}
                            onPlaying={() => {
                                nativePlaybackStartedRef.current = true;
                                setNativePhase('playing');
                            }}
                            onWaiting={() => setNativePhase('loading')}
                            onEnded={() => setNativePhase('ended')}
                            onError={(event) => {
                                setNativePhase('error');
                                setNativeError(event.currentTarget.error?.message || 'The video decoder could not play this stream.');
                            }}
                        >
                            {subtitleUrl && (
                                <track
                                    key={`${subtitleUrl}_${remuxWindowRef.current.start}`}
                                    ref={showSubtitleTrack}
                                    kind="subtitles"
                                    src={subtitleUrl}
                                    srcLang={mediaTracks.subtitle_tracks.find(track => track.index === selectedSubtitleStream)?.language || 'und'}
                                    label={mediaTracks.subtitle_tracks.find(track => track.index === selectedSubtitleStream)?.title || 'Subtitle'}
                                    onLoad={event => shiftSubtitleCues(event.currentTarget.track, remuxWindowRef.current.start)}
                                    default
                                />
                            )}
                        </video>
                    )}

                    {/* HLS video element — rendered as soon as HLS mode is active so attachMedia works */}
                    {isHlsMode && (
                        <video
                            ref={hlsVideoCallbackRef}
                            crossOrigin="anonymous"
                            aria-label={`Video: ${file.name}`}
                            onClick={togglePlayback}
                            controls={false}
                            controlsList="nodownload"
                            autoPlay
                            className={`w-full h-full object-contain ${hlsPhase === 'ready' ? 'opacity-100' : 'opacity-0 absolute inset-0 pointer-events-none'}`}
                        >
                            {subtitleUrl && (
                                <track
                                    key={subtitleUrl}
                                    ref={showSubtitleTrack}
                                    kind="subtitles"
                                    src={subtitleUrl}
                                    srcLang={mediaTracks.subtitle_tracks.find(track => track.index === selectedSubtitleStream)?.language || 'und'}
                                    label={mediaTracks.subtitle_tracks.find(track => track.index === selectedSubtitleStream)?.title || 'Subtitle'}
                                    default
                                />
                            )}
                        </video>
                    )}

                    {/* MSE video element (original mode, hidden during loading/error/HLS) */}
                    {showOriginalVideo && (
                        <video
                            ref={mseVideoRef}
                            crossOrigin="anonymous"
                            aria-label={`Video: ${file.name}`}
                            onClick={togglePlayback}
                            controls={false}
                            controlsList="nodownload"
                            autoPlay
                            className={`w-full h-full object-contain ${(isMseLoading || msePhase === 'error') ? 'opacity-0' : 'opacity-100'}`}
                        >
                            {subtitleUrl && (
                                <track
                                    key={subtitleUrl}
                                    ref={showSubtitleTrack}
                                    kind="subtitles"
                                    src={subtitleUrl}
                                    srcLang={mediaTracks.subtitle_tracks.find(track => track.index === selectedSubtitleStream)?.language || 'und'}
                                    label={mediaTracks.subtitle_tracks.find(track => track.index === selectedSubtitleStream)?.title || 'Subtitle'}
                                    default
                                />
                            )}
                        </video>
                    )}
                    {/* D opens diagnostics only on request; no duplicate control surface. */}
                    {debugOverlay && (
                        <div className="pointer-events-auto absolute bottom-4 left-4 z-30 w-52 rounded-xl border border-white/15 bg-black/85 p-3 font-mono text-[10px] text-white/80 backdrop-blur-md">
                            <div className="mb-2 flex items-center gap-2 font-semibold"><Activity className="h-3.5 w-3.5"/> Playback diagnostics</div>
                            <div className="flex justify-between gap-2"><span>Mode</span><span>{isHlsMode ? 'HLS' : 'Original'}</span></div>
                            <div className="flex justify-between gap-2"><span>Speed</span><span>{measuredKbps > 0 ? Math.round(measuredKbps) + ' kbps' : '—'}</span></div>
                            <div className="flex justify-between gap-2"><span>Buffered</span><span>{debugBufferedSecs.toFixed(1)} s</span></div>
                            <div className="flex justify-between gap-2"><span>Playing</span><span>{playingResolution ? playingResolution.w + '×' + playingResolution.h : 'Unknown'}</span></div>
                            <button type="button" onClick={handleClearTranscodeCache} disabled={clearingCache}
                                className="mt-2 flex items-center gap-1.5 border-t border-white/10 pt-2 text-red-300 disabled:opacity-50">
                                <Trash2 className="h-3 w-3"/>{clearingCache ? 'Clearing cache…' : 'Clear this video’s HLS cache'}
                            </button>
                        </div>
                    )}
                </div>

                {/* Single transport bar for native, MSE and HLS playback. */}
                <footer className={'tr-stream-chrome z-40 flex shrink-0 items-center gap-3 border-t border-white/10 bg-[#151522] px-4 sm:px-6 py-3.5 transition-opacity duration-300 ' +
                    (isFullscreen ? 'absolute inset-x-0 bottom-0 border-0 bg-gradient-to-t from-black/90 to-black/30' : 'relative') +
                    (chromeHidden ? ' opacity-0 pointer-events-none' : ' opacity-100')}>
                    <button type="button" onClick={togglePlayback}
                        aria-label={nativePaused ? 'Play video' : 'Pause video'}
                        title="Play/Pause (Space)"
                        className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-violet-600 shadow-[0_0_20px_rgba(124,58,237,.4)] transition hover:bg-violet-500">
                        {nativePaused ? <Play className="h-5 w-5 fill-current"/> : <Pause className="h-5 w-5 fill-current"/>}
                    </button>
                    <button type="button" onClick={() => seekBy(-10)} aria-label="Back 10 seconds"
                        title="Back 10 seconds (Left/J)" className="rounded-lg p-2 text-white/85 hover:bg-white/10">
                        <RotateCcw className="h-5 w-5"/>
                    </button>
                    <button type="button" onClick={() => seekBy(10)} aria-label="Forward 10 seconds"
                        title="Forward 10 seconds (Right/L)" className="rounded-lg p-2 text-white/85 hover:bg-white/10">
                        <RotateCw className="h-5 w-5"/>
                    </button>
                    <span className="min-w-[47px] shrink-0 text-center font-mono text-[11px] text-white/80 sm:text-xs">
                        {formatPlayerClock(movieTime)}
                    </span>
                    <input type="range" aria-label="Video position" title="Seek"
                        min={0} max={Math.max(durationToShow, 0.01)} step={0.1}
                        value={Math.min(Math.max(0, scrubPosition), Math.max(durationToShow, 0.01))}
                        disabled={!Number.isFinite(durationToShow) || durationToShow <= 0}
                        onChange={event => setScrubTime(Number(event.target.value))}
                        onPointerUp={event => commitScrub(Number(event.currentTarget.value))}
                        onKeyUp={event => {
                            if (['ArrowLeft', 'ArrowRight', 'Home', 'End', 'PageUp', 'PageDown'].includes(event.key)) {
                                commitScrub(Number(event.currentTarget.value));
                            }
                        }}
                        onBlur={event => {
                            if (scrubTime !== null) commitScrub(Number(event.currentTarget.value));
                        }}
                        className="tr-stream-range min-w-0 flex-1 cursor-pointer disabled:opacity-35"
                        style={{ background: 'linear-gradient(to right, #8354f7 ' + playedPercent + '%, rgba(255,255,255,.24) ' + playedPercent + '%)' }}
                    />
                    <span className="hidden min-w-[47px] shrink-0 text-center font-mono text-[11px] text-white/80 sm:block sm:text-xs">
                        {formatPlayerClock(durationToShow)}
                    </span>
                    <button type="button" onClick={toggleMute} aria-label={isMuted ? 'Unmute video' : 'Mute video'}
                        title={isMuted ? 'Unmute (M)' : 'Mute (M)'} className="shrink-0 rounded-lg p-1.5 text-white/85 hover:bg-white/10">
                        {isMuted || volume === 0 ? <VolumeX className="h-5 w-5"/> :
                            volume < 0.5 ? <Volume1 className="h-5 w-5"/> : <Volume2 className="h-5 w-5"/>}
                    </button>
                    <input type="range" aria-label="Volume" min={0} max={1} step={0.05}
                        value={isMuted ? 0 : volume} onChange={event => handleVolumeChange(Number(event.target.value))}
                        className="tr-stream-range hidden w-20 cursor-pointer sm:block"
                        style={{ background: 'linear-gradient(to right, #8354f7 ' + (isMuted ? 0 : volume * 100) + '%, rgba(255,255,255,.24) ' + (isMuted ? 0 : volume * 100) + '%)' }}
                    />
                    <select aria-label="Playback speed" value={playbackRate}
                        onChange={event => setPlaybackRate(Number(event.target.value))}
                        className="hidden cursor-pointer rounded-xl border border-white/20 bg-white/5 px-2.5 py-2 text-xs text-white outline-none focus:ring-2 focus:ring-violet-400 sm:block">
                        <option value={0.5}>0.5×</option>
                        <option value={0.75}>0.75×</option>
                        <option value={1}>1.0×</option>
                        <option value={1.25}>1.25×</option>
                        <option value={1.5}>1.5×</option>
                        <option value={2}>2.0×</option>
                    </select>
                    {externalVideoPlayers.length > 0 && (
                        <div className="relative shrink-0">
                            <button
                                type="button"
                                data-testid="external-player-menu-button"
                                aria-haspopup="menu"
                                aria-expanded={externalPlayerMenuOpen}
                                aria-label="Open video with an external player"
                                title="Open with an installed player"
                                disabled={openingExternalPlayer !== null}
                                onClick={() => {
                                    revealControls();
                                    setExternalPlayerMenuOpen(open => !open);
                                }}
                                className="inline-flex h-10 items-center gap-1.5 rounded-xl border border-white/15 bg-white/5 px-2.5 text-xs font-medium text-white/90 transition hover:bg-white/10 disabled:opacity-50 sm:px-3"
                            >
                                <ExternalLink className="h-4 w-4" aria-hidden="true" />
                                <span className="hidden sm:inline">Open with</span>
                                <ChevronDown className="h-3.5 w-3.5 text-white/60" aria-hidden="true" />
                            </button>
                            {externalPlayerMenuOpen && (
                                <div
                                    role="menu"
                                    aria-label="Installed external video players"
                                    className="absolute bottom-full right-0 z-50 mb-2 min-w-44 overflow-hidden rounded-xl border border-white/15 bg-[#1c1b2a] p-1.5 shadow-2xl"
                                >
                                    <p className="px-2.5 py-1.5 text-[10px] font-semibold uppercase tracking-wider text-white/40">
                                        Open video with
                                    </p>
                                    {externalVideoPlayers.map(player => (
                                        <button
                                            type="button"
                                            role="menuitem"
                                            key={player.id}
                                            disabled={openingExternalPlayer !== null}
                                            onClick={() => { void handleOpenExternalPlayer(player.id); }}
                                            className="flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-xs text-white/90 transition hover:bg-white/10 disabled:opacity-50"
                                        >
                                            {openingExternalPlayer === player.id
                                                ? <Loader2 className="h-4 w-4 animate-spin text-violet-300" />
                                                : <ExternalLink className="h-4 w-4 text-violet-300" />}
                                            <span>{player.name}</span>
                                        </button>
                                    ))}
                                </div>
                            )}
                        </div>
                    )}
                    <button type="button" onClick={toggleFullscreen}
                        aria-label={isFullscreen ? 'Exit fullscreen' : 'Enter fullscreen'}
                        title={isFullscreen ? 'Exit fullscreen (F)' : 'Fullscreen (F)'}
                        className="shrink-0 rounded-lg p-2 text-white/85 hover:bg-white/10">
                        {isFullscreen ? <Minimize2 className="h-5 w-5"/> : <Maximize2 className="h-5 w-5"/>}
                    </button>
                </footer>
            </div>
        </div>
    );
}
