//! A live probe of the stream formats, run by hand before building on them.
//!
//! `cargo test --features tidal -- --ignored live_tidal_probe --nocapture`
//!
//! Asks for HI_RES_LOSSLESS on the first favorite tracks until one comes back
//! as DASH and one as BTS, then prints the MPD (URLs reduced to their path),
//! the box layout of the init segment and of segment 1, whether the CDN
//! honours `Range` on a segment, and the top-level boxes of the BTS file
//! (a `moof` there means fragmented MP4). URLs are never printed: their query
//! carries the CDN token.

use base64::Engine as _;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::Client;

use super::manifest::{self, PlaybackInfo};
use super::{auth, restore_login, track_id_from_uri, TidalSource, FAVORITES_URI};
use crate::core::source::{MediaSource, Searcher};

/// Tracks tried before giving up on finding both manifest kinds.
const MAX_TRACKS: usize = 40;
/// Searches whose top hits are tried after the favorite tracks.
const SEARCHES: [&str; 3] = [
  "Daft Punk Random Access Memories",
  "Fleetwood Mac Rumours",
  "Taylor Swift",
];

/// `text` with every URL's query replaced, so no CDN token is printed.
fn redact(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  // Where the current token (a run without quotes, brackets or spaces)
  // starts in `out`, and whether its query is being dropped.
  let mut token_start = 0usize;
  let mut in_query = false;
  for c in text.chars() {
    let boundary = c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace();
    if boundary {
      in_query = false;
      out.push(c);
      token_start = out.len();
      continue;
    }
    if in_query {
      continue;
    }
    if c == '?' && out[token_start..].contains("://") {
      out.push_str("?<redacted>");
      in_query = true;
      continue;
    }
    out.push(c);
  }
  out
}

/// The value of attribute `name` on the first element that has it.
fn attr(xml: &str, name: &str) -> Option<String> {
  let key = format!(" {name}=\"");
  let start = xml.find(&key)? + key.len();
  let end = xml[start..].find('"')? + start;
  Some(xml[start..end].replace("&amp;", "&"))
}

fn be(buf: &[u8], at: usize, n: usize) -> Option<u64> {
  let bytes = buf.get(at..at + n)?;
  Some(bytes.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64))
}

/// Print the box tree of `buf`, descending into the boxes that hold boxes.
fn print_boxes(buf: &[u8], depth: usize) {
  let mut pos = 0usize;
  while pos + 8 <= buf.len() {
    let size32 = be(buf, pos, 4).unwrap() as usize;
    let kind = String::from_utf8_lossy(&buf[pos + 4..pos + 8]).to_string();
    let (header, size) = match size32 {
      0 => (8, buf.len() - pos),
      1 => (16, be(buf, pos + 8, 8).unwrap_or(0) as usize),
      n => (8, n),
    };
    let indent = "  ".repeat(depth + 1);
    let truncated = size < header || pos + size > buf.len();
    let end = if truncated { buf.len() } else { pos + size };
    let body = &buf[(pos + header).min(end)..end];
    let mut note = String::new();
    match kind.as_str() {
      "mdat" => {
        note = format!(
          "payload {} bytes, starts {}",
          size.saturating_sub(header),
          body
            .iter()
            .take(4)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
        );
      }
      "dfLa" => {
        // Version and flags, then FLAC metadata blocks; STREAMINFO first.
        let info = body.get(4 + 4..);
        if let Some(info) = info.filter(|i| i.len() >= 18) {
          let rate = (be(info, 10, 3).unwrap() >> 4) as u32;
          let bits = ((be(info, 12, 2).unwrap() >> 4) & 0x1f) as u8 + 1;
          let samples = be(info, 13, 5).unwrap() & 0xf_ffff_ffff;
          note = format!(
            "{} bytes of blocks, first block type {}, {rate} Hz, {bits} bit, {samples} samples",
            body.len().saturating_sub(4),
            body[4] & 0x7f
          );
        }
      }
      "sidx" => {
        let version = body.first().copied().unwrap_or(0);
        let at = if version == 0 {
          4 + 4 + 4 + 8 + 2
        } else {
          4 + 4 + 4 + 16 + 2
        };
        note = format!("{:?} references", be(body, at, 2));
      }
      "trun" => note = format!("{:?} samples", be(body, 4, 4)),
      _ => {}
    }
    println!(
      "{indent}{kind} {size}{}{}",
      if truncated { " (truncated)" } else { "" },
      if note.is_empty() {
        String::new()
      } else {
        format!(": {note}")
      }
    );
    let children = match kind.as_str() {
      "moov" | "trak" | "mdia" | "minf" | "stbl" | "mvex" | "moof" | "traf" | "edts" | "dinf"
      | "udta" => Some(0),
      // Version, flags and entry count.
      "stsd" => Some(8),
      // The audio sample entry's fixed fields.
      "fLaC" | "mp4a" => Some(28),
      _ => None,
    };
    if let Some(skip) = children {
      if body.len() > skip {
        print_boxes(&body[skip..], depth + 1);
      }
    }
    if truncated {
      return;
    }
    pos += size;
  }
}

async fn fetch(http: &Client, url: &str, range: Option<&str>) -> (u16, String, Vec<u8>) {
  let mut request = http.get(url);
  if let Some(range) = range {
    request = request.header(RANGE, range);
  }
  let response = request
    .send()
    .await
    .map_err(|e| e.without_url())
    .expect("request");
  let status = response.status().as_u16();
  let headers = format!(
    "content-length {:?}, content-range {:?}",
    response.headers().get(CONTENT_LENGTH),
    response.headers().get(CONTENT_RANGE)
  );
  let body = response
    .bytes()
    .await
    .map_err(|e| e.without_url())
    .expect("body");
  (status, headers, body.to_vec())
}

async fn probe_dash(http: &Client, mpd: &str) {
  println!("--- MPD ---\n{}", redact(mpd));
  let init = attr(mpd, "initialization").expect("an initialization URL");
  let media = attr(mpd, "media").expect("a media template");
  let start: u64 = attr(mpd, "startNumber")
    .and_then(|s| s.parse().ok())
    .unwrap_or(1);
  let first = media.replace("$Number$", &start.to_string());

  let (status, headers, body) = fetch(http, &init, None).await;
  println!("--- init: HTTP {status}, {headers}, {} bytes", body.len());
  print_boxes(&body, 0);

  let (status, headers, body) = fetch(http, &first, Some("bytes=0-4095")).await;
  println!(
    "--- segment {start}, Range bytes=0-4095: HTTP {status}, {headers}, {} bytes",
    body.len()
  );
  let (status, headers, body) = fetch(http, &first, None).await;
  println!(
    "--- segment {start}, whole: HTTP {status}, {headers}, {} bytes",
    body.len()
  );
  print_boxes(&body, 0);
}

async fn probe_bts(http: &Client, url: &str) {
  let (status, headers, body) = fetch(http, url, Some("bytes=0-65535")).await;
  println!("--- BTS first 64 KiB: HTTP {status}, {headers}");
  print_boxes(&body, 0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a saved Tidal login, a client ID and the network"]
async fn live_tidal_probe() {
  let mut config = crate::core::user_config::UserConfig::new();
  config.load_config().expect("config.yml");
  let client = auth::client_credentials(&config.behavior).expect("a Tidal client ID");
  let source = TidalSource::new(restore_login(client).await.expect("a saved login"));
  let http = Client::builder()
    .user_agent(super::USER_AGENT)
    .build()
    .unwrap();

  let mut tracks = source.tracks(FAVORITES_URI).await.expect("favorite tracks");
  println!("{} favorite tracks", tracks.len());
  // Catalog hits from albums with hi-res masters, for an empty library.
  for query in SEARCHES {
    let results = source.search(query).await.expect("search");
    tracks.extend(results.tracks.into_iter().take(5));
  }
  let (mut dash_done, mut bts_done) = (false, false);
  for track in tracks.iter().take(MAX_TRACKS) {
    let Some(id) = track.uri.as_deref().and_then(|u| track_id_from_uri(u).ok()) else {
      continue;
    };
    let info: PlaybackInfo = match source
      .client
      .get_json(
        &manifest::playback_info_path(id),
        &manifest::playback_info_params("HI_RES_LOSSLESS"),
      )
      .await
    {
      Ok(info) => info,
      Err(e) => {
        println!("{}: {e:#}", track.name);
        continue;
      }
    };
    println!(
      "{} - {}: {} as {}",
      track.name,
      track.artists.join(", "),
      info.audio_quality,
      info.manifest_mime_type
    );
    let raw = base64::engine::general_purpose::STANDARD
      .decode(info.manifest.trim())
      .expect("a base64 manifest");
    if info.manifest_mime_type.contains("dash+xml") && !dash_done {
      dash_done = true;
      probe_dash(&http, &String::from_utf8_lossy(&raw)).await;
    } else if info.manifest_mime_type.contains("vnd.tidal.bts") && !bts_done {
      bts_done = true;
      let doc: serde_json::Value = serde_json::from_slice(&raw).expect("a BTS manifest");
      println!(
        "--- BTS manifest: mimeType {}, codecs {}, encryptionType {}",
        doc["mimeType"], doc["codecs"], doc["encryptionType"]
      );
      let url = doc["urls"][0].as_str().expect("a URL").to_string();
      probe_bts(&http, &url).await;
    }
    if dash_done && bts_done {
      break;
    }
  }
  println!("found DASH: {dash_done}, found BTS: {bts_done}");
}

#[test]
fn redact_drops_url_queries_only() {
  assert_eq!(
    redact(r#"<?xml version='1.0'?><S media="https://cdn/t/$Number$.mp4?token=x&amp;y=1" d="2"/>"#),
    r#"<?xml version='1.0'?><S media="https://cdn/t/$Number$.mp4?<redacted>" d="2"/>"#
  );
}
