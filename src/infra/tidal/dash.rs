//! The hi-res (MPEG-DASH) delivery, pure over `&str` and `&[u8]`.
//!
//! A HI_RES_LOSSLESS manifest is an MPD with one FLAC representation, whose
//! `SegmentTemplate` names an init segment and numbered media segments. The
//! init segment is a plain fragmented-MP4 `moov` whose `fLaC` sample entry
//! carries a `dfLa` box: the FLAC metadata blocks, STREAMINFO first. Each
//! media segment is a `moof` and an `mdat` whose payload is bare FLAC frames.
//!
//! The track is rebuilt as a raw FLAC file (`fLaC`, the metadata blocks, then
//! every segment's `mdat` payload in order), which the FLAC demuxer reads and
//! seeks without scanning the whole file the way the MP4 demuxer does.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// The segments of a DASH manifest, in play order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashManifest {
  pub init_url: String,
  pub segment_urls: Vec<String>,
}

#[derive(Deserialize)]
struct Mpd {
  #[serde(rename = "Period", default)]
  periods: Vec<Period>,
}

#[derive(Deserialize)]
struct Period {
  #[serde(rename = "AdaptationSet", default)]
  adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Deserialize)]
struct AdaptationSet {
  #[serde(rename = "@codecs")]
  codecs: Option<String>,
  #[serde(rename = "SegmentTemplate")]
  segment_template: Option<SegmentTemplate>,
  #[serde(rename = "Representation", default)]
  representations: Vec<Representation>,
}

#[derive(Deserialize)]
struct Representation {
  #[serde(rename = "@codecs")]
  codecs: Option<String>,
  #[serde(rename = "SegmentTemplate")]
  segment_template: Option<SegmentTemplate>,
}

#[derive(Deserialize)]
struct SegmentTemplate {
  #[serde(rename = "@initialization")]
  initialization: Option<String>,
  #[serde(rename = "@media")]
  media: Option<String>,
  #[serde(rename = "@startNumber")]
  start_number: Option<u64>,
  #[serde(rename = "SegmentTimeline")]
  timeline: Option<SegmentTimeline>,
}

#[derive(Deserialize)]
struct SegmentTimeline {
  #[serde(rename = "S", default)]
  entries: Vec<TimelineEntry>,
}

#[derive(Deserialize)]
struct TimelineEntry {
  /// Additional repeats of this entry; negative ("until the end") is not
  /// valid in a static manifest.
  #[serde(rename = "@r", default)]
  repeat: i64,
}

/// Upper bound on the segments of one track: hours of audio at Tidal's
/// four-second segments, and a cap on what a hostile manifest can allocate.
const MAX_SEGMENTS: u64 = 10_000;

/// Parse an MPD into the init URL and every media segment URL. The first
/// representation with a usable template wins (Tidal sends exactly one); a
/// template on the adaptation set applies to representations without one.
pub fn parse_mpd(xml: &str) -> Result<DashManifest> {
  let mpd: Mpd = quick_xml::de::from_str(xml).context("parsing the DASH manifest")?;
  for set in mpd.periods.iter().flat_map(|p| &p.adaptation_sets) {
    for rep in &set.representations {
      let Some(template) = rep
        .segment_template
        .as_ref()
        .or(set.segment_template.as_ref())
        .filter(|t| t.media.as_deref().is_some_and(|m| !m.is_empty()))
      else {
        continue;
      };
      let codecs = rep.codecs.as_deref().or(set.codecs.as_deref());
      if let Some(codecs) = codecs.filter(|c| !c.eq_ignore_ascii_case("flac")) {
        return Err(anyhow!("the DASH stream is {codecs}, not FLAC"));
      }
      return expand(template);
    }
  }
  Err(anyhow!("the DASH manifest has no usable segment template"))
}

fn expand(template: &SegmentTemplate) -> Result<DashManifest> {
  let media = template.media.as_deref().unwrap_or_default();
  if !media.contains("$Number$") {
    return Err(anyhow!("unsupported DASH media template"));
  }
  let init_url = template
    .initialization
    .clone()
    .filter(|u| !u.is_empty())
    .ok_or_else(|| anyhow!("the DASH manifest has no init segment"))?;
  let mut count = 0u64;
  for entry in template.timeline.iter().flat_map(|t| &t.entries) {
    let repeat = u64::try_from(entry.repeat)
      .map_err(|_| anyhow!("unsupported DASH timeline repeat {}", entry.repeat))?;
    count = count.saturating_add(1).saturating_add(repeat);
  }
  if count == 0 {
    return Err(anyhow!("the DASH segment timeline is empty"));
  }
  if count > MAX_SEGMENTS {
    return Err(anyhow!("the DASH manifest lists {count} segments"));
  }
  let start = template.start_number.unwrap_or(1);
  let segment_urls = (start..start + count)
    .map(|n| media.replace("$Number$", &n.to_string()))
    .collect();
  Ok(DashManifest {
    init_url,
    segment_urls,
  })
}

/// The format of a FLAC stream, read from its STREAMINFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlacFormat {
  pub sample_rate: u32,
  pub bits_per_sample: u8,
}

impl FlacFormat {
  /// Playbar label: `FLAC 24/96`, `FLAC 16/44.1`.
  pub fn label(&self) -> String {
    let khz = if self.sample_rate.is_multiple_of(1000) {
      (self.sample_rate / 1000).to_string()
    } else {
      format!("{:.1}", self.sample_rate as f64 / 1000.0)
    };
    format!("FLAC {}/{khz}", self.bits_per_sample)
  }
}

/// The parsed init segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlacInit {
  /// `fLaC` and the metadata blocks, the last one flagged as last.
  pub header: Vec<u8>,
  pub format: FlacFormat,
}

/// One box: its type and the bytes after its header.
struct Mp4Box<'a> {
  kind: [u8; 4],
  body: &'a [u8],
}

/// The top-level boxes of `buf`, each of which must fit.
fn boxes(buf: &[u8]) -> Result<Vec<Mp4Box<'_>>> {
  let mut out = Vec::new();
  let mut pos = 0usize;
  while pos < buf.len() {
    let header = buf
      .get(pos..pos + 8)
      .ok_or_else(|| anyhow!("box header truncated at byte {pos}"))?;
    let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
    let kind: [u8; 4] = header[4..8].try_into().unwrap();
    let remaining = buf.len() - pos;
    let (header_len, size) = match size32 {
      0 => (8, remaining),
      1 => {
        let large = buf
          .get(pos + 8..pos + 16)
          .ok_or_else(|| anyhow!("box header truncated at byte {pos}"))?;
        let large = u64::from_be_bytes(large.try_into().unwrap());
        (16, usize::try_from(large).unwrap_or(usize::MAX))
      }
      n => (8, n as usize),
    };
    if size < header_len || size > remaining {
      return Err(anyhow!("invalid box size {size} at byte {pos}"));
    }
    out.push(Mp4Box {
      kind,
      body: &buf[pos + header_len..pos + size],
    });
    pos += size;
  }
  Ok(out)
}

/// The body of the first child of type `kind`, after `skip` bytes of the
/// parent's own fields.
fn child<'a>(parent: &'a [u8], skip: usize, kind: &[u8; 4]) -> Result<&'a [u8]> {
  let children = parent
    .get(skip..)
    .ok_or_else(|| anyhow!("{} box truncated", String::from_utf8_lossy(kind)))?;
  boxes(children)?
    .into_iter()
    .find(|b| &b.kind == kind)
    .map(|b| b.body)
    .ok_or_else(|| anyhow!("no {} box", String::from_utf8_lossy(kind)))
}

/// Parse the init segment into the FLAC header and format.
pub fn parse_init(buf: &[u8]) -> Result<FlacInit> {
  let mut body = buf;
  for kind in [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"] {
    body = child(body, 0, kind)?;
  }
  // stsd: version, flags and entry count; the sample entry: 28 bytes of
  // audio fields; dfLa: version and flags.
  let entry = child(body, 8, b"fLaC").context("the init segment is not FLAC")?;
  let dfla = child(entry, 28, b"dfLa")?;
  let blocks = dfla.get(4..).ok_or_else(|| anyhow!("dfLa box truncated"))?;
  let mut header = b"fLaC".to_vec();
  header.extend_from_slice(blocks);
  let format = mark_last_block(&mut header)?;
  Ok(FlacInit { header, format })
}

/// Flag the final metadata block as last (dropping anything after a block
/// already flagged so) and read the format from STREAMINFO.
fn mark_last_block(header: &mut Vec<u8>) -> Result<FlacFormat> {
  let info = header
    .get(4..42)
    .filter(|b| b[0] & 0x7f == 0 && b[1..4] == [0, 0, 34])
    .ok_or_else(|| anyhow!("the FLAC header does not start with STREAMINFO"))?;
  // STREAMINFO after the block header: sizes (10 bytes), then 20 bits of
  // sample rate, 3 of channels, 5 of bits per sample.
  let packed = u32::from_be_bytes(info[14..18].try_into().unwrap());
  let format = FlacFormat {
    sample_rate: packed >> 12,
    bits_per_sample: ((packed >> 4) & 0x1f) as u8 + 1,
  };
  let mut pos = 4usize;
  loop {
    let block = header
      .get(pos..pos + 4)
      .ok_or_else(|| anyhow!("FLAC header truncated at byte {pos}"))?;
    let len = u32::from_be_bytes([0, block[1], block[2], block[3]]) as usize;
    let next = pos + 4 + len;
    if next > header.len() {
      return Err(anyhow!("FLAC metadata block at byte {pos} is truncated"));
    }
    if block[0] & 0x80 != 0 {
      header.truncate(next);
      return Ok(format);
    }
    if next == header.len() {
      header[pos] |= 0x80;
      return Ok(format);
    }
    pos = next;
  }
}

/// Where a media segment's FLAC frames are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadLayout {
  /// The `mdat` payload: its offset in the segment and its length.
  Found { offset: u64, len: u64 },
  /// The prefix ends before the `mdat` header: read this many bytes.
  NeedBytes(u64),
}

/// Find the `mdat` payload from a prefix of a media segment. `total` is the
/// segment's full size, needed only when the `mdat` runs to the end.
pub fn media_payload(prefix: &[u8], total: Option<u64>) -> Result<PayloadLayout> {
  let len = prefix.len() as u64;
  // Ask for enough to read a 64-bit box size at `pos`, unless the segment
  // is already all here.
  let need = |pos: u64| {
    let want = total.map_or(pos + 16, |t| (pos + 16).min(t));
    if want <= len {
      Err(anyhow!("the segment is truncated at byte {pos}"))
    } else {
      Ok(PayloadLayout::NeedBytes(want))
    }
  };
  let mut pos = 0u64;
  loop {
    if total.is_some_and(|t| pos >= t) {
      return Err(anyhow!("the segment has no mdat box"));
    }
    if pos + 8 > len {
      return need(pos);
    }
    let at = pos as usize;
    let size32 = u32::from_be_bytes(prefix[at..at + 4].try_into().unwrap());
    let kind = &prefix[at + 4..at + 8];
    let (header_len, size) = match size32 {
      0 => {
        let total = total.ok_or_else(|| anyhow!("an open-ended box needs the segment size"))?;
        (8, total - pos)
      }
      1 if pos + 16 > len => return need(pos),
      1 => (
        16,
        u64::from_be_bytes(prefix[at + 8..at + 16].try_into().unwrap()),
      ),
      n => (8, u64::from(n)),
    };
    if size < header_len {
      return Err(anyhow!("invalid box size {size} at byte {pos}"));
    }
    let end = pos
      .checked_add(size)
      .ok_or_else(|| anyhow!("box size overflow at byte {pos}"))?;
    if let Some(total) = total.filter(|&t| end > t) {
      return Err(anyhow!(
        "box at byte {pos} runs past the {total}-byte segment"
      ));
    }
    if kind == b"mdat" {
      return Ok(PayloadLayout::Found {
        offset: pos + header_len,
        len: size - header_len,
      });
    }
    pos = end;
  }
}

/// Builders for DASH test data in the live layout, shared with the segment
/// download tests.
#[cfg(test)]
pub(super) mod fixtures {
  pub fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
  }

  /// STREAMINFO for 48 kHz, stereo, 24 bit, as a metadata block (not last).
  pub fn streaminfo_block() -> Vec<u8> {
    let mut block = vec![0x00, 0, 0, 34];
    block.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]); // block sizes
    block.extend_from_slice(&[0; 6]); // frame sizes
                                      // 48000 Hz (20 bits), 2 channels - 1 (3 bits), 24 bits - 1 (5 bits),
                                      // then 36 bits of total samples.
    let packed: u64 = (48_000u64 << 44) | (1 << 41) | (23 << 36) | 10_851_543;
    block.extend_from_slice(&packed.to_be_bytes());
    block.extend_from_slice(&[0xab; 16]); // MD5
    block
  }

  /// An init segment whose `dfLa` holds `blocks`.
  pub fn init_segment(blocks: &[u8]) -> Vec<u8> {
    let mut dfla = vec![0, 0, 0, 0];
    dfla.extend_from_slice(blocks);
    let mut entry = vec![0u8; 28];
    entry.extend(mp4_box(b"dfLa", &dfla));
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(mp4_box(b"fLaC", &entry));
    let mut stbl = mp4_box(b"stsd", &stsd);
    stbl.extend(mp4_box(b"stts", &[0; 8]));
    let minf = [mp4_box(b"smhd", &[0; 8]), mp4_box(b"stbl", &stbl)].concat();
    let mdia = [mp4_box(b"mdhd", &[0; 24]), mp4_box(b"minf", &minf)].concat();
    let trak = [mp4_box(b"tkhd", &[0; 84]), mp4_box(b"mdia", &mdia)].concat();
    let moov = [mp4_box(b"mvhd", &[0; 100]), mp4_box(b"trak", &trak)].concat();
    [mp4_box(b"ftyp", b"iso6"), mp4_box(b"moov", &moov)].concat()
  }

  /// A media segment: a `moof` of `moof_len` bytes, then an `mdat` of `frames`.
  pub fn media_segment(moof_len: usize, frames: &[u8]) -> Vec<u8> {
    [
      mp4_box(b"moof", &vec![0; moof_len - 8]),
      mp4_box(b"mdat", frames),
    ]
    .concat()
  }
}

#[cfg(test)]
mod tests {
  use super::fixtures::*;
  use super::*;

  /// The live manifest's shape (2026-10), with placeholder URLs.
  const LIVE_MPD: &str = r#"<?xml version='1.0' encoding='UTF-8'?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-main:2011" type="static" mediaPresentationDuration="PT3M46.073S"><Period id="0"><AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" lang="und" segmentAlignment="true"><Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/><Representation id="FLAC_HIRES,48000,24" codecs="flac" bandwidth="1738594" audioSamplingRate="48000"><AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2"/><SegmentTemplate timescale="48000" initialization="https://cdn/t/0.mp4?a=1&amp;b=2" media="https://cdn/t/$Number$.mp4?a=1&amp;b=2" startNumber="1"><SegmentTimeline><S d="188416" r="56"/><S d="111831"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#;

  #[test]
  fn the_live_manifest_lists_its_init_and_every_segment() {
    let manifest = parse_mpd(LIVE_MPD).unwrap();
    assert_eq!(manifest.init_url, "https://cdn/t/0.mp4?a=1&b=2");
    assert_eq!(manifest.segment_urls.len(), 58);
    assert_eq!(manifest.segment_urls[0], "https://cdn/t/1.mp4?a=1&b=2");
    assert_eq!(manifest.segment_urls[57], "https://cdn/t/58.mp4?a=1&b=2");
  }

  fn mpd(set_attrs: &str, set_template: &str, rep: &str) -> String {
    format!(
      r#"<MPD><Period><AdaptationSet {set_attrs}>{set_template}{rep}</AdaptationSet></Period></MPD>"#
    )
  }

  const TEMPLATE: &str = r#"<SegmentTemplate initialization="i" media="m$Number$" startNumber="5"><SegmentTimeline><S d="1" r="1"/><S d="2"/></SegmentTimeline></SegmentTemplate>"#;

  #[test]
  fn a_template_on_the_adaptation_set_applies_to_its_representation() {
    let xml = mpd(r#"codecs="flac""#, TEMPLATE, "<Representation/>");
    let manifest = parse_mpd(&xml).unwrap();
    assert_eq!(manifest.init_url, "i");
    assert_eq!(manifest.segment_urls, ["m5", "m6", "m7"]);
  }

  #[test]
  fn the_start_number_defaults_to_one() {
    let template = TEMPLATE.replace(r#" startNumber="5""#, "");
    let xml = mpd(
      "",
      "",
      &format!("<Representation>{template}</Representation>"),
    );
    assert_eq!(parse_mpd(&xml).unwrap().segment_urls, ["m1", "m2", "m3"]);
  }

  #[test]
  fn a_stream_that_is_not_flac_is_refused() {
    let xml = mpd(
      "",
      "",
      &format!(r#"<Representation codecs="mp4a.40.2">{TEMPLATE}</Representation>"#),
    );
    let err = parse_mpd(&xml).unwrap_err();
    assert!(err.to_string().contains("not FLAC"), "{err:#}");
  }

  #[test]
  fn unusable_templates_are_errors() {
    let cases = [
      // No template anywhere.
      mpd("", "", "<Representation/>"),
      // No timeline.
      mpd(
        "",
        r#"<SegmentTemplate initialization="i" media="m$Number$"/>"#,
        "<Representation/>",
      ),
      // A $Time$ template.
      mpd(
        "",
        &TEMPLATE.replace("$Number$", "$Time$"),
        "<Representation/>",
      ),
      // No init segment.
      mpd(
        "",
        &TEMPLATE.replace(r#"initialization="i" "#, ""),
        "<Representation/>",
      ),
      // An open-ended repeat.
      mpd(
        "",
        &TEMPLATE.replace(r#"r="1""#, r#"r="-1""#),
        "<Representation/>",
      ),
      // More segments than any track has.
      mpd(
        "",
        &TEMPLATE.replace(r#"r="1""#, r#"r="99999999""#),
        "<Representation/>",
      ),
      "not xml at all".to_string(),
    ];
    for xml in cases {
      assert!(parse_mpd(&xml).is_err(), "{xml}");
    }
  }

  #[test]
  fn the_init_segment_yields_a_flac_header_and_format() {
    let init = parse_init(&init_segment(&streaminfo_block())).unwrap();
    assert_eq!(&init.header[..4], b"fLaC");
    assert_eq!(init.header[4], 0x80, "STREAMINFO is flagged as last");
    assert_eq!(init.header[5..], streaminfo_block()[1..]);
    assert_eq!(
      init.format,
      FlacFormat {
        sample_rate: 48_000,
        bits_per_sample: 24
      }
    );
    assert_eq!(init.format.label(), "FLAC 24/48");
  }

  #[test]
  fn only_the_last_of_several_metadata_blocks_is_flagged() {
    let mut blocks = streaminfo_block();
    blocks.extend_from_slice(&[0x04, 0, 0, 2, 0xaa, 0xbb]); // VORBIS_COMMENT
    let header = parse_init(&init_segment(&blocks)).unwrap().header;
    assert_eq!(header[4], 0x00);
    assert_eq!(header[42], 0x84);
    assert_eq!(header.len(), 4 + blocks.len());
  }

  #[test]
  fn blocks_after_one_flagged_last_are_dropped() {
    let mut blocks = streaminfo_block();
    blocks[0] = 0x80;
    blocks.extend_from_slice(&[0x04, 0, 0, 2, 0xaa, 0xbb]);
    let header = parse_init(&init_segment(&blocks)).unwrap().header;
    assert_eq!(header.len(), 42);
  }

  #[test]
  fn an_init_segment_without_flac_is_an_error() {
    let mut no_dfla = init_segment(&streaminfo_block());
    let at = no_dfla.windows(4).position(|w| w == b"dfLa").unwrap();
    no_dfla[at..at + 4].copy_from_slice(b"free");
    assert!(parse_init(&no_dfla).is_err());
    let no_streaminfo = init_segment(&[0x04, 0, 0, 2, 0xaa, 0xbb]);
    assert!(parse_init(&no_streaminfo).is_err());
  }

  #[test]
  fn a_truncated_init_segment_is_an_error_without_panic() {
    let full = init_segment(&streaminfo_block());
    for cut in 0..full.len() {
      assert!(parse_init(&full[..cut]).is_err(), "prefix of {cut} bytes");
    }
  }

  #[test]
  fn sample_rates_label_like_the_quality_setting() {
    let label = |sample_rate, bits_per_sample| {
      FlacFormat {
        sample_rate,
        bits_per_sample,
      }
      .label()
    };
    assert_eq!(label(44_100, 16), "FLAC 16/44.1");
    assert_eq!(label(96_000, 24), "FLAC 24/96");
    assert_eq!(label(192_000, 24), "FLAC 24/192");
  }

  #[test]
  fn the_payload_follows_the_moof() {
    let segment = media_segment(280, &[0xff, 0xf8, 1, 2, 3]);
    let found = PayloadLayout::Found {
      offset: 288,
      len: 5,
    };
    assert_eq!(media_payload(&segment, None).unwrap(), found);
    // The header alone is enough, with or without the segment size.
    assert_eq!(media_payload(&segment[..288], None).unwrap(), found);
    assert_eq!(
      media_payload(&segment[..288], Some(segment.len() as u64)).unwrap(),
      found
    );
  }

  #[test]
  fn a_short_prefix_asks_for_the_bytes_that_reach_the_mdat_header() {
    let segment = media_segment(2000, &[1; 10]);
    for cut in [0, 4, 100, 1999, 2007] {
      let layout = media_payload(&segment[..cut], Some(segment.len() as u64)).unwrap();
      let PayloadLayout::NeedBytes(need) = layout else {
        panic!("{cut}: {layout:?}");
      };
      assert!(need as usize > cut, "{cut}: {need}");
      assert!(media_payload(&segment[..need as usize], None).is_ok());
    }
  }

  #[test]
  fn an_open_ended_or_large_mdat_is_measured() {
    let mut open = media_segment(16, &[]);
    open[16..20].copy_from_slice(&0u32.to_be_bytes());
    open.extend_from_slice(&[9; 7]);
    assert_eq!(
      media_payload(&open, Some(open.len() as u64)).unwrap(),
      PayloadLayout::Found { offset: 24, len: 7 }
    );
    assert!(media_payload(&open, None).is_err());

    let mut large = fixtures::mp4_box(b"moof", &[0; 8]);
    large.extend_from_slice(&1u32.to_be_bytes());
    large.extend_from_slice(b"mdat");
    large.extend_from_slice(&(16u64 + 3).to_be_bytes());
    large.extend_from_slice(&[7; 3]);
    assert_eq!(
      media_payload(&large, None).unwrap(),
      PayloadLayout::Found { offset: 32, len: 3 }
    );
  }

  #[test]
  fn a_segment_without_an_mdat_is_an_error() {
    let segment = fixtures::mp4_box(b"moof", &[0; 20]);
    assert!(media_payload(&segment, Some(segment.len() as u64)).is_err());
    let mut bad = fixtures::mp4_box(b"moof", &[0; 20]);
    bad[..4].copy_from_slice(&3u32.to_be_bytes());
    assert!(media_payload(&bad, None).is_err());
    let mut past = media_segment(16, &[1; 4]);
    past[16..20].copy_from_slice(&100u32.to_be_bytes());
    assert!(media_payload(&past, Some(past.len() as u64)).is_err());
  }
}
