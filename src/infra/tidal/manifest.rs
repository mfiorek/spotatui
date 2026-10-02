//! The playback manifest: what `tracks/<id>/playbackinfopostpaywall` returns.
//!
//! Pure: no network. The reply carries a base64 manifest whose shape depends
//! on `manifestMimeType`. The AAC tiers (LOW, HIGH) come back as **BTS**, a
//! JSON document with direct, unencrypted CDN URLs. HI_RES_LOSSLESS comes
//! back as an unencrypted MPEG-DASH FLAC manifest when the track has a hi-res
//! master; that one is not played yet. `audioQuality` names what the server
//! delivered, which can be lower than what was asked for.

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use serde::Deserialize;

/// The tier asked for: HIGH (AAC 320), which this client type always gets
/// over BTS.
pub const REQUESTED_QUALITY: &str = "HIGH";

/// The query of a playback-info request for `quality`.
pub fn playback_info_params(quality: &str) -> [(&'static str, String); 3] {
  [
    ("playbackmode", "STREAM".to_string()),
    ("audioquality", quality.to_string()),
    ("assetpresentation", "FULL".to_string()),
  ]
}

/// The request path of a track's playback info.
pub fn playback_info_path(track_id: &str) -> String {
  format!("tracks/{track_id}/playbackinfopostpaywall")
}

/// The `playbackinfopostpaywall` reply.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackInfo {
  #[serde(default)]
  pub audio_quality: String,
  #[serde(default)]
  pub manifest_mime_type: String,
  #[serde(default)]
  pub manifest: String,
}

/// The format the server delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
  Low,
  High,
  Lossless,
  HiRes,
  Other(String),
}

impl Delivered {
  pub fn from_audio_quality(quality: &str) -> Self {
    match quality {
      "LOW" => Delivered::Low,
      "HIGH" => Delivered::High,
      "LOSSLESS" => Delivered::Lossless,
      "HI_RES_LOSSLESS" | "HI_RES" => Delivered::HiRes,
      other => Delivered::Other(other.to_string()),
    }
  }

  /// The playbar label: the AAC fallback stays visible.
  pub fn label(&self) -> String {
    match self {
      Delivered::Low => "AAC 96".to_string(),
      Delivered::High => "AAC 320".to_string(),
      Delivered::Lossless => "FLAC 16/44.1".to_string(),
      Delivered::HiRes => "FLAC hi-res".to_string(),
      Delivered::Other(quality) => quality.clone(),
    }
  }
}

/// A playable source: one direct URL and its MIME type, for rodio's probe.
#[derive(Debug, PartialEq)]
pub struct StreamSource {
  pub url: String,
  pub mime_type: Option<String>,
  pub delivered: Delivered,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BtsManifest {
  #[serde(default)]
  mime_type: String,
  #[serde(default)]
  encryption_type: String,
  #[serde(default)]
  urls: Vec<String>,
}

/// Decode the manifest of a playback-info reply into a playable source.
pub fn stream_source(info: &PlaybackInfo) -> Result<StreamSource> {
  let raw = base64::engine::general_purpose::STANDARD
    .decode(info.manifest.trim())
    .context("decoding the Tidal manifest")?;
  let mime = info.manifest_mime_type.as_str();
  if mime.contains("vnd.tidal.bts") {
    let manifest: BtsManifest =
      serde_json::from_slice(&raw).context("parsing the Tidal manifest")?;
    let encryption = manifest.encryption_type.as_str();
    if !encryption.is_empty() && encryption != "NONE" {
      return Err(anyhow!("the stream is encrypted ({encryption})"));
    }
    let url = manifest
      .urls
      .into_iter()
      .next()
      .filter(|u| !u.is_empty())
      .ok_or_else(|| anyhow!("the manifest has no stream URL"))?;
    return Ok(StreamSource {
      url,
      mime_type: Some(manifest.mime_type).filter(|m| !m.is_empty()),
      delivered: Delivered::from_audio_quality(&info.audio_quality),
    });
  }
  if mime.contains("dash+xml") {
    return Err(anyhow!("hi-res (DASH) streams are not supported yet"));
  }
  Err(anyhow!("unsupported manifest type {mime:?}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn info(mime: &str, quality: &str, manifest: &str) -> PlaybackInfo {
    PlaybackInfo {
      audio_quality: quality.to_string(),
      manifest_mime_type: mime.to_string(),
      manifest: base64::engine::general_purpose::STANDARD.encode(manifest),
    }
  }

  const BTS: &str = "application/vnd.tidal.bts";

  #[test]
  fn a_bts_manifest_yields_its_first_url_and_mime_type() {
    let reply = info(
      BTS,
      "HIGH",
      r#"{"mimeType":"audio/mp4","codecs":"mp4a.40.2","encryptionType":"NONE","urls":["https://cdn/a.m4a","https://cdn/b.m4a"]}"#,
    );
    assert_eq!(
      stream_source(&reply).unwrap(),
      StreamSource {
        url: "https://cdn/a.m4a".to_string(),
        mime_type: Some("audio/mp4".to_string()),
        delivered: Delivered::High,
      }
    );
  }

  #[test]
  fn a_bts_manifest_without_an_encryption_type_is_plain() {
    let reply = info(BTS, "LOW", r#"{"urls":["https://cdn/a.m4a"]}"#);
    let source = stream_source(&reply).unwrap();
    assert_eq!(source.mime_type, None);
    assert_eq!(source.delivered, Delivered::Low);
  }

  #[test]
  fn an_encrypted_bts_manifest_is_refused() {
    let reply = info(
      BTS,
      "HIGH",
      r#"{"encryptionType":"OLD_AES","urls":["https://cdn/a.m4a"]}"#,
    );
    let err = stream_source(&reply).unwrap_err();
    assert!(err.to_string().contains("encrypted"), "{err:#}");
  }

  #[test]
  fn a_bts_manifest_without_urls_is_an_error() {
    for body in [r#"{"urls":[]}"#, r#"{"urls":[""]}"#, "{}"] {
      assert!(stream_source(&info(BTS, "HIGH", body)).is_err(), "{body}");
    }
  }

  #[test]
  fn a_dash_manifest_is_not_supported_yet() {
    let reply = info("application/dash+xml", "HI_RES_LOSSLESS", "<MPD/>");
    let err = stream_source(&reply).unwrap_err();
    assert!(err.to_string().contains("DASH"), "{err:#}");
  }

  #[test]
  fn an_unknown_manifest_type_is_an_error() {
    assert!(stream_source(&info("application/x-other", "HIGH", "{}")).is_err());
  }

  #[test]
  fn a_manifest_that_is_not_base64_is_an_error() {
    let reply = PlaybackInfo {
      audio_quality: "HIGH".to_string(),
      manifest_mime_type: BTS.to_string(),
      manifest: "not base64!".to_string(),
    };
    assert!(stream_source(&reply).is_err());
  }

  #[test]
  fn delivered_qualities_have_playbar_labels() {
    let label = |q: &str| Delivered::from_audio_quality(q).label();
    assert_eq!(label("LOW"), "AAC 96");
    assert_eq!(label("HIGH"), "AAC 320");
    assert_eq!(label("LOSSLESS"), "FLAC 16/44.1");
    assert_eq!(label("HI_RES_LOSSLESS"), "FLAC hi-res");
    assert_eq!(label("DOLBY_ATMOS"), "DOLBY_ATMOS");
  }

  #[test]
  fn the_playback_info_request_streams_the_full_asset() {
    assert_eq!(
      playback_info_path("123"),
      "tracks/123/playbackinfopostpaywall"
    );
    assert_eq!(
      playback_info_params("HIGH"),
      [
        ("playbackmode", "STREAM".to_string()),
        ("audioquality", "HIGH".to_string()),
        ("assetpresentation", "FULL".to_string()),
      ]
    );
  }
}
