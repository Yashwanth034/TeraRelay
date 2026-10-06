#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for tool in ffmpeg ffprobe python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "SKIP media contract: $tool is not installed"
    exit 0
  }
done

TMP="$(mktemp -d "${TMPDIR:-/tmp}/terarelay-media-qa.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/subtitle.srt" <<'SUBEOF'
1
00:00:00,000 --> 00:00:02,000
TeraRelay subtitle QA
SUBEOF

ffmpeg -hide_banner -loglevel error -y   -f lavfi -i "testsrc2=size=640x360:rate=24"   -f lavfi -i "sine=frequency=440:sample_rate=48000"   -f lavfi -i "sine=frequency=880:sample_rate=48000"   -i "$TMP/subtitle.srt"   -t 3   -map 0:v:0 -map 1:a:0 -map 2:a:0 -map 3:s:0   -c:v libx264 -preset ultrafast -pix_fmt yuv420p   -c:a aac -c:s srt   -metadata:s:a:0 language=eng -metadata:s:a:0 title="English QA"   -metadata:s:a:1 language=tel -metadata:s:a:1 title="Telugu QA"   -metadata:s:s:0 language=eng -metadata:s:s:0 title="English Subs"   "$TMP/source.mkv"

ffprobe -v error   -show_entries stream=index,codec_type,codec_name,channels:stream_tags=language,title   -of json "$TMP/source.mkv" > "$TMP/probe.json"

read -r AUDIO_INDEX SUBTITLE_INDEX < <(python3 - "$TMP/probe.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
audio = [s for s in data["streams"] if s.get("codec_type") == "audio"]
subs = [s for s in data["streams"] if s.get("codec_type") == "subtitle"]
assert len(audio) == 2, audio
assert len(subs) == 1, subs
assert audio[1].get("tags", {}).get("language") == "tel", audio[1]
print(audio[1]["index"], subs[0]["index"])
PY
)

ffmpeg -hide_banner -loglevel error -y   -i "$TMP/source.mkv"   -map 0:v:0 -map "0:${AUDIO_INDEX}?" -sn -dn   -c:v copy -c:a copy   -movflags frag_keyframe+empty_moov+default_base_moof   -f mp4 "$TMP/remux.mp4"

ffprobe -v error   -show_entries stream=codec_type:stream_tags=language   -of json "$TMP/remux.mp4" > "$TMP/remux.json"

python3 - "$TMP/remux.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
video = [s for s in data["streams"] if s.get("codec_type") == "video"]
audio = [s for s in data["streams"] if s.get("codec_type") == "audio"]
assert len(video) == 1, video
assert len(audio) == 1, audio
assert audio[0].get("tags", {}).get("language") == "tel", audio[0]
PY

grep -a -q "moof" "$TMP/remux.mp4"

ffmpeg -hide_banner -loglevel error -y   -i "$TMP/source.mkv"   -map "0:${SUBTITLE_INDEX}" -vn -an -c:s webvtt   "$TMP/subtitle.vtt"

grep -q '^WEBVTT' "$TMP/subtitle.vtt"
grep -q 'TeraRelay subtitle QA' "$TMP/subtitle.vtt"

PLAYER="app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx"
grep -Fq "seekBy(10)" "$PLAYER"
grep -Fq "seekBy(-10)" "$PLAYER"
grep -Fq "cmd_probe_media_tracks" "$PLAYER"
grep -Fq "cmd_prepare_subtitle_track" "$PLAYER"
grep -Fq "audioStreamIndex: selectedAudioStreamRef.current" "$PLAYER"
grep -Fq "handleProgressiveDetected();" "$PLAYER"

echo "PASS media contract"
echo "  MKV probe: 2 audio tracks + 1 subtitle discovered"
echo "  audio map: selected Telugu stream preserved in fMP4"
echo "  subtitle: selected embedded stream converted to WebVTT"
echo "  fMP4: fragmented output contains moof boxes"
echo "  player: seek/audio/subtitle/fallback wiring present"
