#!/usr/bin/env bash
# Real-media compatibility smoke test. Uses generated fixtures only; never
# reads the installed app's profile, Telegram data, or preexisting cache.
set -euo pipefail
for binary in ffmpeg ffprobe python3; do
  if ! command -v "$binary" >/dev/null 2>&1; then
    echo "SKIP HLS compatibility: $binary unavailable"
    exit 0
  fi
done
tmp="$(mktemp -d "${TMPDIR:-/tmp}/terarelay-hls-compat.XXXXXX")"
trap 'rm -rf -- "$tmp"' EXIT

mkdir -p "$tmp/hls"
ffmpeg -nostdin -hide_banner -loglevel error -y \
  -f lavfi -i 'testsrc2=size=640x480:rate=24' \
  -f lavfi -i 'sine=frequency=660:sample_rate=48000' \
  -t 10 -map 0:v:0 -map 1:a:0 \
  -c:v libx264 -preset ultrafast -pix_fmt yuv420p -c:a aac "$tmp/source.mp4"

# Mirrors the FFmpeg options in app/src-tauri/src/transcode.rs, without
# Telegram, downloading, or writing the app's actual media cache.
ffmpeg -nostdin -hide_banner -loglevel error -y -i "$tmp/source.mp4" \
  -map '0:v:0' -map '0:a:0?' -sn -dn -vf 'scale=-2:480' \
  -c:v libx264 -preset veryfast -crf 23 \
  -force_key_frames 'expr:gte(t,n_forced*4)' -sc_threshold 0 \
  -c:a aac -b:a 128k \
  -maxrate 2500k -bufsize 5000k -f hls -hls_time 4 \
  -hls_playlist_type vod \
  -hls_segment_filename "$tmp/hls/segment_%03d.ts" "$tmp/hls/index.m3u8"

python3 - "$tmp/hls" <<'PYCHECK'
from pathlib import Path
import sys
root = Path(sys.argv[1])
playlist = (root / 'index.m3u8').read_text()
assert '#EXT-X-ENDLIST' in playlist, 'incomplete playlist published as ready'
assert '#EXTINF:' in playlist, 'no HLS media duration entries'
segments = [line.strip() for line in playlist.splitlines()
            if line.strip() and not line.lstrip().startswith('#')]
assert len(segments) >= 3, f'HLS did not honor keyframed 4-second segments: {segments}'
for name in segments:
    assert name.startswith('segment_') and name.endswith('.ts') and name[8:-3].isdigit(), name
    assert (root / name).is_file() and (root / name).stat().st_size > 0, name
print(f'PASS completed HLS VOD playlist: {len(segments)} real segments')
PYCHECK

ffprobe -v error -show_entries stream=codec_type,codec_name,height \
  -of json "$tmp/hls/index.m3u8" > "$tmp/streams.json"
python3 - "$tmp/streams.json" <<'PYCHECK'
import json, sys
streams = json.load(open(sys.argv[1]))['streams']
assert any(s.get('codec_type') == 'video' and s.get('codec_name') == 'h264'
           and s.get('height') == 480 for s in streams), streams
assert any(s.get('codec_type') == 'audio' and s.get('codec_name') == 'aac'
           for s in streams), streams
print('PASS HLS compatible codecs: H.264 480p video and AAC audio')
PYCHECK

ffmpeg -nostdin -hide_banner -loglevel error -i "$tmp/hls/index.m3u8" \
  -map 0:v:0 -map '0:a:0?' -f null - >/dev/null
ffmpeg -nostdin -hide_banner -loglevel error -ss 6.5 -i "$tmp/hls/index.m3u8" \
  -t 1 -map 0:v:0 -f null - >/dev/null
echo 'PASS complete HLS decode and nonzero seek from generated media'
