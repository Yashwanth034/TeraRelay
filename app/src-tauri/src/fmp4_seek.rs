//! Timeline anchors for source-offset compatibility playback.
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub(super) struct SeekAnchor {
    pub source_timestamp: f64,
    pub window_start: f64,
}

pub(super) fn finite_number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}

fn parse_anchor(value: &serde_json::Value, requested: f64) -> Result<SeekAnchor, String> {
    let source_start = finite_number(&value["format"]["start_time"]).unwrap_or(0.0);
    let packet = value["packets"]
        .as_array()
        .and_then(|packets| {
            packets.iter().find(|packet| {
                packet["flags"]
                    .as_str()
                    .is_some_and(|flags| flags.contains('K'))
            })
        })
        .ok_or("Could not locate a video keyframe for this position")?;
    let source_timestamp =
        finite_number(&packet["pts_time"]).ok_or("The video keyframe has no valid timestamp")?;
    let window_start = (source_timestamp - source_start).max(0.0);
    if window_start > requested + 1.0 {
        return Err(
            "The source did not provide a keyframe before the requested position".to_string(),
        );
    }
    Ok(SeekAnchor {
        source_timestamp,
        window_start,
    })
}

async fn probe_interval(
    ffprobe: &Path,
    source: &str,
    timestamp: f64,
) -> Result<serde_json::Value, String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        tokio::process::Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-rw_timeout",
                "20000000",
                "-select_streams",
                "v:0",
                "-read_intervals",
                &format!("{timestamp:.6}%+#1"),
                "-show_packets",
                "-show_entries",
                "format=start_time:packet=pts_time,flags",
                "-of",
                "json",
            ])
            .arg(source)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "Video seek probe timed out".to_string())?
    .map_err(|error| format!("Could not start video seek probe: {error}"))?;
    if !output.status.success() {
        let mut error = String::from_utf8_lossy(&output.stderr).replace(source, "[video source]");
        if let Some(token) = source
            .split("token=")
            .nth(1)
            .and_then(|token| token.split('&').next())
        {
            if !token.is_empty() {
                error = error.replace(token, "[redacted]");
            }
        }
        return Err(format!("Video seek probe failed: {}", error.trim()));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Invalid video seek probe: {error}"))
}

pub(super) async fn probe_seek_anchor(
    ffprobe: &Path,
    source: &str,
    requested: f64,
) -> Result<SeekAnchor, String> {
    let initial = probe_interval(ffprobe, source, requested).await?;
    let source_start = finite_number(&initial["format"]["start_time"]).unwrap_or(0.0);
    // ffprobe intervals use source timestamps; the player uses elapsed movie time.
    // Sources can start before zero because of AAC priming, or at a nonzero PTS.
    let probe = if source_start.abs() > 0.000001 {
        probe_interval(ffprobe, source, requested + source_start).await?
    } else {
        initial
    };
    parse_anchor(&probe, requested)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyframe_anchor_preserves_movie_position_and_source_start() {
        let json = serde_json::json!({"format":{"start_time":"-0.064"},"packets":[{"pts_time":"5400.0","flags":"K__"}]});
        let anchor = parse_anchor(&json, 5403.25).unwrap();
        assert_eq!(anchor.source_timestamp, 5400.0);
        assert_eq!(anchor.window_start, 5400.064);
    }
    #[test]
    fn invalid_or_forward_only_seek_metadata_is_rejected() {
        for json in [
            serde_json::json!({"format":{},"packets":[]}),
            serde_json::json!({"format":{},"packets":[{"pts_time":"NaN","flags":"K__"}]}),
            serde_json::json!({"format":{},"packets":[{"pts_time":"200","flags":"K__"}]}),
        ] {
            assert!(parse_anchor(&json, 100.0).is_err());
        }
    }
}
