//! PDF export.
//!
//! Each slide is drawn by the DeckCraft renderer at print resolution (so the PDF looks exactly like
//! the slide, effects included) and overlaid with an invisible layer of real text from the text
//! layout engine with embedded font subsets, so the text can be searched, selected and copied.
//! Hyperlinks and shape actions become link annotations, slide titles become bookmarks.
//!
//! Layouts: one slide per page, notes pages (slide above its speaker notes) and handouts (1, 2, 3,
//! 4, 6 or 9 slides per page).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};

use craft_pdf::write::{self, Metadata, Writer, backend as krilla};

use deckcraft_fonts::FontDb;
use deckcraft_model::Presentation;
use deckcraft_model::text::{Action as ClickAction, RunKind, safe_url};
use deckcraft_render::{Placed, PlacedText, RenderOpts};
use krilla::action::{Action, LinkAction};
use krilla::annotation::{Annotation, LinkAnnotation, Target};
use krilla::destination::XyzDestination;
use krilla::geom::{Point, Size, Transform};
use krilla::image::{BitsPerComponent, CustomImage, Image, ImageColorspace};
use krilla::num::NormalizedF32;
use krilla::outline::{Outline, OutlineNode};
use krilla::page::PageSettings;
use krilla::paint::Fill;
use krilla::surface::Surface;
use krilla::text::{Font, GlyphId, KrillaGlyph, TextDirection};
use kurbo::{Affine, Rect};
use serde::{Deserialize, Serialize};
#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("nothing to export (no slides in range)")]
    NoPages,
    #[error("PDF writer: {0}")]
    Write(String),
}

impl From<write::WriteError> for PdfError {
    fn from(error: write::WriteError) -> Self {
        Self::Write(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, PdfError>;

/// What each page shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PageLayout {
    /// One slide per page at the slide size.
    #[default]
    Slides,
    /// Portrait page: the slide above its notes.
    Notes,
    /// Portrait page with `per_page` slides (1, 2, 3, 4, 6 or 9).
    Handouts { per_page: u8 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PdfOptions {
    pub layout: PageLayout,
    /// Raster resolution of slide artwork (dots per inch, clamped to 36–600 dpi), lowered when a slide exceeds its DPI pixel budget (up to 64M pixels).
    pub dpi: f64,
    /// Slide indices to export (0-based); `None` = all.
    pub slides: Option<Vec<usize>>,
    pub include_hidden: bool,
    /// Invisible real-text layer (search, select, copy).
    pub text_layer: bool,
    /// Thin frame around each slide (notes/handouts).
    pub frame_slides: bool,
    /// Document title (defaults to the presentation title).
    pub title: Option<String>,
    /// Handout/notes page size in points (default US Letter portrait; see [`paper_size`]).
    pub paper: (f64, f64),
}

impl Default for PdfOptions {
    fn default() -> Self {
        PdfOptions {
            layout: PageLayout::Slides,
            dpi: 200.0,
            slides: None,
            include_hidden: false,
            text_layer: true,
            frame_slides: true,
            title: None,
            paper: (612.0, 792.0),
        }
    }
}

/// Paper sizes the Print page offers, as (id, label, width × height in points, portrait).
pub const PAPERS: [(&str, &str, (f64, f64)); 4] = [
    ("letter", "Letter (8.5 × 11 in)", (612.0, 792.0)),
    ("a4", "A4 (210 × 297 mm)", (595.28, 841.89)),
    ("legal", "Legal (8.5 × 14 in)", (612.0, 1008.0)),
    ("a3", "A3 (297 × 420 mm)", (841.89, 1190.55)),
];

/// The page size in points for a paper id from [`PAPERS`] (case-insensitive), turned on its side
/// when `landscape`. `None` for an unknown id.
pub fn paper_size(id: &str, landscape: bool) -> Option<(f64, f64)> {
    let (_, _, (w, h)) = PAPERS.iter().find(|p| p.0.eq_ignore_ascii_case(id.trim()))?;
    Some(if landscape { (*h, *w) } else { (*w, *h) })
}

/// The slide box on a notes page of `paper`: an inch of margin at the sides and the slide at most
/// half the page tall, so landscape paper still leaves room for the notes.
pub fn notes_slide_box(paper: (f64, f64), aspect: f64) -> Rect {
    let (pw, ph) = paper;
    let mut w = (pw - 2.0 * 72.0).max(1.0);
    let mut h = w / aspect.max(0.01);
    let max_h = (ph * 0.5 - 54.0).max(1.0);
    if h > max_h {
        h = max_h;
        w = h * aspect;
    }
    let x = (pw - w) / 2.0;
    Rect::new(x, 54.0, x + w, 54.0 + h)
}

/// Export `pres` as PDF bytes.
pub fn export(pres: &Presentation, opts: &PdfOptions) -> Result<Vec<u8>> {
    write::guard(|| export_inner(pres, opts))
}

fn plan_pages(pres: &Presentation, opts: &PdfOptions, slides: &[usize], sw: f64, sh: f64) -> Vec<Vec<(usize, Rect)>> {
    match opts.layout {
        PageLayout::Slides => slides.iter().map(|&i| vec![(i, Rect::new(0.0, 0.0, sw, sh))]).collect(),
        PageLayout::Notes => {
            let b = notes_frames(pres, opts.paper).0;
            slides.iter().map(|&i| vec![(i, b)]).collect()
        }
        PageLayout::Handouts { per_page } => {
            let boxes = handout_boxes(per_page, opts.paper, sw / sh);
            slides.chunks(boxes.len()).map(|c| c.iter().zip(&boxes).map(|(i, b)| (*i, *b)).collect()).collect()
        }
    }
}

fn build_outline(pres: &Presentation, pages: &[Vec<(usize, Rect)>]) -> Outline {
    let mut outline = Outline::new();
    for (p, v) in pages.iter().enumerate() {
        for (i, _) in v {
            let t = pres.slides[*i].title();
            let t = t.lines().next().unwrap_or("").trim().to_string();
            let label = if t.is_empty() { format!("Slide {}", i + 1) } else { format!("{}. {t}", i + 1) };
            outline.push_child(OutlineNode::new(label, XyzDestination::new(p, Point::from_xy(0.0, 0.0))));
        }
    }
    outline
}

fn export_inner(pres: &Presentation, opts: &PdfOptions) -> Result<Vec<u8>> {
    let slides: Vec<usize> = match &opts.slides {
        Some(v) => v.iter().copied().filter(|i| *i < pres.slides.len()).collect(),
        None => (0..pres.slides.len()).filter(|i| opts.include_hidden || !pres.slides[*i].hidden).collect(),
    };
    if slides.is_empty() {
        return Err(PdfError::NoPages);
    }
    let title = opts.title.clone().unwrap_or_else(|| pres.props.title.clone());
    let metadata = Metadata {
        creator: "DeckCraft".into(),
        producer: "DeckCraft".into(),
        title: (!title.trim().is_empty()).then_some(title),
        authors: if pres.props.author.trim().is_empty() { Vec::new() } else { vec![pres.props.author.clone()] },
        ..Default::default()
    };
    let mut writer = Writer::new(Default::default(), metadata);
    let pdf = writer.document_mut();

    let (sw, sh) = (pres.slide_size.width.max(1.0), pres.slide_size.height.max(1.0));
    // One clock reading for the whole export so backdrops sharing a layout reuse their raster
    // and a date field cannot differ between the raster and text layer.
    let now = deckcraft_text::datetime::DateTime::now_local();
    let mut ex = Exporter { pres, opts, fonts: HashMap::new(), backdrops: Backdrops::default() };
    // Page plan: per page, the slides on it with their boxes (page points).
    let pages = plan_pages(pres, opts, &slides, sw, sh);
    // Bookmarks and slide → page lookup for internal links.
    let page_of: HashMap<usize, usize> = pages.iter().enumerate().flat_map(|(p, v)| v.iter().map(move |(i, _)| (*i, p))).collect();
    pdf.set_outline(build_outline(pres, &pages));

    let page_px: usize = pages.first().map_or(1, |v| {
        v.iter()
            .map(|(_, b)| {
                let s = raster_scale(b.width(), (sw, sh), opts.dpi);
                ((sw * s) * (sh * s)).round() as usize
            })
            .sum::<usize>()
            .max(1)
    });
    let chunk_size = page_chunk_size(page_px);
    for page_chunk in pages.chunks(chunk_size) {
        let prepared = prepare_page_chunk(pres, opts, page_chunk, (sw, sh), now, &mut ex.backdrops);
        for (v, slides_ready) in page_chunk.iter().zip(prepared) {
            let (pw, ph) = match opts.layout {
                PageLayout::Slides => (sw, sh),
                _ => opts.paper,
            };
            let size = Size::from_wh(pw as f32, ph as f32).ok_or(PdfError::NoPages)?;
            let mut page = pdf.start_page_with(PageSettings::new(size));
            let mut annots = Vec::new();
            {
                let mut s = page.surface();
                for ((i, b), ready) in v.iter().zip(slides_ready) {
                    ex.draw_prepared_slide(&mut s, *b, &ready)?;
                    annots.extend(ex.links_from(*i, *b, &page_of, &ready.placed));
                }
                match opts.layout {
                    PageLayout::Notes => {
                        if let Some((i, _)) = v.first() {
                            let notes = pres.slides[*i].notes_text();
                            ex.paragraphs(&mut s, &notes, notes_frames(pres, opts.paper).1, 12.0)?;
                        }
                        ex.page_texts(&mut s, pres.notes_master.as_deref(), pw, ph, v.first().map(|x| x.0 + 1).unwrap_or(1))?;
                    }
                    PageLayout::Handouts { per_page } => {
                        if per_page == 3 {
                            for (_, b) in v {
                                ex.note_lines(&mut s, Rect::new(b.x1 + 24.0, b.y0, pw - 54.0, b.y1));
                            }
                        }
                        ex.page_texts(&mut s, pres.handout_master.as_deref(), pw, ph, page_of.get(&v[0].0).map(|p| p + 1).unwrap_or(1))?;
                    }
                    PageLayout::Slides => {}
                }
                s.finish();
            }
            for a in annots {
                page.add_annotation(a);
            }
            page.finish();
        }
    }
    writer.finish().map_err(Into::into)
}

/// A notes-size master's rect of placeholder `kind`, scaled to a page of `paper` points.
fn master_rect(pres: &Presentation, m: &deckcraft_model::Master, kind: deckcraft_model::PhType, paper: (f64, f64)) -> Option<Rect> {
    let x = m.shapes.iter().find(|s| s.ph_type() == Some(kind))?.xfrm?;
    let (kx, ky) = (paper.0 / pres.notes_size.width.max(1.0), paper.1 / pres.notes_size.height.max(1.0));
    let r = Rect::new(x.x * kx, x.y * ky, (x.x + x.w) * kx, (x.y + x.h) * ky);
    (r.width() > 1.0 && r.height() > 1.0).then_some(r)
}

/// A notes page's slide box and notes area on `paper`: where the notes master puts its slide image
/// and body placeholders, or the default layout when it has none.
pub fn notes_frames(pres: &Presentation, paper: (f64, f64)) -> (Rect, Rect) {
    let (pw, ph) = paper;
    let aspect = pres.slide_size.width.max(1.0) / pres.slide_size.height.max(1.0);
    let nm = pres.notes_master.as_deref();
    let slide = match nm.and_then(|m| master_rect(pres, m, deckcraft_model::PhType::SlideImage, paper)) {
        // The slide keeps its own shape, centred in the frame.
        Some(f) => {
            let (w, h) = if f.width() / f.height() > aspect { (f.height() * aspect, f.height()) } else { (f.width(), f.width() / aspect) };
            Rect::from_center_size(f.center(), (w, h))
        }
        None => notes_slide_box(paper, aspect),
    };
    let notes = nm
        .and_then(|m| master_rect(pres, m, deckcraft_model::PhType::Body, paper))
        .unwrap_or_else(|| Rect::new(72.0, slide.y1 + 36.0, pw - 72.0, ph - 54.0));
    (slide, notes)
}

/// How a master's header, date, footer or page number is printed: its own resolved formatting.
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    pub family: String,
    /// Points, before scaling to the paper.
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    pub color: deckcraft_model::Rgba,
}

/// The resolved style of placeholder `sh`'s first run (or its paragraph's end properties) on
/// master `m`: run, list style, master text styles, theme fonts and colours.
pub fn master_text_style(pres: &Presentation, m: &deckcraft_model::Master, sh: &deckcraft_model::Shape) -> TextStyle {
    use deckcraft_model::resolve::{self, Ctx};
    let ctx = Ctx::for_master(pres, m);
    let para = sh.text.as_ref().and_then(|t| t.paragraphs.first()).cloned().unwrap_or_default();
    let own = para.runs.first().map(|r| r.props.clone()).unwrap_or_else(|| para.end_props.clone());
    let r = resolve::run(&ctx, sh, &para, &own);
    let family = r.font.as_deref().map(|f| if f.starts_with('+') { m.theme.font(f) } else { f.to_string() }).unwrap_or_default();
    let color = match &r.fill {
        Some(deckcraft_model::Fill::Solid { color }) => ctx.color(color, None),
        _ => deckcraft_model::Rgba::rgb(0x60, 0x60, 0x60),
    };
    TextStyle { family, size: r.size.unwrap_or(12.0).clamp(1.0, 400.0), bold: r.bold == Some(true), italic: r.italic == Some(true), color }
}

/// One laid-out piece of master text on a page of `paper` points: a run's text on one line, its
/// baseline start and its size, colour, weight and slant.
#[derive(Clone, Debug, PartialEq)]
pub struct TextPiece {
    pub at: (f64, f64),
    pub text: String,
    pub size: f64,
    pub color: deckcraft_model::Rgba,
    pub bold: bool,
    pub italic: bool,
}

/// The header, date, footer and page number of `m` for page `n`, laid out by the text engine
/// (wrapped, mixed formatting) and scaled to `paper`, for drawing without glyph access (the
/// Notes Page view).
pub fn master_text_pieces(pres: &Presentation, m: &deckcraft_model::Master, paper: (f64, f64), n: usize) -> Vec<TextPiece> {
    let (kx, ky) = (paper.0 / pres.notes_size.width.max(1.0), paper.1 / pres.notes_size.height.max(1.0));
    let mut out = vec![];
    for pt in deckcraft_render::place_master_texts(pres, m, n) {
        let mut para_chars: HashMap<usize, Vec<char>> = HashMap::new();
        for run in &pt.layout.runs {
            let Some(g) = run.glyphs.first() else { continue };
            let chars = para_chars.entry(run.para).or_insert_with(|| {
                pt.body.paragraphs.get(run.para).map(|p| p.runs.iter().flat_map(|r| r.content().chars()).collect()).unwrap_or_default()
            });
            let (a, b) = (run.chars.0.min(chars.len()), run.chars.1.min(chars.len()));
            let text: String = chars.get(a..b.max(a)).unwrap_or_default().iter().collect();
            if text.trim().is_empty() {
                continue;
            }
            let p = pt.transform * kurbo::Point::new(g.1, g.2);
            out.push(TextPiece {
                at: (p.x * kx, p.y * ky),
                text,
                size: run.size * ky,
                color: run.color,
                bold: run.fake_bold || run.face.weight >= 600.0,
                italic: run.fake_italic || run.face.italic,
            });
        }
    }
    out
}

/// The header, date, footer and page-number text a master puts on page `n`, with where it goes,
/// whether it's right-aligned and its style. Placeholders with no text are left out; the page
/// number is always `n`.
pub fn master_texts(pres: &Presentation, m: &deckcraft_model::Master, paper: (f64, f64), n: usize) -> Vec<(Rect, String, bool, TextStyle)> {
    use deckcraft_model::PhType;
    let mut out = vec![];
    for kind in [PhType::Header, PhType::Date, PhType::Footer, PhType::SlideNum] {
        let Some(r) = master_rect(pres, m, kind, paper) else { continue };
        let Some(sh) = m.shapes.iter().find(|s| s.ph_type() == Some(kind)) else { continue };
        let text = if kind == PhType::SlideNum { n.to_string() } else { sh.text.as_ref().map(|t| t.text()).unwrap_or_default().trim().to_string() };
        if text.is_empty() {
            continue;
        }
        let right = sh
            .text
            .as_ref()
            .and_then(|t| t.paragraphs.first())
            .and_then(|p| p.props.align)
            .map_or(matches!(kind, PhType::Date | PhType::SlideNum), |a| a == deckcraft_model::text::Align::Right);
        let mut style = master_text_style(pres, m, sh);
        // Sizes scale with the page, as the placeholders do.
        style.size *= paper.1 / pres.notes_size.height.max(1.0);
        out.push((r, text, right, style));
    }
    out
}

/// Slide boxes for a handout page (portrait paper, half-inch-ish margins).
pub fn handout_boxes(per_page: u8, paper: (f64, f64), aspect: f64) -> Vec<Rect> {
    let (pw, ph) = paper;
    let (mx, top, bottom) = (54.0, 54.0, 54.0);
    let (cols, rows, notes) = match per_page {
        0 | 1 => (1, 1, false),
        2 => (1, 2, false),
        3 => (1, 3, true),
        4 => (2, 2, false),
        6 => (2, 3, false),
        _ => (3, 3, false),
    };
    let gap = 18.0;
    let aw = if notes { (pw - 2.0 * mx) * 0.5 } else { pw - 2.0 * mx };
    let ah = ph - top - bottom - 18.0;
    let cw = (aw - gap * (cols as f64 - 1.0)) / cols as f64;
    let ch = (ah - gap * (rows as f64 - 1.0)) / rows as f64;
    let (w, h) = if cw / ch > aspect { (ch * aspect, ch) } else { (cw, cw / aspect) };
    let mut out = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let cx = mx + c as f64 * (cw + gap) + (cw - w) / 2.0;
            let cy = top + r as f64 * (ch + gap) + (ch - h) / 2.0;
            let cx = if notes { mx } else { cx };
            out.push(Rect::new(cx, cy, cx + w, cy + h));
        }
    }
    out
}

struct Exporter<'a> {
    pres: &'a Presentation,
    opts: &'a PdfOptions,
    fonts: HashMap<u32, Font>,
    backdrops: Backdrops,
}

fn tf(a: Affine) -> Transform {
    let c = a.as_coeffs();
    Transform::from_row(c[0] as f32, c[1] as f32, c[2] as f32, c[3] as f32, c[4] as f32, c[5] as f32)
}

fn grey(v: u8, opacity: f32) -> Fill {
    Fill {
        paint: krilla::color::rgb::Color::new(v, v, v).into(),
        opacity: NormalizedF32::new(opacity.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ONE),
        ..Default::default()
    }
}

/// Premultiplied RGBA (the renderer's pixels) to the straight RGBA the writer stores. Runs of
/// pixels that are all fully transparent or fully opaque, most of a slide, are skipped in blocks.
#[cfg(test)]
fn straight(rgba: &mut [u8]) {
    let (blocks, rest) = rgba.as_chunks_mut::<64>();
    for block in blocks {
        if block.as_chunks::<4>().0.iter().fold(false, |m, px| m | (px[3].wrapping_sub(1) < 254)) {
            straight_px(block);
        }
    }
    straight_px(rest);
}

#[cfg(test)]
fn straight_px(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = (((*c as u32 * 255 + a / 2) / a).min(255)) as u8;
            }
        }
    }
}

/// A backdrop raster composited onto white (slides are normally opaque, and opaque blocks are skipped).
fn opaque(rgba: &mut [u8]) {
    let (blocks, rest) = rgba.as_chunks_mut::<64>();
    for block in blocks {
        if !block.as_chunks::<4>().0.iter().fold(true, |m, px| m & (px[3] == 255)) {
            opaque_px(block);
        }
    }
    opaque_px(rest);
}

fn opaque_px(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let k = 255 - px[3];
        px[0] = px[0].saturating_add(k);
        px[1] = px[1].saturating_add(k);
        px[2] = px[2].saturating_add(k);
        px[3] = 255;
    }
}

/// Gives every fully transparent content pixel the backdrop's colour. The renderer leaves them
/// RGB 0,0,0; a viewer that smooths the image blends colour and mask separately, so the black
/// shows as a grey hairline around white pictures and shapes (#1455).
#[cfg(test)]
fn fill_clear(front: &mut [u8], back: &[u8]) {
    for (f, b) in front.as_chunks_mut::<4>().0.iter_mut().zip(back.as_chunks::<4>().0) {
        if f[3] == 0 {
            f[..3].copy_from_slice(&b[..3]);
        }
    }
}

/// Most bytes of full-frame backdrop RGB kept for `fill_clear` when slides reuse a non-solid
/// backdrop. Solid backdrops take 3 bytes; encoded PDF XObject handles are kept for all slides.
const BACKDROP_CACHE_BYTES: usize = 128 << 20;

/// Maximum pages prepared together per parallel chunk; actual chunk size is dynamically bounded
/// by [`IN_FLIGHT_PIXEL_BUDGET`] so peak raster RAM stays bounded even at 600 dpi.
const MAX_PAGE_CHUNK: usize = 8;
/// Total in-flight slide pixels prepared in parallel per chunk (e.g. 2 slides at 36M px/slide,
/// up to 8 at standard DPI).
const IN_FLIGHT_PIXEL_BUDGET: usize = 72_000_000;

#[derive(Clone)]
struct RawImage {
    rgb: Arc<Vec<u8>>,
    alpha: Option<Arc<Vec<u8>>>,
    size: (u32, u32),
    hash: (u64, u64),
}

impl Hash for RawImage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.size.hash(state);
        self.alpha.is_some().hash(state);
        self.hash.hash(state);
    }
}

impl CustomImage for RawImage {
    fn color_channel(&self) -> &[u8] {
        &self.rgb
    }
    fn alpha_channel(&self) -> Option<&[u8]> {
        self.alpha.as_deref().map(Vec::as_slice)
    }
    fn bits_per_component(&self) -> BitsPerComponent {
        BitsPerComponent::Eight
    }
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn icc_profile(&self) -> Option<&[u8]> {
        None
    }
    fn color_space(&self) -> ImageColorspace {
        ImageColorspace::Rgb
    }
}

fn hash_bytes_128(bytes: &[u8]) -> (u64, u64) {
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h1);
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    0x9e37_79b9_7f4a_7c15_u64.hash(&mut h2);
    bytes.len().hash(&mut h2);
    let (chunks, tail) = bytes.as_chunks::<8>();
    for c in chunks {
        h2.write_u64(u64::from_le_bytes(*c).rotate_left(13) ^ 0xa076_1d64_78bd_642f);
    }
    h2.write(tail);
    (h1.finish(), h2.finish())
}

fn rgb_image(rgb: Arc<Vec<u8>>, alpha: Option<Arc<Vec<u8>>>, w: u32, h: u32, hash: (u64, u64)) -> Option<Image> {
    Image::from_custom(RawImage { rgb, alpha, size: (w, h), hash }, false).ok()
}

#[derive(Clone)]
enum BackdropPixels {
    Solid([u8; 3]),
    Rgb(Arc<Vec<u8>>),
}

#[derive(Clone)]
enum BackdropRef {
    Solid([u8; 3]),
    Rgb(Weak<Vec<u8>>),
}

#[derive(Clone)]
struct HashedBackdrop {
    image: Image,
    pixels: BackdropRef,
}

#[derive(Clone)]
struct CachedBackdrop {
    image: Image,
    pixels: BackdropPixels,
}

/// Structural identity of a slide's backdrop (background + master/layout non-placeholder graphics)
/// at a given render scale. Two slides with the same key produce identical backdrop pixels,
/// so subsequent slides skip rendering `SlidePart::Backdrop` altogether.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BackdropKey {
    layout: deckcraft_model::LayoutId,
    show_master_shapes: bool,
    color_map: Option<Vec<(String, String)>>,
    background: Option<String>,
    slide_num: Option<u32>,
    scale_bits: u64,
}
fn text_has_slidenum(tb: &deckcraft_model::text::TextBody) -> bool {
    tb.paragraphs.iter().any(|p| p.runs.iter().any(|r| matches!(&r.kind, RunKind::Field { field } if field.eq_ignore_ascii_case("slidenum"))))
}

fn shapes_have_slidenum(shapes: &[deckcraft_model::Shape]) -> bool {
    let mut found = false;
    deckcraft_model::walk(shapes, &mut |s, _| {
        if found || s.ph.is_some() {
            return;
        }
        if s.text.as_ref().is_some_and(text_has_slidenum) {
            found = true;
        } else if let deckcraft_model::ShapeKind::Table(t) = &s.kind
            && t.rows.iter().any(|r| r.cells.iter().any(|c| text_has_slidenum(&c.text)))
        {
            found = true;
        }
    });
    found
}

fn backdrop_key(pres: &Presentation, index: usize, scale: f64) -> BackdropKey {
    let slide = &pres.slides[index];
    let scale_bits = if scale == 0.0 { 0.0_f64.to_bits() } else { scale.to_bits() };
    let has_slidenum = deckcraft_model::resolve::Ctx::for_slide(pres, slide).is_some_and(|rctx| {
        let (show_layout, show_master) = deckcraft_model::resolve::show_master_shapes(slide, rctx.layout);
        (show_master && shapes_have_slidenum(&rctx.master.shapes)) || (show_layout && rctx.layout.is_some_and(|l| shapes_have_slidenum(&l.shapes)))
    });
    BackdropKey {
        layout: slide.layout,
        show_master_shapes: slide.show_master_shapes,
        color_map: slide.color_map.clone(),
        background: slide.background.as_ref().map(|b| format!("{b:?}")),
        slide_num: has_slidenum.then(|| pres.slide_number(index)),
        scale_bits,
    }
}

/// Backdrops already rendered and handed to the writer.
///
/// - `by_key` avoids re-rendering `SlidePart::Backdrop` for slides that share a layout/background.
/// - `by_hash` deduplicates identical rendered backdrops even across distinct layouts/overrides
///   using a 128-bit digest plus exact pixel verification when the cached buffer is still resident,
///   keeping only lightweight `krilla::Image` handles so large decks never lose deduplication.
/// - Non-solid RGB pixel buffers kept for `fill_clear` are FIFO-bounded by `BACKDROP_CACHE_BYTES`.
#[derive(Default)]
struct Backdrops {
    by_key: HashMap<BackdropKey, CachedBackdrop>,
    by_hash: HashMap<(u32, u32, u64, u64), Vec<HashedBackdrop>>,
    rgb_order: VecDeque<(BackdropKey, usize)>,
    rgb_bytes: usize,
}

struct EncodedBackdrop {
    w: u32,
    h: u32,
    hash: (u64, u64),
    rgb: Arc<Vec<u8>>,
    solid: Option<[u8; 3]>,
}

fn encode_backdrop(mut img: deckcraft_render::Image) -> Option<EncodedBackdrop> {
    opaque(&mut img.pixels);
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 {
        return None;
    }
    let mut rgb = Vec::with_capacity((w as usize).saturating_mul(h as usize).saturating_mul(3));
    let mut solid = img.pixels.get(0..3).map(|s| [s[0], s[1], s[2]]);
    for px in img.pixels.as_chunks::<4>().0 {
        let c = [px[0], px[1], px[2]];
        if solid.is_some_and(|first| first != c) {
            solid = None;
        }
        rgb.extend_from_slice(&c);
    }
    let hash = hash_bytes_128(&rgb);
    Some(EncodedBackdrop { w, h, hash, rgb: Arc::new(rgb), solid })
}

impl Backdrops {
    fn get(&self, key: &BackdropKey) -> Option<CachedBackdrop> {
        self.by_key.get(key).cloned()
    }

    fn insert_encoded(&mut self, key: BackdropKey, enc: EncodedBackdrop) -> Option<CachedBackdrop> {
        let bucket = self.by_hash.entry((enc.w, enc.h, enc.hash.0, enc.hash.1)).or_default();
        let existing = bucket.iter().find(|cand| match (&cand.pixels, enc.solid) {
            (BackdropRef::Solid(a), Some(b)) => *a == b,
            (BackdropRef::Solid(_), None) | (BackdropRef::Rgb(_), Some(_)) => false,
            (BackdropRef::Rgb(weak), None) => weak.upgrade().is_none_or(|buf| *buf == *enc.rgb),
        });
        let image = match existing {
            Some(hit) => hit.image.clone(),
            None => {
                let created = rgb_image(enc.rgb.clone(), None, enc.w, enc.h, enc.hash)?;
                let pref = match enc.solid {
                    Some(c) => BackdropRef::Solid(c),
                    None => BackdropRef::Rgb(Arc::downgrade(&enc.rgb)),
                };
                bucket.push(HashedBackdrop { image: created.clone(), pixels: pref });
                created
            }
        };
        let pixels = match enc.solid {
            Some(c) => BackdropPixels::Solid(c),
            None => BackdropPixels::Rgb(enc.rgb),
        };
        let entry = CachedBackdrop { image, pixels };
        match &entry.pixels {
            BackdropPixels::Solid(_) => {
                self.by_key.insert(key, entry.clone());
            }
            BackdropPixels::Rgb(buf) => {
                let len = buf.len();
                if len <= BACKDROP_CACHE_BYTES {
                    while self.rgb_bytes + len > BACKDROP_CACHE_BYTES
                        && let Some((old_key, old_len)) = self.rgb_order.pop_front()
                    {
                        self.by_key.remove(&old_key);
                        self.rgb_bytes = self.rgb_bytes.saturating_sub(old_len);
                    }
                    self.rgb_bytes += len;
                    self.rgb_order.push_back((key.clone(), len));
                    self.by_key.insert(key, entry.clone());
                }
            }
        }
        Some(entry)
    }
}

/// Unpremultiplies `front` (premultiplied RGBA) into straight RGB + alpha channels in a single
/// pass, filling fully transparent pixels with the backdrop's RGB colour (#1455). Returns `None`
/// when every pixel is transparent (pure backdrop slide).
fn split_content_over_backdrop(front: &[u8], back: &BackdropPixels) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let chunks = front.as_chunks::<4>().0;
    if !chunks.iter().any(|px| px[3] != 0) {
        return None;
    }
    let n = chunks.len();
    let mut rgb = Vec::with_capacity(n * 3);
    let mut alpha = Vec::with_capacity(n);
    let mut all_opaque = true;
    match back {
        BackdropPixels::Solid(bg) => {
            for px in chunks {
                let a = px[3];
                alpha.push(a);
                if a == 0 {
                    all_opaque = false;
                    rgb.extend_from_slice(bg);
                } else if a == 255 {
                    rgb.extend_from_slice(&px[..3]);
                } else {
                    all_opaque = false;
                    let au = a as u32;
                    rgb.push((((px[0] as u32 * 255 + au / 2) / au).min(255)) as u8);
                    rgb.push((((px[1] as u32 * 255 + au / 2) / au).min(255)) as u8);
                    rgb.push((((px[2] as u32 * 255 + au / 2) / au).min(255)) as u8);
                }
            }
        }
        BackdropPixels::Rgb(bg_buf) => {
            let bg_chunks = bg_buf.as_chunks::<3>().0;
            for (idx, px) in chunks.iter().enumerate() {
                let a = px[3];
                alpha.push(a);
                if a == 0 {
                    all_opaque = false;
                    let bg = bg_chunks.get(idx).copied().unwrap_or([255, 255, 255]);
                    rgb.extend_from_slice(&bg);
                } else if a == 255 {
                    rgb.extend_from_slice(&px[..3]);
                } else {
                    all_opaque = false;
                    let au = a as u32;
                    rgb.push((((px[0] as u32 * 255 + au / 2) / au).min(255)) as u8);
                    rgb.push((((px[1] as u32 * 255 + au / 2) / au).min(255)) as u8);
                    rgb.push((((px[2] as u32 * 255 + au / 2) / au).min(255)) as u8);
                }
            }
        }
    }
    Some((rgb, (!all_opaque).then_some(alpha)))
}

struct PreparedSlide {
    backdrop: Option<Image>,
    content: Option<Image>,
    content_opaque: bool,
    placed: Placed,
}

fn prepare_slide(
    pres: &Presentation,
    index: usize,
    scale: f64,
    now: Option<deckcraft_text::datetime::DateTime>,
    backdrop: Option<&CachedBackdrop>,
    threads: u16,
) -> PreparedSlide {
    let ropts = RenderOpts { scale, threads, now, ..Default::default() };
    let front = deckcraft_render::render_slide_part(pres, index, deckcraft_render::SlidePart::Content, &ropts);
    let mut content_opaque = false;
    let content = backdrop.and_then(|back| {
        let (w, h) = (front.width, front.height);
        if w == 0 || h == 0 {
            return None;
        }
        let (rgb, alpha) = split_content_over_backdrop(&front.pixels, &back.pixels)?;
        content_opaque = alpha.is_none();
        // Per-slide content rasters are distinct per slide; use a unique tag to avoid hashing tens
        // of megabytes of pixels on the render thread.
        let tag = ((1u64 << 63) | (((index as u64) << 16) ^ ((w as u64) << 32) ^ (h as u64)), index as u64);
        rgb_image(Arc::new(rgb), alpha.map(Arc::new), w, h, tag)
    });
    let placed = deckcraft_render::place_slide_with(pres, index, &ropts);
    PreparedSlide { backdrop: backdrop.map(|b| b.image.clone()), content, content_opaque, placed }
}

fn prepare_page_chunk(
    pres: &Presentation,
    opts: &PdfOptions,
    chunk: &[Vec<(usize, Rect)>],
    slide_size: (f64, f64),
    now: Option<deckcraft_text::datetime::DateTime>,
    backdrops: &mut Backdrops,
) -> Vec<Vec<PreparedSlide>> {
    // Gather jobs for all slides in this chunk and identify uncached backdrops.
    let mut jobs: Vec<(usize, f64, BackdropKey)> = Vec::new();
    let mut missing_keys: Vec<(usize, f64, BackdropKey)> = Vec::new();
    let mut seen_missing: HashSet<BackdropKey> = HashSet::new();
    for page in chunk {
        for (i, b) in page {
            let scale = raster_scale(b.width(), slide_size, opts.dpi);
            let key = backdrop_key(pres, *i, scale);
            if backdrops.get(&key).is_none() && seen_missing.insert(key.clone()) {
                missing_keys.push((*i, scale, key.clone()));
            }
            jobs.push((*i, scale, key));
        }
    }

    // Render and RGB-encode missing backdrops (in parallel off wasm32 when there are several).
    let rendered_backdrops: Vec<(BackdropKey, Option<EncodedBackdrop>)> = {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use rayon::prelude::{IntoParallelRefIterator as _, ParallelIterator as _};
            let threads = if missing_keys.len() > 1 { 0 } else { 4 };
            missing_keys
                .par_iter()
                .map(|(i, scale, key)| {
                    let ropts = RenderOpts { scale: *scale, threads, now, ..Default::default() };
                    let img = deckcraft_render::render_slide_part(pres, *i, deckcraft_render::SlidePart::Backdrop, &ropts);
                    (key.clone(), encode_backdrop(img))
                })
                .collect()
        }
        #[cfg(target_arch = "wasm32")]
        {
            missing_keys
                .iter()
                .map(|(i, scale, key)| {
                    let ropts = RenderOpts { scale: *scale, threads: 0, now, ..Default::default() };
                    let img = deckcraft_render::render_slide_part(pres, *i, deckcraft_render::SlidePart::Backdrop, &ropts);
                    (key.clone(), encode_backdrop(img))
                })
                .collect()
        }
    };
    let mut local_backdrops: HashMap<BackdropKey, CachedBackdrop> = HashMap::new();
    for (key, enc) in rendered_backdrops {
        if let Some(enc) = enc
            && let Some(cached) = backdrops.insert_encoded(key.clone(), enc)
        {
            local_backdrops.insert(key, cached);
        }
    }

    let job_backdrops: Vec<Option<CachedBackdrop>> =
        jobs.iter().map(|(_, _, key)| backdrops.get(key).or_else(|| local_backdrops.get(key).cloned())).collect();

    // Render slide contents & text/link placement (in parallel across the chunk off wasm32).
    let flat: Vec<PreparedSlide> = {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use rayon::prelude::{IndexedParallelIterator as _, IntoParallelRefIterator as _, ParallelIterator as _};
            let threads = if jobs.len() > 1 { 0 } else { 4 };
            jobs.par_iter()
                .zip(job_backdrops.par_iter())
                .map(|((i, scale, _), back)| prepare_slide(pres, *i, *scale, now, back.as_ref(), threads))
                .collect()
        }
        #[cfg(target_arch = "wasm32")]
        {
            jobs.iter().zip(job_backdrops.iter()).map(|((i, scale, _), back)| prepare_slide(pres, *i, *scale, now, back.as_ref(), 0)).collect()
        }
    };

    let mut it = flat.into_iter();
    chunk.iter().map(|page| it.by_ref().take(page.len()).collect()).collect()
}

/// Base pixel budget per slide raster at <= 300 dpi: 9M pixels preserves full 300 dpi on standard
/// 16:9 (9.0M px) and Letter (8.4M px) slides while keeping giant stage decks (56"×18") at ~4K–5K.
/// Above 300 dpi the cap scales proportionally to `(dpi / 300)^2` up to `MAX_RASTER_PIXELS_CEIL`
/// (64M px) so explicit high-DPI exports honor the documented 36–600 dpi contract.
const BASE_RASTER_PIXELS: f64 = 9_000_000.0;
const MAX_RASTER_PIXELS_CEIL: f64 = 64_000_000.0;

/// Most ruled note lines one handout area gets (a letter page holds about 30).
const MAX_NOTE_LINES: usize = 500;

/// Per-slide pixel budget for `dpi` (already clamped or unclamped).
fn raster_pixel_cap(dpi: f64) -> f64 {
    let clamped = dpi.clamp(36.0, 600.0);
    if clamped > 300.0 { (BASE_RASTER_PIXELS * (clamped / 300.0).powi(2)).min(MAX_RASTER_PIXELS_CEIL) } else { BASE_RASTER_PIXELS }
}

/// Number of pages to prepare concurrently given `slide_px` pixels per page.
fn page_chunk_size(slide_px: usize) -> usize {
    (IN_FLIGHT_PIXEL_BUDGET / slide_px.max(1)).clamp(1, MAX_PAGE_CHUNK)
}

/// Render scale for a slide of `slide` units drawn `box_width` points wide at `dpi`, with the total pixel count capped.
fn raster_scale(box_width: f64, slide: (f64, f64), dpi: f64) -> f64 {
    let clamped = dpi.clamp(36.0, 600.0);
    let scale = (box_width / 72.0 * clamped) / slide.0;
    let cap = raster_pixel_cap(clamped);
    let pixels = slide.0 * scale * slide.1 * scale;
    if pixels > cap { scale * (cap / pixels).sqrt() } else { scale }
}

impl Exporter<'_> {
    fn font(&mut self, face: &deckcraft_fonts::FontFace) -> Result<Font> {
        if let Some(font) = self.fonts.get(&face.id()) {
            return Ok(font.clone());
        }
        let font = write::font(face.data(), face.index(), None)?;
        self.fonts.insert(face.id(), font.clone());
        Ok(font)
    }

    /// Draw a prepared slide into `b` (page points).
    fn draw_prepared_slide(&mut self, s: &mut Surface, b: Rect, ready: &PreparedSlide) -> Result<()> {
        let pres = self.pres;
        let sw = pres.slide_size.width.max(1.0);
        let k = b.width() / sw;
        if let Some(size) = Size::from_wh(b.width() as f32, b.height() as f32) {
            s.push_transform(&Transform::from_translate(b.x0 as f32, b.y0 as f32));
            if !ready.content_opaque
                && let Some(back) = &ready.backdrop
            {
                s.draw_image(back.clone(), size);
            }
            if let Some(front) = &ready.content {
                s.draw_image(front.clone(), size);
            }
            s.pop();
        }
        if self.opts.frame_slides && self.opts.layout != PageLayout::Slides {
            let p = kurbo::Shape::to_path(&b, 0.1);
            if let Some(path) = to_path(&p) {
                s.set_fill(None);
                s.set_stroke(Some(krilla::paint::Stroke {
                    paint: krilla::color::rgb::Color::new(0x80, 0x80, 0x80).into(),
                    width: 0.5,
                    ..Default::default()
                }));
                s.draw_path(&path);
                s.set_stroke(None);
            }
        }
        if self.opts.text_layer {
            let view = Affine::translate((b.x0, b.y0)) * Affine::scale(k);
            for pt in &ready.placed.texts {
                self.text_layer(s, pt, view, false)?;
            }
        }
        Ok(())
    }

    /// Invisible glyphs exactly over the rasterised text.
    fn text_layer(&mut self, s: &mut Surface, pt: &PlacedText, view: Affine, visible: bool) -> Result<()> {
        // Each paragraph's characters, gathered once: a long paragraph wraps into many runs.
        let mut para_chars: HashMap<usize, Vec<char>> = HashMap::new();
        for run in &pt.layout.runs {
            if run.glyphs.is_empty() || run.size <= 0.0 {
                continue;
            }
            let chars = para_chars.entry(run.para).or_insert_with(|| {
                pt.body.paragraphs.get(run.para).map(|p| p.runs.iter().flat_map(|r| r.content().chars()).collect()).unwrap_or_default()
            });
            let (a, b) = (run.chars.0.min(chars.len()), run.chars.1.min(chars.len()));
            let text: String = chars.get(a..b.max(a)).unwrap_or_default().iter().collect();
            if text.trim().is_empty() {
                continue;
            }
            let font = self.font(&run.face)?;
            let n = run.glyphs.len();
            let ranges = glyph_text_ranges(&text, run.chars, &run.cells, n);
            let (x0, y0) = (run.glyphs[0].1, run.glyphs[0].2);
            let kg: Vec<KrillaGlyph> = run
                .glyphs
                .iter()
                .enumerate()
                .map(|(gi, g)| {
                    let next = run.glyphs.get(gi + 1).map_or(g.1 + run.face.advance(g.0) * run.size / run.face.upem.max(1.0), |nx| nx.1);
                    let range = ranges.get(gi).cloned().unwrap_or(text.len()..text.len());
                    KrillaGlyph::new(GlyphId::new(g.0), ((next - g.1) / run.size) as f32, 0.0, ((g.2 - y0) / run.size) as f32, 0.0, range, None)
                })
                .collect();
            s.push_transform(&tf(view * pt.transform * Affine::translate((x0, y0))));
            s.set_fill(Some(if visible {
                let c = run.color;
                Fill {
                    paint: krilla::color::rgb::Color::new(c.r, c.g, c.b).into(),
                    opacity: NormalizedF32::new((f64::from(c.a) / 255.0 * run.alpha) as f32).unwrap_or(NormalizedF32::ONE),
                    ..Default::default()
                }
            } else {
                grey(0, 0.0)
            }));
            s.draw_glyphs(Point::from_xy(0.0, 0.0), &kg, font, &text, run.size as f32, false);
            s.set_fill(None);
            s.pop();
        }
        Ok(())
    }

    fn links_from(&self, i: usize, b: Rect, page_of: &HashMap<usize, usize>, placed: &Placed) -> Vec<Annotation> {
        let pres = self.pres;
        let k = b.width() / pres.slide_size.width.max(1.0);
        let view = Affine::translate((b.x0, b.y0)) * Affine::scale(k);
        let mut out = Vec::new();
        for l in &placed.links {
            let r = view.transform_rect_bbox(l.rect).intersect(b);
            let Some(rect) = krilla::geom::Rect::from_ltrb(r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32) else { continue };
            let goto = |slide: Option<usize>| {
                slide.and_then(|s| page_of.get(&s)).map(|p| Target::Destination(XyzDestination::new(*p, Point::from_xy(0.0, 0.0)).into()))
            };
            let target = match &l.link.action {
                // Only the links the slide show opens (web and mail): a reader of the file gets the rest
                // as plain text.
                ClickAction::Url { url } if safe_url(url) => Some(Target::Action(Action::Link(LinkAction::new(url.clone())))),
                ClickAction::Slide { slide } => goto(pres.slides.iter().position(|x| x.id == *slide)),
                ClickAction::NextSlide => goto(Some(i + 1)),
                ClickAction::PreviousSlide => goto(i.checked_sub(1)),
                ClickAction::FirstSlide => goto(Some(0)),
                ClickAction::LastSlide => goto(pres.slides.len().checked_sub(1)),
                _ => None,
            };
            if let Some(t) = target {
                let alt = if l.link.tooltip.is_empty() { None } else { Some(l.link.tooltip.clone()) };
                out.push(Annotation::new_link(LinkAnnotation::new(rect, t), alt));
            }
        }
        out
    }

    fn ui_font(&mut self) -> Result<(Font, std::sync::Arc<deckcraft_fonts::FontFace>)> {
        let face = FontDb::global().face(deckcraft_fonts::DEFAULT_FAMILY, "Regular");
        let f = self.font(&face)?;
        Ok((f, face))
    }

    /// Wrapped plain text (notes).
    fn paragraphs(&mut self, s: &mut Surface, text: &str, area: Rect, size: f64) -> Result<()> {
        let (font, face) = self.ui_font()?;
        let k = size / face.upem.max(1.0);
        let width = |w: &str| w.chars().map(|c| face.advance(face.glyph_for(c)) * k).sum::<f64>();
        let lh = size * 1.3;
        let mut y = area.y0 + size;
        s.set_fill(Some(grey(0x20, 1.0)));
        let space = width(" ");
        'outer: for para in text.split('\n') {
            // The line's width is kept as words are added, not measured again for each word.
            let mut line = String::new();
            let mut line_w = 0.0;
            for word in para.split(' ') {
                let word_w = width(word);
                let cand_w = if line.is_empty() { word_w } else { line_w + space + word_w };
                if cand_w > area.width() && !line.is_empty() {
                    if y > area.y1 {
                        break 'outer;
                    }
                    s.draw_text(Point::from_xy(area.x0 as f32, y as f32), font.clone(), size as f32, &line, false, TextDirection::Auto);
                    y += lh;
                    line = word.to_string();
                    line_w = word_w;
                } else {
                    if !line.is_empty() {
                        line.push(' ');
                    }
                    line.push_str(word);
                    line_w = cand_w;
                }
            }
            if y > area.y1 {
                break;
            }
            if !line.is_empty() {
                s.draw_text(Point::from_xy(area.x0 as f32, y as f32), font.clone(), size as f32, &line, false, TextDirection::Auto);
            }
            y += lh;
        }
        s.set_fill(None);
        Ok(())
    }

    /// Ruled lines for handwritten notes (3-per-page handouts).
    fn note_lines(&self, s: &mut Surface, r: Rect) {
        // Counted, not stepped: at huge coordinates adding 22 leaves y unchanged, and a huge page
        // would want millions of rules.
        let first = r.y0 + 18.0;
        let count = ((r.y1 - first) / 22.0).floor() + 1.0;
        if count.is_nan() || count < 1.0 {
            return;
        }
        s.set_stroke(Some(krilla::paint::Stroke {
            paint: krilla::color::rgb::Color::new(0xB0, 0xB0, 0xB0).into(),
            width: 0.5,
            ..Default::default()
        }));
        for i in 0..(count.min(MAX_NOTE_LINES as f64) as usize) {
            let y = first + 22.0 * i as f64;
            let mut p = kurbo::BezPath::new();
            p.move_to((r.x0, y));
            p.line_to((r.x1, y));
            if let Some(path) = to_path(&p) {
                s.draw_path(&path);
            }
        }
        s.set_stroke(None);
    }

    /// A notes or handout page's header, date, footer and page number from its master; just the
    /// page number when the deck has no such master.
    fn page_texts(&mut self, s: &mut Surface, master: Option<&deckcraft_model::Master>, pw: f64, ph: f64, n: usize) -> Result<()> {
        let Some(m) = master else { return self.footer(s, pw, ph, n) };
        // Laid out by the text engine in the placeholders, scaled from the notes size to the page:
        // wrapping, alignment and mixed formatting as the master has them.
        let pres = self.pres;
        let view = Affine::scale_non_uniform(pw / pres.notes_size.width.max(1.0), ph / pres.notes_size.height.max(1.0));
        for pt in deckcraft_render::place_master_texts(pres, m, n) {
            self.text_layer(s, &pt, view, true)?;
        }
        Ok(())
    }
    fn footer(&mut self, s: &mut Surface, pw: f64, ph: f64, n: usize) -> Result<()> {
        let (font, _) = self.ui_font()?;
        s.set_fill(Some(grey(0x60, 1.0)));
        s.draw_text(Point::from_xy((pw - 72.0) as f32, (ph - 30.0) as f32), font, 9.0, &n.to_string(), false, TextDirection::Auto);
        s.set_fill(None);
        Ok(())
    }
}

/// Byte range in `text` (the run's characters `chars`) of each of `n` glyphs, for the ToUnicode map.
/// `cells` holds each glyph's shaping-cluster head (paragraph character index); a cluster runs to the
/// next head in the run, so a ligature maps to all its characters and every glyph of a cluster shares
/// its range (krilla then writes the text once).
fn glyph_text_ranges(text: &str, chars: (usize, usize), cells: &[(usize, char)], n: usize) -> Vec<std::ops::Range<usize>> {
    let starts: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    let byte = |ci: usize| ci.checked_sub(chars.0).and_then(|i| starts.get(i).copied()).unwrap_or(text.len());
    let mut heads: Vec<usize> = cells.iter().map(|c| c.0).collect();
    heads.sort_unstable();
    heads.dedup();
    let end = |ci: usize| heads.get(heads.partition_point(|&h| h <= ci)).copied().unwrap_or(chars.1);
    (0..n).map(|gi| cells.get(gi).map_or(text.len()..text.len(), |&(ci, _)| byte(ci)..byte(end(ci)))).collect()
}

fn to_path(bp: &kurbo::BezPath) -> Option<krilla::geom::Path> {
    use kurbo::PathEl;
    let mut pb = krilla::geom::PathBuilder::new();
    for el in bp.elements() {
        match *el {
            PathEl::MoveTo(p) => pb.move_to(p.x as f32, p.y as f32),
            PathEl::LineTo(p) => pb.line_to(p.x as f32, p.y as f32),
            PathEl::QuadTo(a, p) => pb.quad_to(a.x as f32, a.y as f32, p.x as f32, p.y as f32),
            PathEl::CurveTo(a, b, p) => pb.cubic_to(a.x as f32, a.y as f32, b.x as f32, b.y as f32, p.x as f32, p.y as f32),
            PathEl::ClosePath => pb.close(),
        }
    }
    pb.finish()
}

#[cfg(test)]
mod tests;
