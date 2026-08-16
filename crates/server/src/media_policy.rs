//! Operator policy for the codecs viewers may send inbound.
//!
//! Deliberately separate from what this host can decode
//! (`media_input::camera_codec_mask`, `video_decode::available`): that is a
//! capability probe of the machine, this is a restriction the operator places
//! on top of it. The two are intersected before anything reaches a client.

use blit_remote::media::{
    AUDIO_CODEC_OPUS, AUDIO_CODEC_PCM, AUDIO_CODECS_ALL, VIDEO_CODEC_AV1, VIDEO_CODEC_AV1_444,
    VIDEO_CODEC_H264, VIDEO_CODEC_H264_444, VIDEO_CODEC_MJPEG, VIDEO_CODECS_ALL,
};

/// Which inbound codecs viewers may use, as `VIDEO_CODEC_*` / `AUDIO_CODEC_*`
/// bitmasks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaCodecPolicy {
    /// Camera formats. Motion JPEG is always kept: the server capability
    /// message is rejected by clients without it, and it is the one format
    /// every browser can produce with no encoder at all.
    pub camera: u8,
    /// Microphone formats. PCM is always kept, for the same reason — it is
    /// the fallback `startMicrophone` reaches when Opus is unavailable.
    pub microphone: u8,
}

impl Default for MediaCodecPolicy {
    fn default() -> Self {
        Self {
            camera: VIDEO_CODECS_ALL,
            microphone: AUDIO_CODECS_ALL,
        }
    }
}

impl MediaCodecPolicy {
    /// Runtime defaults, overridable with `BLIT_MEDIA_CAMERA_CODECS` and
    /// `BLIT_MEDIA_MICROPHONE_CODECS` (comma-separated lists).
    ///
    /// A value that does not parse leaves that axis unrestricted rather than
    /// silently narrowing it — the same shape as
    /// [`crate::SurfaceEncoderPreference::defaults`]. The CLI flags reject
    /// bad input outright instead, because there a typo is a typed mistake
    /// rather than stale environment.
    pub fn defaults() -> Self {
        let default = Self::default();
        Self {
            camera: std::env::var("BLIT_MEDIA_CAMERA_CODECS")
                .ok()
                .and_then(|value| Self::parse_camera(&value).ok())
                .unwrap_or(default.camera),
            microphone: std::env::var("BLIT_MEDIA_MICROPHONE_CODECS")
                .ok()
                .and_then(|value| Self::parse_microphone(&value).ok())
                .unwrap_or(default.microphone),
        }
    }

    /// Parse a comma-separated camera codec list. Each name selects exactly
    /// one format: `h264` does not imply `h264-444`, so a list that wants
    /// both says both.
    pub fn parse_camera(value: &str) -> Result<u8, String> {
        parse_list(
            value,
            "camera codec",
            VIDEO_CODEC_MJPEG,
            |item| match item {
                "mjpeg" => Some(VIDEO_CODEC_MJPEG),
                "h264" => Some(VIDEO_CODEC_H264),
                "av1" => Some(VIDEO_CODEC_AV1),
                "h264-444" => Some(VIDEO_CODEC_H264_444),
                "av1-444" => Some(VIDEO_CODEC_AV1_444),
                _ => None,
            },
        )
    }

    /// Parse a comma-separated microphone codec list.
    pub fn parse_microphone(value: &str) -> Result<u8, String> {
        parse_list(
            value,
            "microphone codec",
            AUDIO_CODEC_PCM,
            |item| match item {
                "pcm" => Some(AUDIO_CODEC_PCM),
                "opus" => Some(AUDIO_CODEC_OPUS),
                _ => None,
            },
        )
    }
}

/// Fold a comma-separated list into a bitmask, always setting `mandatory`.
/// An empty list is the mandatory codec alone, not "everything": writing
/// `--camera-codecs ''` reads as "as little as the protocol permits".
fn parse_list(
    value: &str,
    what: &str,
    mandatory: u8,
    lookup: impl Fn(&str) -> Option<u8>,
) -> Result<u8, String> {
    let mut mask = mandatory;
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        mask |= lookup(item).ok_or_else(|| format!("unknown {what}: {item}"))?;
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camera_lists() {
        assert_eq!(
            MediaCodecPolicy::parse_camera("h264, av1-444"),
            Ok(VIDEO_CODEC_MJPEG | VIDEO_CODEC_H264 | VIDEO_CODEC_AV1_444)
        );
        // The base name never drags its 4:4:4 sibling in.
        assert_eq!(
            MediaCodecPolicy::parse_camera("h264"),
            Ok(VIDEO_CODEC_MJPEG | VIDEO_CODEC_H264)
        );
        // Motion JPEG survives a list that never mentions it, and an empty
        // list narrows to it rather than widening to everything.
        assert_eq!(MediaCodecPolicy::parse_camera(""), Ok(VIDEO_CODEC_MJPEG));
        assert_eq!(
            MediaCodecPolicy::parse_camera("av1"),
            Ok(VIDEO_CODEC_MJPEG | VIDEO_CODEC_AV1)
        );
        assert!(MediaCodecPolicy::parse_camera("vp9").is_err());
    }

    #[test]
    fn parses_microphone_lists() {
        assert_eq!(
            MediaCodecPolicy::parse_microphone("opus"),
            Ok(AUDIO_CODEC_PCM | AUDIO_CODEC_OPUS)
        );
        assert_eq!(
            MediaCodecPolicy::parse_microphone("pcm"),
            Ok(AUDIO_CODEC_PCM)
        );
        assert!(MediaCodecPolicy::parse_microphone("mp3").is_err());
    }

    #[test]
    fn default_allows_everything() {
        let policy = MediaCodecPolicy::default();
        assert_eq!(policy.camera, VIDEO_CODECS_ALL);
        assert_eq!(policy.microphone, AUDIO_CODECS_ALL);
    }
}
