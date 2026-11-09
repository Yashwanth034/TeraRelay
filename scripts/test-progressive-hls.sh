#!/usr/bin/env bash
# Synthetic MKV -> pipe -> live EVENT-HLS playback contract.
# Never reads real Telegram credentials, files or user application data.
set -euo pipefail
for tool in ffmpeg ffprobe timeout grep; do
  command -v "$tool" >/dev/null || { echo "SKIP progressive HLS QA: missing $tool"; exit 0; }
done
TMP="$(mktemp -d /tmp/terarelay-progressive-qa.XXXXXX)"
pid=""
cleanup() {
  if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || :; wait "$pid" 2>/dev/null || :; fi
  rm -rf "$TMP"
}
trap cleanup EXIT
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'testsrc2=size=320x180:rate=25' \
  -f lavfi -i 'sine=frequency=440:sample_rate=48000' \
  -t 24 -map 0:v:0 -map 1:a:0 \
  -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
  -c:a aac -f matroska "$TMP/original.mkv"
mkdir -p "$TMP/live"
(
  cat "$TMP/original.mkv" | timeout 40s ffmpeg -hide_banner -loglevel error \
    -readrate 2 -probesize 5000000 -analyzeduration 3000000 -i pipe:0 \
    -map '0:v:0' -map '0:a:0?' -sn -dn \
    -vf 'scale=-2:min(ih\,720)' \
    -c:v libx264 -preset superfast -crf 23 \
    -force_key_frames 'expr:gte(t,n_forced*4)' -sc_threshold 0 \
    -c:a aac -b:a 128k -maxrate 2800k -bufsize 5600k \
    -f hls -hls_time 4 -hls_playlist_type event -hls_list_size 0 \
    -hls_segment_filename "$TMP/live/segment_%03d.ts" "$TMP/live/index.m3u8"
) >"$TMP/ffmpeg.log" 2>&1 &
pid=$!
ready=0
for _ in $(seq 1 40); do
  playlist="$TMP/live/index.m3u8"
  if [[ -f "$playlist" ]]; then
    count=$(grep -c '^segment_[0-9]\+\.ts$' "$playlist" || :)
    if (( count >= 3 )) && ! grep -q '^#EXT-X-ENDLIST$' "$playlist"; then
      for i in 000 001 002; do test -s "$TMP/live/segment_$i.ts"; done
      ready=1
      break
    fi
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "ERROR: FFmpeg exited before live segments were available"
    cat "$TMP/ffmpeg.log"
    exit 1
  fi
  sleep 0.5
done
test "$ready" = 1 || { echo "ERROR: 3 live segments not published early"; cat "$TMP/ffmpeg.log"; exit 1; }
echo "PASS: 3 live HLS segments exist before the source finishes conversion"
wait "$pid"
pid=""
grep -q '^#EXT-X-PLAYLIST-TYPE:EVENT$' "$TMP/live/index.m3u8"
grep -q '^#EXT-X-ENDLIST$' "$TMP/live/index.m3u8"
ffprobe -hide_banner -v error -show_entries format=duration -of csv=p=0 "$TMP/live/index.m3u8" >/dev/null
echo "PASS: final EVENT playlist ends correctly and contains playable segments"
