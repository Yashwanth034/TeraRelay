import { useState } from 'react';
import { Play, Pause } from 'lucide-react';

type TimeRangesLike = Pick<TimeRanges, 'length' | 'start' | 'end'>;
type SeekMedia = Pick<HTMLMediaElement, 'duration' | 'seekable' | 'buffered'>;

function containsTime(ranges: TimeRangesLike, time: number): boolean {
    for (let i = 0; i < ranges.length; i++) {
        if (time >= ranges.start(i) && time <= ranges.end(i)) return true;
    }
    return false;
}

// Keep transport diagnostics useful without exposing local stream credentials
// or flooding the player with raw FFmpeg logs.
export function describePlaybackError(error: unknown): string {
    const raw = String(error).replace(/token=[^\s&]+/gi, 'token=[redacted]');
    if (/Server returned 5XX|HTTP (?:error |status )?5\d\d|5XX Server Error/i.test(raw)) {
        return 'TeraRelay could not read the video from its local stream (HTTP 5xx). The Telegram chunks may be unavailable or this upload may need indexing recovery. Retry the existing upload recovery, then retry playback; do not delete or upload another copy.';
    }
    return raw.length > 500 ? raw.slice(0, 497) + '...' : raw;
}

// Native media elements can become paused as a *consequence* of reaching the
// end of a growing fMP4. That is not the user's pause intent: when the
// completed ranged stream replaces it, resume playback unless the user
// explicitly requested a pause while the replacement was pending.
export function shouldResumeAfterRemuxFinish(
    pending: { play: boolean } | null,
    video: Pick<HTMLMediaElement, 'paused' | 'ended'> | null,
    previouslyPlaying = false,
): boolean {
    if (pending) return pending.play;
    // WebKit/GStreamer may issue a 'pause' when the growing response closes,
    // without setting ended=true. Remember the user's last play intent, rather
    // than treating that transport pause as an explicit user request.
    return !video || video.ended || !video.paused || previouslyPlaying;
}

// Opt-in only: small local single-file MP4s can bypass a WebKitGTK HTTP
// range/duration bug without launching FFmpeg. Never hold a multi-GB Telegram
// video in browser memory or issue an untrusted remote URL request.
export const NATIVE_MEMORY_MP4_LIMIT = 8 * 1024 * 1024;

export function eligibleNativeMemoryMp4(name: string, size: number, isSplit: boolean, streamUrl: string): boolean {
    if (!/\.mp4$/i.test(name) || isSplit || !Number.isSafeInteger(size)
        || size <= 0 || size > NATIVE_MEMORY_MP4_LIMIT) return false;
    try {
        const url = new URL(streamUrl);
        return (url.protocol === 'http:' || url.protocol === 'https:')
            && ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname.toLowerCase())
            && !url.username && !url.password
            && /^\/stream\/[A-Za-z0-9_-]+\/[0-9]+$/.test(url.pathname)
            && !!url.searchParams.get('token');
    } catch {
        return false;
    }
}

export async function fetchBoundedNativeMp4(
    streamUrl: string,
    expectedSize: number,
    signal: AbortSignal,
    fetcher: typeof fetch = fetch,
): Promise<Blob> {
    if (!eligibleNativeMemoryMp4('source.mp4', expectedSize, false, streamUrl)) {
        throw new Error('In-memory playback is only available for small authenticated local MP4s');
    }
    // Keep this opt-in authenticated loopback fetch on its validated origin:
    // the browser must not follow a redirect to an external media endpoint.
    const response = await fetcher(streamUrl, { cache: 'no-store', redirect: 'error', signal });
    if (response.status !== 200) throw new Error('In-memory MP4 requires one complete HTTP 200 response');
    if (response.headers.get('content-type')?.split(';')[0].trim().toLowerCase() !== 'video/mp4') {
        throw new Error('In-memory source is not an MP4 response');
    }
    const announced = response.headers.get('content-length');
    if (announced !== null && (!/^\d+$/.test(announced)
        || Number(announced) !== expectedSize)) {
        throw new Error('In-memory MP4 response size does not match metadata');
    }
    if (!response.body) throw new Error('In-memory MP4 response has no body');
    const reader = response.body.getReader();
    const chunks: ArrayBuffer[] = [];
    let received = 0;
    let finished = false;
    try {
        while (true) {
            if (signal.aborted) throw new Error('In-memory playback cancelled');
            const { done, value } = await reader.read();
            if (done) { finished = true; break; }
            if (value.byteLength > expectedSize - received) {
                throw new Error('In-memory MP4 exceeded the declared size or limit');
            }
            received += value.byteLength;
            const copied = new Uint8Array(value.byteLength);
            copied.set(value);
            chunks.push(copied.buffer);
        }
    } finally {
        if (!finished) await reader.cancel().catch(() => {});
        reader.releaseLock();
    }
    if (signal.aborted) throw new Error('In-memory playback cancelled');
    if (received !== expectedSize) throw new Error('In-memory MP4 is incomplete');
    return new Blob(chunks, { type: 'video/mp4' });
}

// A decoder reload must never append HTTP query parameters to opaque
// browser-owned Blob/data URLs. React remounts the native video separately;
// only HTTP streaming endpoints can accept the _native cache-busting key.
export function nativeProgressiveRevisionUrl(streamUrl: string, revision: number): string {
    if (!Number.isSafeInteger(revision) || revision <= 0 || /^(blob|data):/i.test(streamUrl)) {
        return streamUrl;
    }
    // HTTP fragments are never sent to the server. Preserve them after the
    // cache-busting query or native retries silently reuse the old resource.
    const fragmentOffset = streamUrl.indexOf('#');
    const requestUrl = fragmentOffset === -1 ? streamUrl : streamUrl.slice(0, fragmentOffset);
    const fragment = fragmentOffset === -1 ? '' : streamUrl.slice(fragmentOffset);
    const separator = requestUrl.includes('?') ? '&' : '?';
    return `${requestUrl}${separator}_native=${revision}${fragment}`;
}

// WebKit/GStreamer can reuse the old growing-media parser when only the
// query string changes. Unique path segments force a separate media fetch;
// the authenticated server resolves these revision aliases to the same file.
export function nativeRemuxRevisionUrl(streamUrl: string, revision: number): string {
    if (!Number.isSafeInteger(revision) || revision <= 0) return streamUrl;
    try {
        const url = new URL(streamUrl);
        if (!/\/fmp4\/[A-Za-z0-9_-]+\/output\.mp4$/.test(url.pathname)) return streamUrl;
        url.pathname = url.pathname.replace(
            /\/([A-Za-z0-9_-]+)\/output\.mp4$/,
            (_, key: string) => `/${key}__refresh_${revision}/output.mp4`,
        );
        return url.toString();
    } catch {
        return streamUrl;
    }
}

// A native decoder can reject an otherwise valid MP4 even after a bounded
// reload. The user may explicitly choose the existing FFmpeg/HLS route.
// Prefer modest quality and do not upscale a known lower-resolution source.
export function chooseCompatibilityQuality(
    variants: ReadonlyArray<{ label: string; height: number; available: boolean }>,
    sourceHeight: number | null,
): '360p' | '480p' | '720p' | '1080p' | null {
    for (const quality of ['480p', '360p', '720p', '1080p'] as const) {
        const variant = variants.find(item => item.label === quality);
        if (variant?.available && (sourceHeight === null || variant.height <= sourceHeight)) {
            return quality;
        }
    }
    return null;
}

// The backend returns a same-origin, absolute-path HLS playlist. Never
// concatenate a raw bearer token into a URL: reserved characters must be
// encoded, and a malformed/cross-origin path must never receive the token.
export function buildAuthenticatedHlsUrl(
    streamBase: string,
    playlistPath: string,
    token: string,
): string | null {
    if (!streamBase || !playlistPath || !token || !playlistPath.startsWith('/hls/')) return null;
    try {
        const base = new URL(streamBase);
        if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password) return null;
        const playlist = new URL(playlistPath, base.origin);
        if (playlist.origin !== base.origin
            || !/^\/hls\/(?:[A-Za-z0-9_-]+\/)*index\.m3u8$/.test(playlist.pathname)) return null;
        playlist.hash = '';
        playlist.searchParams.set('token', token);
        return playlist.toString();
    } catch {
        return null;
    }
}

// hls.js loads child manifests and MPEG-TS segments separately from the
// initial playlist. The local stream bearer token must never be attached to
// a cross-origin URI smuggled into a manifest, even if it contains "/hls/".
// The backend only publishes local playlist and numbered TS segment paths.
export function authorizeHlsChildRequestUrl(
    playlistUrl: string,
    requestUrl: string,
    token: string,
): string | null {
    if (!playlistUrl || !requestUrl || !token) return null;
    try {
        const playlist = new URL(playlistUrl);
        if (!['http:', 'https:'].includes(playlist.protocol)
            || playlist.username || playlist.password) return null;
        const request = new URL(requestUrl, playlist);
        if (request.origin !== playlist.origin
            || request.username || request.password
            || !/^\/hls\/(?:[A-Za-z0-9_-]+\/)*(?:master\.m3u8|index\.m3u8|segment_[0-9]+\.ts)$/.test(request.pathname)) {
            return null;
        }
        request.hash = '';
        request.searchParams.set('token', token);
        return request.toString();
    } catch {
        return null;
    }
}

// Subtitle preparation returns one authorized loopback WebVTT path. Protect
// the stream token from malformed/cross-origin backend responses and encode
// reserved query characters, just as with the HLS media URLs.
export function buildAuthenticatedSubtitleUrl(
    streamBase: string,
    subtitlePath: string,
    token: string,
): string | null {
    if (!streamBase || !subtitlePath || !token || !subtitlePath.startsWith('/subtitle/')) return null;
    try {
        const base = new URL(streamBase);
        if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password) return null;
        const subtitle = new URL(subtitlePath, base.origin);
        if (subtitle.origin !== base.origin
            || !/^\/subtitle\/[A-Za-z0-9_-]+\/[0-9]+\.vtt$/.test(subtitle.pathname)) return null;
        subtitle.hash = '';
        subtitle.searchParams.set('token', token);
        return subtitle.toString();
    } catch {
        return null;
    }
}

// The Rust fMP4 remux command publishes only a local output.mp4 endpoint.
// Never concatenate raw session credentials to a backend-supplied path, and
// never authorize a URL outside this local remux route.
export function buildAuthenticatedRemuxUrl(
    streamBase: string,
    remuxPath: string,
    token: string,
): string | null {
    if (!streamBase || !remuxPath || !token || !remuxPath.startsWith('/fmp4/')) return null;
    try {
        const base = new URL(streamBase);
        if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password) return null;
        const remux = new URL(remuxPath, base.origin);
        if (remux.origin !== base.origin
            || !/^\/fmp4\/[A-Za-z0-9_-]+\/output\.mp4$/.test(remux.pathname)) return null;
        remux.hash = '';
        remux.searchParams.set('token', token);
        return remux.toString();
    } catch {
        return null;
    }
}

// A decoder-only retry should not launch expensive FFmpeg conversion for an
// ordinary MP4. But an explicit remux preference is already the user's choice:
// a second native decoder failure must not strand that requested recovery.
export function shouldRemuxAfterNativeDecoderFailure(
    preferRemux: boolean,
    nativeRetryCount: number,
): boolean {
    return preferRemux || nativeRetryCount === 0;
}

// Preserve seek position and an explicit pause when leaving native playback
// for HLS, which may otherwise autoplay upon parsing a ready manifest.
export function restoreCompatibilityPlayback(
    video: Pick<HTMLVideoElement, 'currentTime' | 'play' | 'pause'>,
    savedTime: number,
    shouldPlay: boolean,
): void {
    if (Number.isFinite(savedTime) && savedTime > 0) video.currentTime = savedTime;
    if (shouldPlay) void video.play().catch(() => {});
    else video.pause();
}

// A freshly replaced *completed* fMP4 can expose empty seekable/buffered
// ranges until its first successful metadata load. Keep the desired absolute
// movie time pending instead of starting a duplicate remux at that instant.
export function shouldDeferCompletedRemuxSeek(
    video: Pick<HTMLMediaElement, 'seekable' | 'buffered'>,
    complete: boolean,
    localTarget?: number,
): boolean {
    if (!complete) return false;
    // Some WebKit/GStreamer builds initially expose a 0.2s range from the
    // freshly completed source. Assigning currentTime outside that short
    // range can clamp to its apparent EOF and pause the decoder permanently.
    // Wait for the requested timestamp itself to become addressable.
    if (localTarget !== undefined && localTarget > 0) {
        return !containsTime(video.seekable, localTarget)
            && !containsTime(video.buffered, localTarget);
    }
    return video.seekable.length === 0 && video.buffered.length === 0;
}

export function planPlaybackSeek(
    video: SeekMedia,
    windowStart: number,
    sourceDuration: number | null,
    target: number,
    remux: boolean,
    complete = false,
): { kind: 'local' | 'remux'; time: number } {
    // A native decoder may briefly report only the first 0.02s/0.2s of
    // an otherwise complete MP4. Its truncated duration must not clamp a
    // requested movie seek before decoder recovery can restore it. Prefer
    // the separately probed source duration only on a clear mismatch.
    const nativeTruncated = !remux && sourceDuration !== null
        && Number.isFinite(sourceDuration) && sourceDuration > 3
        && (!Number.isFinite(video.duration) || video.duration < sourceDuration - 2);
    const limit = remux || nativeTruncated ? sourceDuration : video.duration;
    const absolute = Math.max(0, Math.min(Number.isFinite(limit) && limit !== null ? limit : Infinity, target));
    const local = absolute - (remux ? windowStart : 0);
    if (!remux || (local >= 0 && (complete || containsTime(video.seekable, local) || containsTime(video.buffered, local)))) {
        return { kind: 'local', time: local };
    }
    return { kind: 'remux', time: absolute };
}

export function shiftSubtitleCues(track: TextTrack, windowStart: number): void {
    const cues = Array.from(track.cues ?? []);
    for (const cue of cues) {
        if (cue.endTime <= windowStart) track.removeCue(cue);
        else {
            cue.startTime = Math.max(0, cue.startTime - windowStart);
            cue.endTime -= windowStart;
        }
    }
}

export function formatPlaybackTime(time: number): string {
    const seconds = Math.max(0, Math.floor(time));
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    const tail = String(seconds % 60).padStart(2, '0');
    return hours > 0 ? `${hours}:${String(minutes).padStart(2, '0')}:${tail}` : `${minutes}:${tail}`;
}

export function PlaybackTimeline({ currentTime, duration, paused, onTogglePlay, onSeek }: {
    currentTime: number; duration: number; paused: boolean;
    onTogglePlay: () => void; onSeek: (time: number) => void;
}) {
    const [draft, setDraft] = useState<number | null>(null);
    const commit = (time: number) => { setDraft(null); onSeek(time); };
    return (
        <div className="flex w-full items-center gap-3 rounded-lg bg-black/80 px-3 py-2 text-xs text-white">
            <button type="button" onClick={onTogglePlay} title={paused ? 'Play' : 'Pause'} aria-label={paused ? 'Play' : 'Pause'} className="shrink-0 p-1">
                {paused ? <Play className="h-4 w-4" /> : <Pause className="h-4 w-4" />}
            </button>
            <span className="tabular-nums shrink-0">{formatPlaybackTime(currentTime)}</span>
            <input type="range" aria-label="Movie timeline" min={0} max={duration} step={0.1}
                value={draft ?? Math.min(duration, Math.max(0, currentTime))}
                onChange={event => setDraft(Number(event.target.value))}
                onPointerUp={event => commit(Number(event.currentTarget.value))}
                onKeyUp={event => {
                    if (['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End', 'PageUp', 'PageDown', 'Enter'].includes(event.key)) {
                        commit(Number(event.currentTarget.value));
                    }
                }}
                onBlur={event => { if (draft !== null) commit(Number(event.currentTarget.value)); }}
                className="h-1 min-w-0 flex-1 cursor-pointer accent-white" />
            <span className="tabular-nums shrink-0">{formatPlaybackTime(duration)}</span>
        </div>
    );
}
