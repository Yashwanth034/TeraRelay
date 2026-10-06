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

export function planPlaybackSeek(
    video: SeekMedia,
    windowStart: number,
    sourceDuration: number | null,
    target: number,
    remux: boolean,
): { kind: 'local' | 'remux'; time: number } {
    const limit = remux ? sourceDuration : video.duration;
    const absolute = Math.max(0, Math.min(Number.isFinite(limit) && limit !== null ? limit : Infinity, target));
    const local = absolute - (remux ? windowStart : 0);
    if (!remux || (local >= 0 && (containsTime(video.seekable, local) || containsTime(video.buffered, local)))) {
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
