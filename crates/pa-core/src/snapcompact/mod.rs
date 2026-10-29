//! Local, lossy text-to-image compaction. No model call, network or filesystem
//! access occurs here. The host owns durable acceptance and asset lifetime.
//! Source and renderer attribution: `assets/licenses/NOTICE` and `assets/licenses/UPSTREAM-MIT`.
mod renderer;
mod serialize;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use pa_types::session::AgentMessage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, io::Cursor, ops::Range};
use thiserror::Error;

pub use serialize::{serialize_conversation, SerializeOptions};

const MAX_SOURCE_BYTES: usize = 4_000_000;
const MAX_ARCHIVE_BYTES: usize = 24_000_000;
const LEAD: &str = "Snapcompact bitmap archive. This is bounded, lossy conversation text, not a lossless memory. Read images in order between the oldest and newest text.\n";
const FALLBACK_NOTICE: &str =
    "[Text fallback: this model cannot read the bitmap frames; bounded, lossy source text only, not a lossless memory]\n";

/// Errors distinguish an intentional decline from cancellation and bad data.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Snapcompact declined: {0}")]
    Declined(String),
    #[error("Snapcompact budget: {0}")]
    Budget(String),
    #[error("Snapcompact cancelled")]
    Cancelled,
    #[error("Invalid Snapcompact archive: {0}")]
    InvalidArchive(String),
    #[error("Snapcompact rendering failed: {0}")]
    Render(String),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Profiles are chosen from the model lineage, not a gateway's wire protocol.
/// Unknown models require an explicit audited profile; there is no guessed default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderProfile {
    Claude,
    OpenAi,
    Google,
    Explicit {
        frame_size: u32,
        frame_tokens: usize,
    },
}
impl RenderProfile {
    fn geometry(self, dense: bool) -> Result<Geometry> {
        let (size, tokens, cell_width, cell_height) = match self {
            Self::Claude => (1568, 5024, 11, 16),
            Self::OpenAi => (1568, 5024, 8, 22),
            Self::Google => (2048, 5024, 8, 22),
            Self::Explicit {
                frame_size,
                frame_tokens,
            } => (frame_size, frame_tokens, 8, 22),
        };
        if !(64..=2576).contains(&size) || tokens == 0 || tokens > 100_000 {
            return Err(Error::Declined("invalid explicit render profile".into()));
        }
        Ok(Geometry {
            width: size,
            cell_width: if dense { 8 } else { cell_width },
            cell_height: if dense { 8 } else { cell_height },
            dense,
            token_estimate: tokens,
        })
    }
}

/// Limits must be resolved again for every send, including model switches.
/// `available_tokens` excludes system, retained history, tools and output reserve.
/// Payload bytes include base64 image expansion, source and JSON overhead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayLimits {
    pub vision: bool,
    pub profile: Option<RenderProfile>,
    pub max_images: usize,
    pub max_image_bytes: usize,
    pub max_payload_bytes: usize,
    pub available_tokens: usize,
    /// Optional tighter cap on the automatic text fallback. Replay produces the
    /// fallback for incompatible readers without it; this only bounds it further.
    pub text_fallback_chars: Option<usize>,
}

/// Archive construction policy. Hard ceilings still apply to caller values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub max_frames: usize,
    pub max_source_bytes: usize,
    pub serialize: SerializeOptions,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_frames: 80,
            max_source_bytes: 1_000_000,
            serialize: SerializeOptions::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SourceSpan {
    start: u64,
    text: String,
    dim_start: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Geometry {
    width: u32,
    cell_width: u32,
    cell_height: u32,
    dense: bool,
    token_estimate: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Frame {
    ordinal: usize,
    geometry: Geometry,
    height: u32,
    png: String,
    sha256: String,
    omitted_before: u64,
}

/// Immutable proposal data. No persisted authority is implied by its existence.
/// Only bounded portable text and locally rendered PNGs enter this representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archive {
    version: u32,
    source: Vec<SourceSpan>,
    text_head: String,
    text_tail: String,
    frames: Vec<Frame>,
    total_chars: u64,
    truncated_chars: u64,
}
impl Archive {
    /// Serialize validated proposal data. The host decides where to store it.
    /// # Errors
    /// Returns `InvalidArchive` for inconsistent source, frames or PNG content.
    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        serde_json::to_string(self).map_err(|e| Error::InvalidArchive(e.to_string()))
    }
    /// Restore bounded archive data without reading assets or contacting a provider.
    /// # Errors
    /// Rejects oversized, malformed, inconsistent or corrupted archives.
    pub fn from_json(json: &str) -> Result<Self> {
        if json.len() > MAX_ARCHIVE_BYTES {
            return Err(Error::InvalidArchive("serialized size".into()));
        }
        let archive: Self =
            serde_json::from_str(json).map_err(|e| Error::InvalidArchive(e.to_string()))?;
        archive.validate()?;
        Ok(archive)
    }
    /// Total Unicode scalars after serializer filtering/bounds and normalization,
    /// including source lost to prior/current layout or source budgets.
    #[must_use]
    pub fn total_chars(&self) -> u64 {
        self.total_chars
    }
    /// Exact omitted normalized-source scalar count (not canonical history size).
    #[must_use]
    pub fn truncated_chars(&self) -> u64 {
        self.truncated_chars
    }
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }
    /// Bounded prior source with explicit chronological gap notices, for inspection.
    #[must_use]
    pub fn source_text(&self) -> String {
        view(&self.source, self.total_chars).0
    }

    fn validate(&self) -> Result<()> {
        let bad = |why: &str| Error::InvalidArchive(why.into());
        if self.version != 1 || self.frames.len() > 80 || self.source.len() > 512 {
            return Err(bad("version/count"));
        }
        let mut end = 0;
        let mut retained = 0_u64;
        let mut bytes = self.text_head.len() + self.text_tail.len();
        for span in &self.source {
            let chars = span.text.chars().count() as u64;
            if span.start < end || chars == 0 {
                return Err(bad("source order"));
            }
            end = span
                .start
                .checked_add(chars)
                .ok_or_else(|| bad("source overflow"))?;
            if end > self.total_chars {
                return Err(bad("source range"));
            }
            retained += chars;
            bytes += span.text.len();
        }
        if self.total_chars.checked_sub(retained) != Some(self.truncated_chars)
            || bytes > MAX_SOURCE_BYTES * 3
        {
            return Err(bad("source accounting"));
        }
        if self.source.iter().map(|s| s.text.len()).sum::<usize>() > MAX_SOURCE_BYTES
            || self.frames.iter().map(|f| f.png.len()).sum::<usize>() > MAX_ARCHIVE_BYTES / 2
        {
            return Err(bad("source/image hard bound"));
        }
        for (i, frame) in self.frames.iter().enumerate() {
            bytes += frame.png.len();
            let g = &frame.geometry;
            if frame.ordinal != i
                || !(64..=2576).contains(&g.width)
                || !(64..=2576).contains(&frame.height)
                || !matches!(
                    (g.cell_width, g.cell_height, g.dense),
                    (8, 8, true) | (11, 16, false) | (8, 22, false)
                )
                || g.token_estimate == 0
                || g.token_estimate > 100_000
            {
                return Err(bad("frame geometry/order"));
            }
            let png = STANDARD
                .decode(&frame.png)
                .map_err(|e| bad(&e.to_string()))?;
            if png.len() < 33
                || png[..8] != *b"\x89PNG\r\n\x1a\n"
                || digest(&png) != frame.sha256
                || u32::from_be_bytes(png[16..20].try_into().map_err(|_| bad("PNG width"))?)
                    != g.width
                || u32::from_be_bytes(png[20..24].try_into().map_err(|_| bad("PNG height"))?)
                    != frame.height
            {
                return Err(bad("PNG integrity"));
            }
            let mut decoder = png::Decoder::new(Cursor::new(&png));
            decoder.set_limits(png::Limits { bytes: 32_000_000 });
            let mut reader = decoder
                .read_info()
                .map_err(|e| bad(&format!("PNG decode: {e}")))?;
            if reader.info().width != g.width
                || reader.info().height != frame.height
                || reader.info().animation_control.is_some()
            {
                return Err(bad("PNG decoded geometry/animation"));
            }
            let len = reader
                .output_buffer_size()
                .ok_or_else(|| bad("PNG allocation overflow"))?;
            if len > g.width as usize * frame.height as usize * 4 {
                return Err(bad("PNG allocation bound"));
            }
            let mut pixels = vec![0; len];
            reader
                .next_frame(&mut pixels)
                .map_err(|e| bad(&format!("PNG pixels: {e}")))?;
            reader
                .finish()
                .map_err(|e| bad(&format!("PNG stream: {e}")))?;
        }
        if bytes > MAX_ARCHIVE_BYTES {
            return Err(bad("archive allocation bound"));
        }
        Ok(())
    }
}

/// Ordered reconstruction material. PNG bytes never contain native provider state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayBlock {
    Text(String),
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
        sha256: String,
    },
}

// JSON string byte count, conservative for control characters (six-byte \u).
fn json_text_bytes(text: &str) -> usize {
    2 + text
        .bytes()
        .map(|byte| match byte {
            b'"' | b'\\' => 2,
            0..=31 => 6,
            _ => 1,
        })
        .sum::<usize>()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn checkpoint(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

// View ranges refer to UTF-8 bytes; source offsets/counts refer to Unicode scalars.
// Generated gap markers do not count as retained original source characters.
fn view(spans: &[SourceSpan], total: u64) -> (String, Vec<(Range<usize>, &SourceSpan)>) {
    let mut text = String::new();
    let mut maps = Vec::new();
    let mut end = 0;
    let mut dim = false;
    for span in spans {
        if span.start > end {
            if dim {
                text.push('\u{f}');
                dim = false;
            }
            let _ = write!(text, "█[{} source chars omitted]█", span.start - end);
        }
        if dim != span.dim_start {
            text.push(if span.dim_start { '\u{e}' } else { '\u{f}' });
        }
        let start = text.len();
        text.push_str(&span.text);
        maps.push((start..text.len(), span));
        dim = span
            .text
            .chars()
            .rev()
            .find(|ch| matches!(ch, '\u{e}' | '\u{f}'))
            .map_or(span.dim_start, |ch| ch == '\u{e}');
        end = span.start + span.text.chars().count() as u64;
    }
    if total > end {
        if dim {
            text.push('\u{f}');
        }
        let _ = write!(text, "█[{} source chars omitted]█", total - end);
    }
    (text, maps)
}
fn retained_ranges(
    maps: &[(Range<usize>, &SourceSpan)],
    ranges: &[Range<usize>],
) -> Vec<SourceSpan> {
    let mut kept: Vec<SourceSpan> = Vec::new();
    for range in ranges {
        for (map, span) in maps {
            let start = range.start.max(map.start);
            let end = range.end.min(map.end);
            if start >= end {
                continue;
            }
            let offset = start - map.start;
            let original = span.start + span.text[..offset].chars().count() as u64;
            let text = &span.text[offset..end - map.start];
            if let Some(last) = kept.last_mut() {
                if last.start + last.text.chars().count() as u64 == original {
                    last.text.push_str(text);
                    continue;
                }
            }
            kept.push(SourceSpan {
                start: original,
                text: text.to_owned(),
                dim_start: span.text[..offset]
                    .chars()
                    .rev()
                    .find(|ch| matches!(ch, '\u{e}' | '\u{f}'))
                    .map_or(span.dim_start, |ch| ch == '\u{e}'),
            });
        }
    }
    kept
}
fn floor_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
fn char_cells(ch: char) -> usize {
    if matches!(ch, '\u{e}' | '\u{f}') {
        0
    } else if renderer::is_wide(ch as u32) {
        2
    } else {
        1
    }
}
fn pages(text: &str, geometry: &Geometry) -> Vec<Range<usize>> {
    let cols = (geometry.width / geometry.cell_width) as usize;
    let capacity = cols * (geometry.width / geometry.cell_height) as usize;
    let mut pages = Vec::new();
    let mut start = 0;
    let mut cells = 0;
    for (index, ch) in text.char_indices() {
        let units = char_cells(ch);
        let padding = usize::from(units == 2 && cells % cols == cols - 1);
        if cells + padding + units > capacity {
            pages.push(start..index);
            start = index;
            cells = 0;
        }
        cells += units + usize::from(units == 2 && cells % cols == cols - 1);
    }
    if start < text.len() {
        pages.push(start..text.len());
    }
    pages
}

/// Prepare a fresh immutable archive, re-rendering prior bounded source plus new
/// discarded history and split prefix. Old PNGs are never concatenated.
/// # Errors
/// Declines initial nonvision/unknown models. Returns budget, cancellation or
/// rendering errors without changing the prior archive or accepting a proposal.
pub fn prepare(
    prior: Option<&Archive>,
    messages: &[AgentMessage],
    split_prefix: &[AgentMessage],
    config: &Config,
    limits: &ReplayLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<Archive> {
    checkpoint(cancelled)?;
    if !limits.vision {
        return Err(Error::Declined("reader has no vision capability".into()));
    }
    let profile = limits
        .profile
        .ok_or_else(|| Error::Declined("reader needs an explicit render profile".into()))?;
    let high = profile.geometry(false)?;
    let low = profile.geometry(true)?;
    if let Some(old) = prior {
        old.validate()?;
    }
    let mut source = prior.map_or_else(Vec::new, |old| old.source.clone());
    let mut total = prior.map_or(0, |old| old.total_chars);
    serialize::check_input(messages, cancelled)?;
    serialize::check_input(split_prefix, cancelled)?;
    let history = serialize_conversation(messages, &config.serialize);
    let prefix = serialize_conversation(split_prefix, &config.serialize);
    let mut new = serialize::normalize(&format!("{history}\n{prefix}"));
    if total > 0 && !new.is_empty() {
        new.insert(0, '█');
    }
    if !new.is_empty() {
        source.push(SourceSpan {
            start: total,
            text: new,
            dim_start: false,
        });
        total = total
            .checked_add(source.last().map_or(0, |s| s.text.chars().count() as u64))
            .ok_or_else(|| Error::InvalidArchive("total source count overflow".into()))?;
    }
    checkpoint(cancelled)?;
    let (mut text, maps) = view(&source, total);
    let cap = config.max_source_bytes.clamp(256, MAX_SOURCE_BYTES);
    if text.len() > cap {
        let head = floor_boundary(&text, cap / 2);
        let tail = ceil_boundary(&text, text.len() - cap / 2);
        source = retained_ranges(&maps, &[0..head, tail..text.len()]);
        text = view(&source, total).0;
    }
    let (_, maps) = view(&source, total);
    let all = pages(&text, &high);
    let mut archive = Archive {
        version: 1,
        source: Vec::new(),
        text_head: String::new(),
        text_tail: String::new(),
        frames: Vec::new(),
        total_chars: total,
        truncated_chars: 0,
    };
    let mut kept = Vec::new();
    let edge_capacity =
        (high.width / high.cell_width) as usize * (high.width / high.cell_height) as usize;
    if text.chars().map(char_cells).sum::<usize>() <= 2 * edge_capacity {
        archive.text_head = serialize::plain(&text);
        kept.push(0..text.len());
    } else {
        let head = all[0].clone();
        let mut tail_cells = 0;
        let mut tail_start = text.len();
        for (index, ch) in text.char_indices().rev() {
            let units = char_cells(ch);
            if tail_cells + units > edge_capacity {
                break;
            }
            tail_cells += units;
            tail_start = index;
        }
        let tail = tail_start..text.len();
        archive.text_head = serialize::plain(&text[head.clone()]);
        archive.text_tail = serialize::plain(&text[tail.clone()]);
        kept.push(head.clone());
        let mid = head.end..tail.start;
        let hi_pages: Vec<_> = pages(&text[mid.clone()], &high)
            .into_iter()
            .map(|r| (r.start + mid.start..r.end + mid.start, high.clone()))
            .collect();
        let max = config.max_frames.clamp(1, 80).min(limits.max_images);
        let plans = if hi_pages.len() <= max {
            hi_pages
        } else {
            let edge = 3.min(max.saturating_sub(1) / 2);
            let dense_start = hi_pages.get(edge).map_or(mid.end, |p| p.0.start);
            let dense_end = hi_pages
                .get(hi_pages.len() - edge)
                .map_or(mid.end, |p| p.0.start);
            let dense = dense_start..dense_end;
            let low_pages: Vec<_> = pages(&text[dense.clone()], &low)
                .into_iter()
                .map(|r| (r.start + dense.start..r.end + dense.start, low.clone()))
                .collect();
            let budget = max.saturating_sub(2 * edge);
            let dropped = low_pages.len().saturating_sub(budget);
            hi_pages[..edge]
                .iter()
                .cloned()
                .chain(low_pages.into_iter().skip(dropped))
                .chain(hi_pages[hi_pages.len() - edge..].iter().cloned())
                .collect()
        };
        let mut image_bytes = 0;
        let mut rendered: Vec<(Frame, Range<usize>)> = Vec::new();
        for (range, geometry) in plans {
            checkpoint(cancelled)?;
            let dim = text[..range.start]
                .chars()
                .rev()
                .find(|ch| matches!(ch, '\u{e}' | '\u{f}'))
                == Some('\u{e}');
            let page = format!("{}{}", if dim { "\u{e}" } else { "" }, &text[range.clone()]);
            let png = renderer::render(
                &page,
                renderer::RenderOptions {
                    size: geometry.width,
                    font: Some(if geometry.dense { "8x8" } else { "8x13" }.into()),
                    cell_width: Some(geometry.cell_width),
                    cell_height: Some(geometry.cell_height),
                    variant: Some("bw".into()),
                    stretch: Some(false),
                    ..Default::default()
                },
            )?;
            checkpoint(cancelled)?;
            let encoded = STANDARD.encode(&png);
            image_bytes += encoded.len();
            rendered.push((
                Frame {
                    ordinal: 0,
                    height: u32::from_be_bytes(
                        png[20..24]
                            .try_into()
                            .map_err(|_| Error::Render("PNG header".into()))?,
                    ),
                    sha256: digest(&png),
                    png: encoded,
                    geometry,
                    omitted_before: 0,
                },
                range,
            ));
            while image_bytes > limits.max_image_bytes.min(MAX_ARCHIVE_BYTES / 2) {
                image_bytes -= rendered.remove(0).0.png.len();
            }
        }
        let mut last_end = head.end;
        for (mut frame, range) in rendered {
            frame.ordinal = archive.frames.len();
            frame.omitted_before =
                retained_ranges(&maps, std::slice::from_ref(&(last_end..range.start)))
                    .iter()
                    .map(|s| s.text.chars().count() as u64)
                    .sum();
            archive.frames.push(frame);
            kept.push(range.clone());
            last_end = range.end;
        }
        kept.push(tail);
    }
    archive.source = retained_ranges(&maps, &kept);
    let retained: u64 = archive
        .source
        .iter()
        .map(|s| s.text.chars().count() as u64)
        .sum();
    archive.truncated_chars = total - retained;
    checkpoint(cancelled)?;
    // Reject proposals that cannot fit; replay applies current model limits too.
    replay(&archive, limits)?;
    let persisted = archive.to_json()?;
    if persisted.len() > limits.max_payload_bytes.min(MAX_ARCHIVE_BYTES) {
        return Err(Error::Budget(
            "source + frames exceed deployment payload".into(),
        ));
    }
    checkpoint(cancelled)?;
    Ok(archive)
}

/// Automatic bounded text fallback for a reader that cannot consume bitmap
/// frames. The cap counts Unicode scalars; `text_fallback_chars` can only tighten
/// it. The result always carries the lossy-memory label and never claims to be
/// the complete archive.
fn text_fallback(archive: &Archive, limits: &ReplayLimits) -> Result<String> {
    let notice = format!(
        "{FALLBACK_NOTICE}[Archive source is lossy: {} of {} source chars were not retained]\n",
        archive.truncated_chars(),
        archive.total_chars()
    );
    let overhead = LEAD.len() + notice.len();
    let token_budget = limits.available_tokens;
    let payload_budget = limits.max_payload_bytes.saturating_sub(64);
    if token_budget <= overhead || payload_budget <= overhead {
        return Err(Error::Budget("text fallback does not fit".into()));
    }
    let source = serialize::plain(&archive.source_text());
    let render = |cap: usize| format!("{LEAD}{notice}{}", serialize::truncate(&source, cap));
    let fits = |text: &str| text.len() <= token_budget && json_text_bytes(text) <= payload_budget;
    let mut high = limits
        .text_fallback_chars
        .unwrap_or(MAX_SOURCE_BYTES)
        .min(MAX_SOURCE_BYTES)
        .min(source.chars().count());
    let text = render(high);
    if fits(&text) {
        return Ok(text);
    }
    // Rendered text grows with the cap; binary-search the largest fitting bound.
    let mut low = 0;
    let mut best = None;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let text = render(mid);
        if fits(&text) {
            best = Some(text);
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    if let Some(text) = best {
        return Ok(text);
    }
    let text = render(0);
    if fits(&text) {
        Ok(text)
    } else {
        Err(Error::Budget("text fallback does not fit".into()))
    }
}

/// Rebuild ordered text/image context with current send-time limits. Oldest
/// middle frames are omitted first and every omission is visible. A reader that
/// cannot consume the bitmap (nonvision or incompatible render geometry) gets a
/// clearly labelled, bounded and lossy text fallback instead of image blocks.
/// Limits are resolved for the current model, so switching back to a vision
/// model reads the same archive as images again.
/// # Errors
/// Rejects corrupt archives and representations that cannot fit current limits.
pub fn replay(archive: &Archive, limits: &ReplayLimits) -> Result<Vec<ReplayBlock>> {
    archive.validate()?;
    let geometry = limits
        .profile
        .map(|profile| profile.geometry(false))
        .transpose()?;
    let incompatible = !limits.vision
        || geometry.is_none()
        || archive.frames.iter().any(|frame| {
            geometry
                .as_ref()
                .is_some_and(|g| frame.geometry.width > g.width)
        });
    if incompatible {
        return Ok(vec![ReplayBlock::Text(text_fallback(archive, limits)?)]);
    }
    let mut start = 0;
    loop {
        let frames = &archive.frames[start..];
        let image_bytes: usize = frames.iter().map(|f| f.png.len()).sum();
        let text_bytes = json_text_bytes(LEAD)
            + json_text_bytes(&archive.text_head)
            + json_text_bytes(&archive.text_tail)
            + 512
            + frames.len() * 256;
        let token_cost: usize = text_bytes
            + frames
                .iter()
                .map(|f| {
                    geometry.as_ref().map_or(f.geometry.token_estimate, |g| {
                        g.token_estimate.max(f.geometry.token_estimate)
                    })
                })
                .sum::<usize>();
        if frames.len() <= limits.max_images
            && image_bytes <= limits.max_image_bytes
            && image_bytes + text_bytes <= limits.max_payload_bytes
            && token_cost <= limits.available_tokens
        {
            break;
        }
        if start == archive.frames.len() {
            return Err(Error::Budget("text edges do not fit current reader".into()));
        }
        start += 1;
    }
    let mut blocks = vec![ReplayBlock::Text(format!(
        "{LEAD}[{} source chars omitted in archive]\n{}",
        archive.truncated_chars, archive.text_head
    ))];
    if start > 0 {
        blocks.push(ReplayBlock::Text(format!(
            "[{start} oldest middle frames omitted for current reader]"
        )));
    }
    for frame in &archive.frames[start..] {
        if frame.omitted_before > 0 {
            blocks.push(ReplayBlock::Text(format!(
                "[{} source chars omitted before this frame]",
                frame.omitted_before
            )));
        }
        blocks.push(ReplayBlock::Text(format!(
            "[Archive frame {}]",
            frame.ordinal + 1
        )));
        blocks.push(ReplayBlock::Image {
            png: STANDARD
                .decode(&frame.png)
                .map_err(|e| Error::InvalidArchive(e.to_string()))?,
            width: frame.geometry.width,
            height: frame.height,
            sha256: frame.sha256.clone(),
        });
    }
    if !archive.text_tail.is_empty() {
        blocks.push(ReplayBlock::Text(archive.text_tail.clone()));
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests;
