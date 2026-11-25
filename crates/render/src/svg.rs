//! SVG vector picture rasterizer (`image/svg+xml`): parsed with bounded `craft-xml` and drawn
//! onto `vello_cpu` at the requested pixel size, so embedded SVG-only blips and inserted SVG
//! pictures go through the same cache, crop, tile and adjustment path as PNGs and metafiles.

use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

use craft_xml::{El, Limits, Options};
use deckcraft_color::Rgba;
use kurbo::{Affine, BezPath, Circle, Ellipse, PathEl, Point, Rect, RoundedRect, Shape as _};
use vello_cpu::{RenderContext, Resources, peniko};

/// Largest rendered pixel side.
const MAX_SIDE: u32 = 8192;
/// Input size and tree bounds for hostile SVGs.
const MAX_INPUT_BYTES: usize = 16 << 20;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 100_000;
const MAX_VISITS: usize = 50_000;
const MAX_DRAW_OPS: usize = 50_000;
const MAX_USE_DEPTH: usize = 8;
const MAX_USE_EXPANSIONS: usize = 2_000;
const MAX_TOTAL_SEGMENTS: usize = 200_000;
const MAX_CSS_RULES: usize = 4_096;
const MAX_CSS_DECLS: usize = 16_384;
const MAX_DECLS_PER_BLOCK: usize = 256;
const MAX_INLINE_DECLS: usize = 256;
const MAX_INLINE_STYLE_BYTES: usize = 16_384;
const MAX_DECL_VISITS: usize = 200_000;
const MAX_DASH_ENTRIES: usize = 64;
const MAX_DASH_SEGMENTS: f64 = 10_000.0;
const MAX_TOTAL_DASHES: usize = 100_000;
const MAX_GRADIENT_STOPS: usize = 256;
const MAX_TOTAL_GRADIENT_STOPS: usize = 50_000;

/// Cheap check whether `bytes` starts like an SVG document (`<svg` after optional BOM/prolog).
pub fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    let s = match std::str::from_utf8(head) {
        Ok(v) => v,
        Err(e) => {
            // Might have split a multi-byte char at 4096; inspect the valid UTF-8 prefix.
            let Ok(v) = std::str::from_utf8(&head[..e.valid_up_to()]) else { return false };
            v
        }
    };
    looks_like_svg(s)
}

fn looks_like_svg(s: &str) -> bool {
    let mut rest = s.trim_start_matches('\u{feff}').trim_start();
    for _ in 0..16 {
        if let Some(after) = rest.strip_prefix("<?") {
            let Some(end) = after.find("?>") else { return false };
            rest = after[end + 2..].trim_start();
        } else if let Some(after) = rest.strip_prefix("<!--") {
            let Some(end) = after.find("-->") else { return false };
            rest = after[end + 3..].trim_start();
        } else if rest.len() >= 9 && rest.as_bytes()[..9].eq_ignore_ascii_case(b"<!DOCTYPE") {
            let Some(after) = skip_doctype(rest) else { return false };
            rest = after.trim_start();
        } else {
            break;
        }
    }
    let Some(after) = rest.strip_prefix('<') else { return false };
    let tag = after.split(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/').next().unwrap_or("");
    let local = tag.rsplit_once(':').map_or(tag, |(_, l)| l);
    local.eq_ignore_ascii_case("svg")
}

/// Skip a `<!DOCTYPE ...>` declaration, including quoted literals and an internal subset `[ ... ]`.
fn skip_doctype(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let mut i = 9;
    let mut in_subset = false;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            quote = Some(c);
            i += 1;
            continue;
        }
        if in_subset {
            if b[i..].starts_with(b"<!--") {
                i += 4;
                let rel = s[i..].find("-->")?;
                i += rel + 3;
                continue;
            }
            if c == b']' {
                in_subset = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'[' => {
                in_subset = true;
                i += 1;
            }
            b'>' => return Some(&s[i + 1..]),
            _ => i += 1,
        }
    }
    None
}

fn parse_svg_root(bytes: &[u8]) -> Option<El> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES || !is_svg(bytes) {
        return None;
    }
    let limits = Limits {
        max_input: MAX_INPUT_BYTES,
        max_depth: MAX_DEPTH,
        max_nodes: MAX_NODES,
        max_elements: MAX_NODES,
        max_attrs: 512,
        max_attrs_total: MAX_NODES * 4,
    };
    let root = Options::local().with_limits(limits).parse(bytes).ok()?;
    root.name.eq_ignore_ascii_case("svg").then_some(root)
}

/// Viewport `(min_x, min_y, width, height)` in SVG user units.
fn svg_viewport(root: &El) -> Option<(f64, f64, f64, f64)> {
    if let Some(vb) = root.attr("viewBox").or_else(|| root.attr("viewbox")) {
        let nums = parse_numbers(vb);
        if nums.len() >= 4 && nums[2] > 0.0 && nums[3] > 0.0 && nums.iter().take(4).all(|v| v.is_finite()) {
            return Some((nums[0], nums[1], nums[2], nums[3]));
        }
    }
    let w = root.attr("width").and_then(parse_length)?;
    let h = root.attr("height").and_then(parse_length)?;
    (w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()).then_some((0.0, 0.0, w, h))
}

/// Intrinsic size of an SVG in 96-dpi CSS pixels, for sizing and tiled picture fills.
pub fn natural_size(bytes: &[u8]) -> Option<(f64, f64)> {
    let root = parse_svg_root(bytes)?;
    let w = root.attr("width").and_then(parse_length);
    let h = root.attr("height").and_then(parse_length);
    if let (Some(w), Some(h)) = (w, h)
        && w > 0.0
        && h > 0.0
        && w.is_finite()
        && h.is_finite()
    {
        return Some((w, h));
    }
    let (_, _, vw, vh) = svg_viewport(&root)?;
    Some((vw, vh))
}

/// Rasterize an SVG document to straight RGBA with its longer side `side` px.
pub fn rasterize(bytes: &[u8], side: u32) -> Option<image::RgbaImage> {
    let root = parse_svg_root(bytes)?;
    let (vx, vy, vw, vh) = svg_viewport(&root)?;
    let side = side.clamp(16, MAX_SIDE) as f64;
    let k = side / vw.max(vh);
    let w = (vw * k).round().clamp(1.0, MAX_SIDE as f64) as u16;
    let h = (vh * k).round().clamp(1.0, MAX_SIDE as f64) as u16;

    let mut defs: HashMap<&str, &El> = HashMap::new();
    let mut css = CssSheet::default();
    index_tree(&root, &mut defs, &mut css, 0);

    let mut ctx = RenderContext::new_with(w, h, vello_cpu::RenderSettings { num_threads: 0, ..Default::default() });
    let sx = w as f64 / vw;
    let sy = h as f64 / vh;
    let base = Affine::scale_non_uniform(sx, sy) * Affine::translate((-vx, -vy));
    let mut state = RenderState {
        defs: &defs,
        css: &css,
        visits: 0,
        ops: 0,
        use_expansions: 0,
        segments: 0,
        decl_budget: MAX_DECL_VISITS,
        dash_budget: MAX_TOTAL_DASHES,
        gradient_budget: MAX_TOTAL_GRADIENT_STOPS,
        active_uses: Vec::new(),
        path_cache: HashMap::new(),
        style_cache: HashMap::new(),
        gradient_stops_cache: HashMap::new(),
    };
    if let Some(root_decls) = resolve_el_decls(&mut state, &root) {
        let root_style = Style::default().apply_patch(&root_decls.patch);
        for child in root.elements() {
            render_el(&mut ctx, &mut state, child, base, &root_style, 0);
        }
    }

    ctx.flush();
    let mut px = vec![0u8; w as usize * h as usize * 4];
    if let Some(pm) = vello_cpu::PixmapMut::new(w, h, &mut px) {
        ctx.render(pm, &mut Resources::new());
    }
    for p in px.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in p.iter_mut().take(3) {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    image::RgbaImage::from_raw(w as u32, h as u32, px)
}

struct CachedPath {
    path: BezPath,
    segments: usize,
    chord_len: f64,
    bbox: Rect,
}

struct RenderState<'a> {
    defs: &'a HashMap<&'a str, &'a El>,
    css: &'a CssSheet,
    visits: usize,
    ops: usize,
    use_expansions: usize,
    segments: usize,
    decl_budget: usize,
    dash_budget: usize,
    gradient_budget: usize,
    active_uses: Vec<&'a str>,
    path_cache: HashMap<usize, Option<Rc<CachedPath>>>,
    style_cache: HashMap<usize, Rc<ElDecls>>,
    gradient_stops_cache: HashMap<usize, Rc<[peniko::ColorStop]>>,
}

struct CssRule {
    specificity: u8,
    decls: Rc<[(String, String)]>,
}

#[derive(Default)]
struct CssSheet {
    rules: Vec<CssRule>,
    total_decls: usize,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<(usize, Option<String>)>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
}

impl CssSheet {
    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn push(&mut self, sel: &str, decls: &Rc<[(String, String)]>) -> bool {
        if self.rules.len() >= MAX_CSS_RULES {
            return false;
        }
        let idx = self.rules.len();
        if sel == "*" {
            self.rules.push(CssRule { specificity: 0, decls: Rc::clone(decls) });
            self.universal.push(idx);
            true
        } else if let Some(id) = sel.strip_prefix('#') {
            if id.is_empty() {
                return false;
            }
            self.rules.push(CssRule { specificity: 4, decls: Rc::clone(decls) });
            self.by_id.entry(id.to_string()).or_default().push(idx);
            true
        } else if let Some(class) = sel.strip_prefix('.') {
            if class.is_empty() {
                return false;
            }
            self.rules.push(CssRule { specificity: 2, decls: Rc::clone(decls) });
            self.by_class.entry(class.to_string()).or_default().push((idx, None));
            true
        } else if let Some((tag, class)) = sel.split_once('.') {
            if tag.is_empty() || class.is_empty() {
                return false;
            }
            self.rules.push(CssRule { specificity: 3, decls: Rc::clone(decls) });
            self.by_class.entry(class.to_string()).or_default().push((idx, Some(tag.to_ascii_lowercase())));
            true
        } else if !sel.is_empty() {
            self.rules.push(CssRule { specificity: 1, decls: Rc::clone(decls) });
            self.by_tag.entry(sel.to_ascii_lowercase()).or_default().push(idx);
            true
        } else {
            false
        }
    }

    fn matching_indices(&self, el: &El) -> Vec<usize> {
        if self.rules.is_empty() {
            return Vec::new();
        }
        let mut out = self.universal.clone();
        if !self.by_tag.is_empty() {
            let tag_rules = if el.name.bytes().any(|b| b.is_ascii_uppercase()) {
                self.by_tag.get(&el.name.to_ascii_lowercase())
            } else {
                self.by_tag.get(el.name.as_str())
            };
            if let Some(list) = tag_rules {
                out.extend_from_slice(list);
            }
        }
        if !self.by_class.is_empty()
            && let Some(class_attr) = el.attr("class")
        {
            for class in class_attr.split_ascii_whitespace() {
                if let Some(list) = self.by_class.get(class) {
                    for (idx, tag_filter) in list {
                        if tag_filter.as_deref().is_none_or(|t| t.eq_ignore_ascii_case(&el.name)) {
                            out.push(*idx);
                        }
                    }
                }
            }
        }
        if !self.by_id.is_empty()
            && let Some(id) = el.attr("id")
            && let Some(list) = self.by_id.get(id)
        {
            out.extend_from_slice(list);
        }
        if out.len() > 1 {
            out.sort_unstable_by_key(|&idx| (self.rules[idx].specificity, idx));
            out.dedup();
        }
        out
    }
}

fn index_tree<'a>(el: &'a El, defs: &mut HashMap<&'a str, &'a El>, css: &mut CssSheet, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(id) = el.attr("id")
        && !id.is_empty()
    {
        defs.entry(id).or_insert(el);
    }
    if el.name.eq_ignore_ascii_case("style") {
        parse_css(&el.text(), css);
    }
    for child in el.elements() {
        index_tree(child, defs, css, depth + 1);
    }
}

fn parse_css(text: &str, out: &mut CssSheet) {
    if out.rules.len() >= MAX_CSS_RULES || out.total_decls >= MAX_CSS_DECLS {
        return;
    }
    let clean: Cow<'_, str> = if text.contains("/*") {
        let mut buf = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("/*") {
            buf.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("*/") else {
                rest = "";
                break;
            };
            rest = &after[end + 2..];
        }
        buf.push_str(rest);
        Cow::Owned(buf)
    } else {
        Cow::Borrowed(text)
    };

    for block in clean.split('}') {
        if out.rules.len() >= MAX_CSS_RULES || out.total_decls >= MAX_CSS_DECLS {
            break;
        }
        let Some((sels, body)) = block.split_once('{') else { continue };
        let cap = MAX_CSS_DECLS.saturating_sub(out.total_decls).min(MAX_DECLS_PER_BLOCK);
        if cap == 0 {
            break;
        }
        let decls = parse_style_decls_capped(body, cap);
        if decls.is_empty() {
            continue;
        }
        let decl_count = decls.len();
        let decls: Rc<[(String, String)]> = Rc::from(decls);
        let mut pushed_any = false;
        for sel in sels.split(',') {
            if out.rules.len() >= MAX_CSS_RULES {
                break;
            }
            let sel = sel.trim();
            if !sel.is_empty() && out.push(sel, &decls) {
                pushed_any = true;
            }
        }
        if pushed_any {
            out.total_decls += decl_count;
        }
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn parse_style_decls_capped(s: &str, max_decls: usize) -> Vec<(String, String)> {
    if max_decls == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for item in s.split(';') {
        if out.len() >= max_decls {
            break;
        }
        let Some((k, v)) = item.split_once(':') else { continue };
        let k = k.trim();
        let v = v.trim().trim_end_matches("!important").trim();
        if !k.is_empty() && !v.is_empty() {
            out.push((k.to_ascii_lowercase(), v.to_string()));
        }
    }
    out
}

#[derive(Clone, Debug)]
enum Paint {
    None,
    Color(Rgba),
    CurrentColor,
    Url(String),
}

#[derive(Clone, Debug)]
struct Style {
    fill: Paint,
    stroke: Paint,
    fill_opacity: f64,
    stroke_opacity: f64,
    stroke_width: f64,
    fill_rule: peniko::Fill,
    stroke_cap: kurbo::Cap,
    stroke_join: kurbo::Join,
    stroke_miter: f64,
    stroke_dash: Option<Vec<f64>>,
    stroke_dashoffset: f64,
    color: Rgba,
    visible: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fill: Paint::Color(Rgba::BLACK),
            stroke: Paint::None,
            fill_opacity: 1.0,
            stroke_opacity: 1.0,
            stroke_width: 1.0,
            fill_rule: peniko::Fill::NonZero,
            stroke_cap: kurbo::Cap::Butt,
            stroke_join: kurbo::Join::Miter,
            stroke_miter: 4.0,
            stroke_dash: None,
            stroke_dashoffset: 0.0,
            color: Rgba::BLACK,
            visible: true,
        }
    }
}

#[derive(Default)]
struct StylePatch {
    fill: Option<Paint>,
    stroke: Option<Paint>,
    fill_opacity: Option<f64>,
    stroke_opacity: Option<f64>,
    stroke_width: Option<f64>,
    fill_rule: Option<peniko::Fill>,
    stroke_cap: Option<kurbo::Cap>,
    stroke_join: Option<kurbo::Join>,
    stroke_miter: Option<f64>,
    stroke_dash: Option<Option<Vec<f64>>>,
    stroke_dashoffset: Option<f64>,
    color: Option<Rgba>,
    visible: Option<bool>,
}

struct ElDecls {
    display_none: bool,
    opacity: f64,
    clip_path: Option<String>,
    patch: StylePatch,
}

impl Style {
    fn apply_patch(&self, patch: &StylePatch) -> Self {
        let mut s = self.clone();
        if let Some(fill) = &patch.fill {
            s.fill = fill.clone();
        }
        if let Some(stroke) = &patch.stroke {
            s.stroke = stroke.clone();
        }
        if let Some(v) = patch.fill_opacity {
            s.fill_opacity = v;
        }
        if let Some(v) = patch.stroke_opacity {
            s.stroke_opacity = v;
        }
        if let Some(v) = patch.stroke_width {
            s.stroke_width = v;
        }
        if let Some(v) = patch.fill_rule {
            s.fill_rule = v;
        }
        if let Some(v) = patch.stroke_cap {
            s.stroke_cap = v;
        }
        if let Some(v) = patch.stroke_join {
            s.stroke_join = v;
        }
        if let Some(v) = patch.stroke_miter {
            s.stroke_miter = v;
        }
        if let Some(dash) = &patch.stroke_dash {
            s.stroke_dash = dash.clone();
        }
        if let Some(v) = patch.stroke_dashoffset {
            s.stroke_dashoffset = v;
        }
        if let Some(v) = patch.color {
            s.color = v;
        }
        if let Some(v) = patch.visible {
            s.visible = v;
        }
        s
    }
}

impl ElDecls {
    fn apply_decl(&mut self, key: &str, raw: &str) {
        let val = raw.trim();
        if val.is_empty() || val.eq_ignore_ascii_case("inherit") {
            return;
        }
        match key {
            "display" => {
                self.display_none = val.eq_ignore_ascii_case("none");
            }
            "opacity" => {
                if let Some(v) = parse_unit_float(val) {
                    self.opacity = v.clamp(0.0, 1.0);
                }
            }
            "clip-path" => {
                if val.eq_ignore_ascii_case("none") {
                    self.clip_path = None;
                } else if let Some(id) = extract_url_id(val) {
                    self.clip_path = Some(id);
                }
            }
            "fill" => {
                if let Some(p) = parse_paint(val) {
                    self.patch.fill = Some(p);
                }
            }
            "stroke" => {
                if let Some(p) = parse_paint(val) {
                    self.patch.stroke = Some(p);
                }
            }
            "color" => {
                if let Some(c) = parse_color(val) {
                    self.patch.color = Some(c);
                }
            }
            "fill-opacity" => {
                if let Some(v) = parse_unit_float(val) {
                    self.patch.fill_opacity = Some(v.clamp(0.0, 1.0));
                }
            }
            "stroke-opacity" => {
                if let Some(v) = parse_unit_float(val) {
                    self.patch.stroke_opacity = Some(v.clamp(0.0, 1.0));
                }
            }
            "stroke-width" => {
                if let Some(v) = parse_length(val)
                    && v >= 0.0
                    && v.is_finite()
                {
                    self.patch.stroke_width = Some(v);
                }
            }
            "fill-rule" => {
                if val.eq_ignore_ascii_case("evenodd") {
                    self.patch.fill_rule = Some(peniko::Fill::EvenOdd);
                } else if val.eq_ignore_ascii_case("nonzero") {
                    self.patch.fill_rule = Some(peniko::Fill::NonZero);
                }
            }
            "stroke-linecap" => match val {
                "round" => self.patch.stroke_cap = Some(kurbo::Cap::Round),
                "square" => self.patch.stroke_cap = Some(kurbo::Cap::Square),
                "butt" => self.patch.stroke_cap = Some(kurbo::Cap::Butt),
                _ => {}
            },
            "stroke-linejoin" => match val {
                "round" => self.patch.stroke_join = Some(kurbo::Join::Round),
                "bevel" => self.patch.stroke_join = Some(kurbo::Join::Bevel),
                "miter" => self.patch.stroke_join = Some(kurbo::Join::Miter),
                _ => {}
            },
            "stroke-miterlimit" => {
                if let Ok(v) = val.parse::<f64>()
                    && v.is_finite()
                    && v >= 1.0
                {
                    self.patch.stroke_miter = Some(v);
                }
            }
            "stroke-dasharray" => {
                if val.eq_ignore_ascii_case("none") {
                    self.patch.stroke_dash = Some(None);
                } else {
                    let all_nums = parse_numbers(val);
                    if all_nums.is_empty() || all_nums.iter().any(|v| *v < 0.0 || !v.is_finite()) {
                        // Invalid dasharray (empty or contains negative): ignore, preserving inherited value.
                    } else if all_nums.iter().all(|v| *v == 0.0) {
                        // All-zero dasharray (`0` or `0,0`) resets to solid stroke.
                        self.patch.stroke_dash = Some(None);
                    } else {
                        let nums: Vec<f64> = all_nums.into_iter().take(MAX_DASH_ENTRIES).collect();
                        if nums.iter().any(|v| *v > 0.0) {
                            self.patch.stroke_dash = Some(Some(nums));
                        } else {
                            self.patch.stroke_dash = Some(None);
                        }
                    }
                }
            }
            "stroke-dashoffset" => {
                if let Some(v) = parse_length(val)
                    && v.is_finite()
                {
                    self.patch.stroke_dashoffset = Some(v);
                }
            }
            "visibility" => {
                self.patch.visible = Some(!matches!(val, "hidden" | "collapse"));
            }
            _ => {}
        }
    }
}

fn resolve_el_decls(st: &mut RenderState<'_>, el: &El) -> Option<Rc<ElDecls>> {
    let key = (el as *const El) as usize;
    if let Some(hit) = st.style_cache.get(&key) {
        return Some(Rc::clone(hit));
    }
    let mut decls = ElDecls { display_none: false, opacity: 1.0, clip_path: None, patch: StylePatch::default() };
    // 1. Presentation attributes (lowest CSS specificity). Not charged to `decl_budget`:
    //    each element is resolved once (`style_cache`) and bounded by input XML size.
    for (k, v) in &el.attrs {
        if k == "style" {
            continue;
        }
        decls.apply_decl(k, v);
    }
    // 2. Matching <style> rules (*, tag, .class, tag.class, #id) by (specificity, source order).
    //    Only stylesheet rules multiply (rules x elements); if the budget runs out, stop drawing
    //    rather than painting unstyled elements or ignoring `display:none`.
    if !st.css.is_empty() {
        for idx in st.css.matching_indices(el) {
            for (k, v) in st.css.rules[idx].decls.iter() {
                if st.decl_budget == 0 {
                    st.visits = MAX_VISITS;
                    return None;
                }
                st.decl_budget -= 1;
                decls.apply_decl(k, v);
            }
        }
    }
    // 3. Inline style="..." (highest specificity), bounded in raw bytes and declaration count
    //    and cached once per element (not charged to stylesheet `decl_budget`).
    if let Some(inline) = el.attr("style") {
        let bounded = truncate_utf8(inline, MAX_INLINE_STYLE_BYTES);
        let mut count = 0usize;
        for item in bounded.split(';') {
            if count >= MAX_INLINE_DECLS {
                break;
            }
            let Some((k, v)) = item.split_once(':') else { continue };
            let k = k.trim();
            let v = v.trim().trim_end_matches("!important").trim();
            if k.is_empty() || v.is_empty() {
                continue;
            }
            count += 1;
            let key_cow: Cow<'_, str> = if k.bytes().any(|b| b.is_ascii_uppercase()) { Cow::Owned(k.to_ascii_lowercase()) } else { Cow::Borrowed(k) };
            decls.apply_decl(&key_cow, v);
        }
    }
    let rc = Rc::new(decls);
    st.style_cache.insert(key, Rc::clone(&rc));
    Some(rc)
}

fn render_el<'a>(ctx: &mut RenderContext, st: &mut RenderState<'a>, el: &'a El, xfrm: Affine, inherited: &Style, depth: usize) {
    if depth > MAX_DEPTH || st.visits >= MAX_VISITS || st.ops >= MAX_DRAW_OPS || st.segments >= MAX_TOTAL_SEGMENTS {
        return;
    }
    st.visits += 1;
    let name = el.name.as_str();
    if matches!(
        name,
        "defs" | "style" | "title" | "desc" | "metadata" | "clipPath" | "linearGradient" | "radialGradient" | "symbol" | "mask" | "filter"
    ) {
        return;
    }
    let Some(el_decls) = resolve_el_decls(st, el) else {
        return;
    };
    if el_decls.display_none || el_decls.opacity <= 0.0 {
        return;
    }

    let local = el.attr("transform").map(parse_transform).unwrap_or(Affine::IDENTITY);
    let m = xfrm * local;
    let style = inherited.apply_patch(&el_decls.patch);
    let opacity = el_decls.opacity;

    let clip = el_decls.clip_path.as_deref().and_then(|id| clip_path(st, id, m));
    let mut pushed = 0usize;
    if let Some((cp, rule)) = &clip {
        ctx.set_fill_rule(*rule);
        ctx.set_transform(Affine::IDENTITY);
        ctx.push_clip_layer(cp);
        pushed += 1;
    }
    if opacity < 0.999 {
        ctx.push_layer(None, None, Some(opacity as f32), None, None);
        pushed += 1;
    }

    match name {
        "g" | "a" => {
            let pushed_id = push_active_id(st, el);
            for child in el.elements() {
                render_el(ctx, st, child, m, &style, depth + 1);
            }
            if pushed_id {
                st.active_uses.pop();
            }
        }
        "svg" => {
            let pushed_id = push_active_id(st, el);
            let nested = nested_svg_transform(el, m);
            for child in el.elements() {
                render_el(ctx, st, child, nested, &style, depth + 1);
            }
            if pushed_id {
                st.active_uses.pop();
            }
        }
        "use" => {
            if depth + MAX_USE_DEPTH <= MAX_DEPTH + 4 {
                render_use(ctx, st, el, m, &style, depth + 1);
            }
        }
        _ => {
            if style.visible
                && let Some(cached) = cached_path(st, el)
            {
                let cost = cached.segments.max(1);
                if st.segments.saturating_add(cost) > MAX_TOTAL_SEGMENTS {
                    st.segments = MAX_TOTAL_SEGMENTS;
                } else {
                    st.segments += cost;
                    st.ops += 1;
                    draw_shape(ctx, st, &cached, m, &style);
                }
            }
        }
    }

    for _ in 0..pushed {
        ctx.pop_layer();
    }
}

fn push_active_id<'a>(st: &mut RenderState<'a>, el: &'a El) -> bool {
    if let Some(id) = el.attr("id")
        && !id.is_empty()
        && !st.active_uses.contains(&id)
    {
        st.active_uses.push(id);
        return true;
    }
    false
}

fn nested_svg_transform(el: &El, m: Affine) -> Affine {
    let x = el.attr("x").and_then(parse_length).unwrap_or(0.0);
    let y = el.attr("y").and_then(parse_length).unwrap_or(0.0);
    let mut out = m * Affine::translate((x, y));
    if let Some((vx, vy, vw, vh)) = svg_viewport(el)
        && let (Some(w), Some(h)) = (el.attr("width").and_then(parse_length), el.attr("height").and_then(parse_length))
        && w > 0.0
        && h > 0.0
    {
        out *= Affine::scale_non_uniform(w / vw, h / vh) * Affine::translate((-vx, -vy));
    }
    out
}

fn render_use<'a>(ctx: &mut RenderContext, st: &mut RenderState<'a>, el: &'a El, m: Affine, style: &Style, depth: usize) {
    if st.use_expansions >= MAX_USE_EXPANSIONS {
        return;
    }
    let Some(href) = el.attr("href").and_then(|s| s.trim().strip_prefix('#')) else { return };
    if st.active_uses.contains(&href) {
        return;
    }
    let Some(&target) = st.defs.get(href) else { return };
    st.use_expansions += 1;
    st.active_uses.push(href);
    let dx = el.attr("x").and_then(parse_length).unwrap_or(0.0);
    let dy = el.attr("y").and_then(parse_length).unwrap_or(0.0);
    let um = m * Affine::translate((dx, dy));
    if target.name == "symbol" {
        for child in target.elements() {
            render_el(ctx, st, child, um, style, depth + 1);
        }
    } else {
        render_el(ctx, st, target, um, style, depth + 1);
    }
    st.active_uses.pop();
}

fn cached_path(st: &mut RenderState<'_>, el: &El) -> Option<Rc<CachedPath>> {
    let key = (el as *const El) as usize;
    if let Some(hit) = st.path_cache.get(&key) {
        return hit.clone();
    }
    let built = element_path(el).map(|path| {
        let (segments, chord_len) = path_metrics(&path);
        let bbox = path.bounding_box();
        Rc::new(CachedPath { path, segments, chord_len, bbox })
    });
    st.path_cache.insert(key, built.clone());
    built
}

/// Segment count and control-polygon chord-length upper bound in O(elements).
fn path_metrics(path: &BezPath) -> (usize, f64) {
    let mut segments = 0usize;
    let mut len = 0.0f64;
    let mut cur = Point::ZERO;
    let mut start = Point::ZERO;
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => {
                cur = p;
                start = p;
            }
            PathEl::LineTo(p) => {
                segments += 1;
                len += cur.distance(p);
                cur = p;
            }
            PathEl::QuadTo(p1, p2) => {
                segments += 1;
                len += cur.distance(p1) + p1.distance(p2);
                cur = p2;
            }
            PathEl::CurveTo(p1, p2, p3) => {
                segments += 1;
                len += cur.distance(p1) + p1.distance(p2) + p2.distance(p3);
                cur = p3;
            }
            PathEl::ClosePath => {
                segments += 1;
                len += cur.distance(start);
                cur = start;
            }
        }
    }
    (segments, len)
}

fn clip_path(st: &mut RenderState<'_>, id: &str, m: Affine) -> Option<(BezPath, peniko::Fill)> {
    let &cp = st.defs.get(id)?;
    if cp.name != "clipPath" {
        return None;
    }
    let mut combined = BezPath::new();
    let mut rule = peniko::Fill::NonZero;
    for child in cp.elements() {
        if let Some(cached) = cached_path(st, child) {
            let cost = cached.segments.max(1);
            if st.segments.saturating_add(cost) > MAX_TOTAL_SEGMENTS {
                st.segments = MAX_TOTAL_SEGMENTS;
                break;
            }
            st.segments += cost;
            let mut p = cached.path.clone();
            let local = child.attr("transform").map(parse_transform).unwrap_or(Affine::IDENTITY);
            p.apply_affine(m * local);
            combined.extend(p.elements().iter().copied());
            if child.attr("clip-rule").is_some_and(|v| v.eq_ignore_ascii_case("evenodd")) {
                rule = peniko::Fill::EvenOdd;
            }
        }
    }
    (!combined.elements().is_empty()).then_some((combined, rule))
}

fn draw_shape(ctx: &mut RenderContext, st: &mut RenderState<'_>, cached: &CachedPath, m: Affine, style: &Style) {
    let path = &cached.path;
    let bbox = cached.bbox;
    ctx.set_transform(m);
    if !matches!(style.fill, Paint::None) && style.fill_opacity > 0.0 {
        set_paint(ctx, st, &style.fill, style.color, style.fill_opacity, bbox);
        ctx.set_fill_rule(style.fill_rule);
        ctx.fill_path(path);
    }
    if !matches!(style.stroke, Paint::None) && style.stroke_width > 0.0 && style.stroke_opacity > 0.0 {
        set_paint(ctx, st, &style.stroke, style.color, style.stroke_opacity, bbox);
        let mut stroke =
            kurbo::Stroke::new(style.stroke_width).with_caps(style.stroke_cap).with_join(style.stroke_join).with_miter_limit(style.stroke_miter);
        if let Some(dashes) = &style.stroke_dash
            && !dashes.is_empty()
            && dashes.len() <= MAX_DASH_ENTRIES
        {
            let period: f64 = dashes.iter().sum();
            let [a, b, c, d, _, _] = m.as_coeffs();
            let px_scale = a.hypot(b).max(c.hypot(d));
            let est_dashes = if period > 0.0 && period.is_finite() && cached.chord_len.is_finite() {
                (cached.chord_len / period) * (dashes.len() as f64)
            } else {
                f64::INFINITY
            };
            let use_dashes = period > 0.0
                && period.is_finite()
                && px_scale.is_finite()
                && period * px_scale >= 0.25
                && est_dashes.is_finite()
                && est_dashes <= MAX_DASH_SEGMENTS;
            if use_dashes {
                let cost = (est_dashes.ceil() as usize).max(1);
                if st.dash_budget >= cost {
                    st.dash_budget -= cost;
                    let full_cycle = if dashes.len() % 2 == 1 { period * 2.0 } else { period };
                    let offset = style.stroke_dashoffset.rem_euclid(full_cycle);
                    stroke = stroke.with_dashes(offset, dashes.as_slice());
                }
            }
        }
        ctx.set_stroke(stroke);
        ctx.stroke_path(path);
    }
}

enum ResolvedPaint {
    Solid(peniko::Color),
    Gradient(peniko::Gradient, Affine),
}

fn set_paint(ctx: &mut RenderContext, st: &mut RenderState<'_>, paint: &Paint, current: Rgba, opacity: f64, bbox: Rect) {
    match paint {
        Paint::None => {}
        Paint::Color(c) => {
            ctx.reset_paint_transform();
            ctx.set_paint(to_peniko(*c, opacity));
        }
        Paint::CurrentColor => {
            ctx.reset_paint_transform();
            ctx.set_paint(to_peniko(current, opacity));
        }
        Paint::Url(id) => match resolve_gradient(st, id, opacity, bbox) {
            Some(ResolvedPaint::Gradient(grad, pt_xfrm)) => {
                ctx.set_paint(grad);
                if pt_xfrm != Affine::IDENTITY {
                    ctx.set_paint_transform(pt_xfrm);
                } else {
                    ctx.reset_paint_transform();
                }
            }
            Some(ResolvedPaint::Solid(c)) => {
                ctx.reset_paint_transform();
                ctx.set_paint(c);
            }
            None => {
                ctx.reset_paint_transform();
                ctx.set_paint(to_peniko(Rgba::BLACK, 0.0));
            }
        },
    }
}

fn to_peniko(c: Rgba, opacity: f64) -> peniko::Color {
    let a = ((f64::from(c.a) * opacity).round().clamp(0.0, 255.0)) as u8;
    peniko::Color::from_rgba8(c.r, c.g, c.b, a)
}

fn resolve_gradient(st: &mut RenderState<'_>, id: &str, opacity: f64, bbox: Rect) -> Option<ResolvedPaint> {
    let &el = st.defs.get(id)?;
    if el.name != "linearGradient" && el.name != "radialGradient" {
        return None;
    }
    let chain = collect_gradient_chain(st, el);
    let stops = cached_gradient_stops(st, el, &chain);
    if stops.is_empty() {
        return None;
    }
    let cost = stops.len();
    if st.gradient_budget < cost {
        let first = if opacity < 0.9999 { stops[0].color.multiply_alpha(opacity as f32) } else { stops[0].color };
        return Some(ResolvedPaint::Solid(first.to_alpha_color::<peniko::color::Srgb>()));
    }
    st.gradient_budget -= cost;

    let scaled_stops: Option<Vec<peniko::ColorStop>> = if opacity < 0.9999 {
        let alpha = opacity as f32;
        Some(stops.iter().map(|s| peniko::ColorStop { offset: s.offset, color: s.color.multiply_alpha(alpha) }).collect())
    } else {
        None
    };
    let stop_slice: &[peniko::ColorStop] = scaled_stops.as_deref().unwrap_or(&stops);

    let user_space = grad_attr(&chain, "gradientUnits").is_some_and(|v| v == "userSpaceOnUse");
    let spread = match grad_attr(&chain, "spreadMethod") {
        Some("reflect") => peniko::Extend::Reflect,
        Some("repeat") => peniko::Extend::Repeat,
        _ => peniko::Extend::Pad,
    };
    let gxfrm = grad_attr(&chain, "gradientTransform").map(parse_transform).unwrap_or(Affine::IDENTITY);
    let paint_xfrm = if user_space {
        gxfrm
    } else {
        Affine::translate((bbox.x0, bbox.y0)) * Affine::scale_non_uniform(bbox.width().max(1e-6), bbox.height().max(1e-6)) * gxfrm
    };

    if el.name == "linearGradient" {
        let p = |k: &str, def: f64| grad_attr(&chain, k).and_then(|v| parse_grad_coord(v, user_space)).unwrap_or(def);
        let (x1, y1, x2, y2) = (p("x1", 0.0), p("y1", 0.0), p("x2", 1.0), p("y2", 0.0));
        let grad = peniko::Gradient::new_linear(Point::new(x1, y1), Point::new(x2, y2)).with_stops(stop_slice).with_extend(spread);
        return Some(ResolvedPaint::Gradient(grad, paint_xfrm));
    }
    if el.name == "radialGradient" {
        let p = |k: &str, def: f64| grad_attr(&chain, k).and_then(|v| parse_grad_coord(v, user_space)).unwrap_or(def);
        let cx = p("cx", 0.5);
        let cy = p("cy", 0.5);
        let r = p("r", 0.5).max(1e-6);
        let fx = p("fx", cx);
        let fy = p("fy", cy);
        let grad =
            peniko::Gradient::new_two_point_radial(Point::new(fx, fy), 0.0, Point::new(cx, cy), r as f32).with_stops(stop_slice).with_extend(spread);
        return Some(ResolvedPaint::Gradient(grad, paint_xfrm));
    }
    None
}

fn collect_gradient_chain<'a>(st: &RenderState<'a>, start: &'a El) -> Vec<&'a El> {
    let mut chain = vec![start];
    let mut cur = start;
    for _ in 0..8 {
        let Some(href) = cur.attr("href").and_then(|s| s.trim().strip_prefix('#')) else { break };
        let Some(&next) = st.defs.get(href) else { break };
        chain.push(next);
        cur = next;
    }
    chain
}

fn grad_attr<'a>(chain: &[&'a El], key: &str) -> Option<&'a str> {
    chain.iter().find_map(|el| el.attr(key))
}

fn cached_gradient_stops(st: &mut RenderState<'_>, el: &El, chain: &[&El]) -> Rc<[peniko::ColorStop]> {
    let el_key = (el as *const El) as usize;
    if let Some(hit) = st.gradient_stops_cache.get(&el_key) {
        return Rc::clone(hit);
    }
    let Some(&owner) = chain.iter().find(|cand| cand.elements().any(|c| c.name == "stop")) else {
        let empty: Rc<[peniko::ColorStop]> = Rc::from([]);
        st.gradient_stops_cache.insert(el_key, Rc::clone(&empty));
        return empty;
    };
    let owner_key = (owner as *const El) as usize;
    if let Some(hit) = st.gradient_stops_cache.get(&owner_key).cloned() {
        st.gradient_stops_cache.insert(el_key, Rc::clone(&hit));
        return hit;
    }
    let mut out = Vec::new();
    for stop in owner.elements().filter(|c| c.name == "stop").take(MAX_GRADIENT_STOPS) {
        let offset = stop.attr("offset").and_then(parse_unit_float).unwrap_or(0.0).clamp(0.0, 1.0) as f32;
        let mut color = Rgba::BLACK;
        let mut sop = 1.0;
        if let Some(c) = stop.attr("stop-color").and_then(parse_color) {
            color = c;
        }
        if let Some(o) = stop.attr("stop-opacity").and_then(parse_unit_float) {
            sop = o.clamp(0.0, 1.0);
        }
        if let Some(style) = stop.attr("style") {
            let bounded = truncate_utf8(style, MAX_INLINE_STYLE_BYTES);
            for (k, v) in parse_style_decls_capped(bounded, MAX_INLINE_DECLS) {
                if k == "stop-color"
                    && let Some(c) = parse_color(&v)
                {
                    color = c;
                } else if k == "stop-opacity"
                    && let Some(o) = parse_unit_float(&v)
                {
                    sop = o.clamp(0.0, 1.0);
                }
            }
        }
        let c = to_peniko(color, sop);
        out.push(peniko::ColorStop { offset, color: c.into() });
    }
    let rc: Rc<[peniko::ColorStop]> = Rc::from(out);
    st.gradient_stops_cache.insert(owner_key, Rc::clone(&rc));
    if el_key != owner_key {
        st.gradient_stops_cache.insert(el_key, Rc::clone(&rc));
    }
    rc
}

fn parse_grad_coord(s: &str, user_space: bool) -> Option<f64> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        return pct.trim().parse::<f64>().ok().map(|v| v / 100.0);
    }
    if user_space { parse_length(s) } else { s.parse::<f64>().ok() }
}

fn element_path(el: &El) -> Option<BezPath> {
    match el.name.as_str() {
        "path" => el.attr("d").and_then(|d| BezPath::from_svg(d).ok()),
        "rect" => rect_path(el),
        "circle" => {
            let cx = el.attr("cx").and_then(parse_length).unwrap_or(0.0);
            let cy = el.attr("cy").and_then(parse_length).unwrap_or(0.0);
            let r = el.attr("r").and_then(parse_length)?;
            (r > 0.0 && r.is_finite()).then(|| Circle::new((cx, cy), r).to_path(0.1))
        }
        "ellipse" => {
            let cx = el.attr("cx").and_then(parse_length).unwrap_or(0.0);
            let cy = el.attr("cy").and_then(parse_length).unwrap_or(0.0);
            let rx = el.attr("rx").and_then(parse_length)?;
            let ry = el.attr("ry").and_then(parse_length)?;
            (rx > 0.0 && ry > 0.0 && rx.is_finite() && ry.is_finite()).then(|| Ellipse::new((cx, cy), (rx, ry), 0.0).to_path(0.1))
        }
        "line" => {
            let x1 = el.attr("x1").and_then(parse_length).unwrap_or(0.0);
            let y1 = el.attr("y1").and_then(parse_length).unwrap_or(0.0);
            let x2 = el.attr("x2").and_then(parse_length).unwrap_or(0.0);
            let y2 = el.attr("y2").and_then(parse_length).unwrap_or(0.0);
            let mut p = BezPath::new();
            p.move_to((x1, y1));
            p.line_to((x2, y2));
            Some(p)
        }
        "polyline" => poly_path(el.attr("points")?, false),
        "polygon" => poly_path(el.attr("points")?, true),
        _ => None,
    }
}

fn rect_path(el: &El) -> Option<BezPath> {
    let x = el.attr("x").and_then(parse_length).unwrap_or(0.0);
    let y = el.attr("y").and_then(parse_length).unwrap_or(0.0);
    let w = el.attr("width").and_then(parse_length)?;
    let h = el.attr("height").and_then(parse_length)?;
    if !(w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()) {
        return None;
    }
    let rx = el.attr("rx").and_then(parse_length);
    let ry = el.attr("ry").and_then(parse_length);
    let r = match (rx, ry) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => 0.0,
    };
    let rect = Rect::new(x, y, x + w, y + h);
    if r > 0.0 && r.is_finite() { Some(RoundedRect::from_rect(rect, r.min(w * 0.5).min(h * 0.5)).to_path(0.1)) } else { Some(rect.to_path(0.1)) }
}

fn poly_path(points: &str, close: bool) -> Option<BezPath> {
    let nums = parse_numbers(points);
    if nums.len() < 4 {
        return None;
    }
    let mut p = BezPath::new();
    p.move_to((nums[0], nums[1]));
    for pair in nums[2..].as_chunks::<2>().0 {
        p.line_to((pair[0], pair[1]));
    }
    if close {
        p.close_path();
    }
    Some(p)
}

fn parse_paint(val: &str) -> Option<Paint> {
    let v = val.trim();
    if v.eq_ignore_ascii_case("none") {
        return Some(Paint::None);
    }
    if v.eq_ignore_ascii_case("currentColor") {
        return Some(Paint::CurrentColor);
    }
    if let Some(id) = extract_url_id(v) {
        return Some(Paint::Url(id));
    }
    parse_color(v).map(Paint::Color)
}

fn extract_url_id(s: &str) -> Option<String> {
    let rest = s.trim().strip_prefix("url(")?;
    let inner = rest.split(')').next()?.trim().trim_matches(|c| c == '"' || c == '\'');
    let id = inner.strip_prefix('#')?;
    (!id.is_empty()).then(|| id.to_string())
}

fn parse_unit_float(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        return pct.trim().parse::<f64>().ok().map(|v| v / 100.0);
    }
    s.parse::<f64>().ok()
}

fn parse_length(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() || s.ends_with('%') {
        return None;
    }
    let (num, factor) = if let Some(n) = s.strip_suffix("px") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix("pt") {
        (n, 96.0 / 72.0)
    } else if let Some(n) = s.strip_suffix("in") {
        (n, 96.0)
    } else if let Some(n) = s.strip_suffix("cm") {
        (n, 96.0 / 2.54)
    } else if let Some(n) = s.strip_suffix("mm") {
        (n, 96.0 / 25.4)
    } else if let Some(n) = s.strip_suffix("pc") {
        (n, 16.0)
    } else {
        (s, 1.0)
    };
    num.trim().parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| v * factor)
}

fn parse_numbers(s: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        if matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let mut seen_dot = false;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || (!seen_dot && bytes[i] == b'.')) {
            if bytes[i] == b'.' {
                seen_dot = true;
            }
            i += 1;
        }
        if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
            let exp_start = i;
            i += 1;
            if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
                i += 1;
            }
            let digits_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == digits_start {
                i = exp_start;
            }
        }
        if i == start {
            i += 1;
            continue;
        }
        if let Ok(v) = s[start..i].parse::<f64>()
            && v.is_finite()
        {
            out.push(v);
        }
    }
    out
}

fn parse_transform(s: &str) -> Affine {
    let mut out = Affine::IDENTITY;
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(lp) = rest.find('(') else { break };
        let kind = rest[..lp].trim().trim_start_matches(',').trim();
        let after = &rest[lp + 1..];
        let Some(rp) = after.find(')') else { break };
        let args = parse_numbers(&after[..rp]);
        let step = match kind {
            "matrix" if args.len() >= 6 => Affine::new([args[0], args[1], args[2], args[3], args[4], args[5]]),
            "translate" if !args.is_empty() => Affine::translate((args[0], args.get(1).copied().unwrap_or(0.0))),
            "scale" if !args.is_empty() => Affine::scale_non_uniform(args[0], args.get(1).copied().unwrap_or(args[0])),
            "rotate" if !args.is_empty() => {
                let rad = args[0].to_radians();
                if args.len() >= 3 {
                    Affine::translate((args[1], args[2])) * Affine::rotate(rad) * Affine::translate((-args[1], -args[2]))
                } else {
                    Affine::rotate(rad)
                }
            }
            "skewX" if !args.is_empty() => Affine::new([1.0, 0.0, args[0].to_radians().tan(), 1.0, 0.0, 0.0]),
            "skewY" if !args.is_empty() => Affine::new([1.0, args[0].to_radians().tan(), 0.0, 1.0, 0.0, 0.0]),
            _ => Affine::IDENTITY,
        };
        out *= step;
        rest = after[rp + 1..].trim();
    }
    out
}

fn parse_color(val: &str) -> Option<Rgba> {
    let v = val.trim();
    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(inner) = v.strip_prefix("rgb(").or_else(|| v.strip_prefix("rgba(")).and_then(|s| s.strip_suffix(')')) {
        return parse_rgb_fn(inner);
    }
    named_color(v)
}

fn parse_hex_color(hex: &str) -> Option<Rgba> {
    let b = hex.as_bytes();
    let d = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let pair = |hi: u8, lo: u8| Some((d(hi)? << 4) | d(lo)?);
    match b.len() {
        3 => Some(Rgba::rgb(pair(b[0], b[0])?, pair(b[1], b[1])?, pair(b[2], b[2])?)),
        4 => Some(Rgba::rgba(pair(b[0], b[0])?, pair(b[1], b[1])?, pair(b[2], b[2])?, pair(b[3], b[3])?)),
        6 => Some(Rgba::rgb(pair(b[0], b[1])?, pair(b[2], b[3])?, pair(b[4], b[5])?)),
        8 => Some(Rgba::rgba(pair(b[0], b[1])?, pair(b[2], b[3])?, pair(b[4], b[5])?, pair(b[6], b[7])?)),
        _ => None,
    }
}

fn parse_rgb_fn(inner: &str) -> Option<Rgba> {
    let parts: Vec<&str> = inner.split(|c: char| c == ',' || c == '/' || c.is_ascii_whitespace()).filter(|s| !s.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    let chan = |s: &str| -> Option<u8> {
        if let Some(p) = s.strip_suffix('%') {
            p.parse::<f64>().ok().map(|v| (v * 2.55).round().clamp(0.0, 255.0) as u8)
        } else {
            s.parse::<f64>().ok().map(|v| v.round().clamp(0.0, 255.0) as u8)
        }
    };
    let a = parts.get(3).and_then(|s| parse_unit_float(s)).map_or(255, |v| (v * 255.0).round().clamp(0.0, 255.0) as u8);
    Some(Rgba::rgba(chan(parts[0])?, chan(parts[1])?, chan(parts[2])?, a))
}

fn named_color(s: &str) -> Option<Rgba> {
    let lower = s.to_ascii_lowercase();
    let rgb = match lower.as_str() {
        "transparent" => return Some(Rgba::rgba(0, 0, 0, 0)),
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "dimgray" | "dimgrey" => (105, 105, 105),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "navy" => (0, 0, 128),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        "orange" => (255, 165, 0),
        "orangered" => (255, 69, 0),
        "gold" => (255, 215, 0),
        "pink" => (255, 192, 203),
        "brown" => (165, 42, 42),
        "darkblue" => (0, 0, 139),
        "darkgreen" => (0, 100, 0),
        "darkred" => (139, 0, 0),
        "lightblue" => (173, 216, 230),
        "lightgreen" => (144, 238, 144),
        "steelblue" => (70, 130, 180),
        "royalblue" => (65, 105, 225),
        "dodgerblue" => (30, 144, 255),
        "cornflowerblue" => (100, 149, 237),
        "skyblue" => (135, 206, 235),
        "slategray" | "slategrey" => (112, 128, 144),
        "whitesmoke" => (245, 245, 245),
        "beige" => (245, 245, 220),
        "ivory" => (255, 255, 240),
        "coral" => (255, 127, 80),
        "crimson" => (220, 20, 60),
        "indigo" => (75, 0, 130),
        "violet" => (238, 130, 238),
        "turquoise" => (64, 224, 208),
        _ => return None,
    };
    Some(Rgba::rgb(rgb.0, rgb.1, rgb.2))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn sniffs_svg_with_xml_prolog_and_rejects_raster() {
        assert!(is_svg(b"<svg viewBox='0 0 10 10'/>"));
        assert!(is_svg(b"\xef\xbb\xbf<?xml version=\"1.0\"?><!-- comment --><svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\"/>"));
        assert!(is_svg(
            b"<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\" [<!ENTITY a \"1\"><!-- ] -->]><svg viewBox=\"0 0 10 10\"/>"
        ));
        assert!(!is_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_svg(b"<html><body>not svg</body></html>"));
    }

    #[test]
    fn rasterizes_paths_shapes_styles_and_transforms() {
        let svg = br##"<svg viewBox="0 0 100 100" xmlns="http://www.w3.org/2000/svg">
            <defs>
                <style>.accent { fill: #ff4400; }</style>
                <linearGradient id="g1" x1="0" y1="0" x2="1" y2="0">
                    <stop offset="0%" stop-color="#0000ff"/>
                    <stop offset="100%" stop-color="#00ff00"/>
                </linearGradient>
            </defs>
            <rect x="0" y="0" width="50" height="50" class="accent"/>
            <g transform="translate(50, 50)">
                <circle cx="25" cy="25" r="20" fill="url(#g1)"/>
            </g>
            <path d="M10 60 L40 60 L40 90 L10 90 Z" fill="#102030"/>
        </svg>"##;
        assert_eq!(natural_size(svg), Some((100.0, 100.0)));
        let img = rasterize(svg, 100).expect("rasterized svg");
        assert_eq!(img.dimensions(), (100, 100));
        // Top-left CSS class rect (#ff4400)
        assert_eq!(img.get_pixel(25, 25).0, [0xff, 0x44, 0x00, 0xff]);
        // Bottom-left path (#102030)
        assert_eq!(img.get_pixel(25, 75).0, [0x10, 0x20, 0x30, 0xff]);
        // Bottom-right translated gradient circle (non-transparent, blended blue->green)
        let center = img.get_pixel(75, 75).0;
        assert_eq!(center[3], 255);
        assert!(center[1] > 80 && center[2] > 80, "expected blue-green gradient midpoint, got {center:?}");
        // Empty corner stays transparent
        assert_eq!(img.get_pixel(95, 5).0[3], 0);
    }

    #[test]
    fn handles_use_clip_path_and_invalid_svg_gracefully() {
        let svg = br##"<svg viewBox="0 0 40 40" xmlns="http://www.w3.org/2000/svg">
            <defs>
                <rect id="box" width="40" height="40" fill="#22aa44"/>
                <clipPath id="half"><rect x="0" y="0" width="20" height="40"/></clipPath>
            </defs>
            <g clip-path="url(#half)"><use href="#box"/></g>
        </svg>"##;
        let img = rasterize(svg, 40).expect("clipped use");
        assert_eq!(img.get_pixel(10, 20).0, [0x22, 0xaa, 0x44, 0xff]);
        assert_eq!(img.get_pixel(30, 20).0[3], 0);
        assert!(rasterize(b"<svg></svg>", 64).is_none());
    }

    #[test]
    fn hostile_use_fanout_over_empty_groups_is_bounded() {
        use std::fmt::Write as _;
        use std::time::Instant;

        // 15-level x 10 <use> fan-out over empty <g> (10^15 virtual nodes without visit/use caps).
        let mut svg = String::from("<svg viewBox=\"0 0 10 10\" xmlns=\"http://www.w3.org/2000/svg\"><defs><g id=\"l0\"/>");
        for level in 1..=15 {
            let _ = write!(svg, "<g id=\"l{level}\">");
            for _ in 0..10 {
                let _ = write!(svg, "<use href=\"#l{}\"/>", level - 1);
            }
            svg.push_str("</g>");
        }
        svg.push_str("</defs><use href=\"#l15\"/></svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 32).expect("bounded fanout");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (32, 32));
        assert!(elapsed.as_millis() < 200, "15x10 <use> fan-out took {elapsed:?}");
    }

    #[test]
    fn hostile_use_self_cycle_terminates_immediately() {
        use std::time::Instant;

        let mut svg = String::from("<svg viewBox=\"0 0 10 10\" xmlns=\"http://www.w3.org/2000/svg\"><g id=\"c\">");
        for _ in 0..50 {
            svg.push_str("<use href=\"#c\"/>");
        }
        svg.push_str("</g></svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 32).expect("bounded cycle");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (32, 32));
        assert!(elapsed.as_millis() < 200, "self-cycle <use> took {elapsed:?}");
    }

    #[test]
    fn hostile_tiny_stroke_dasharray_falls_back_to_solid_without_oom() {
        use std::time::Instant;

        let svg = br##"<svg viewBox="0 0 100 100" xmlns="http://www.w3.org/2000/svg">
            <path d="M0 0 L100000 0" stroke="black" stroke-width="4" stroke-dasharray="0.00001"/>
        </svg>"##;

        let t0 = Instant::now();
        let img = rasterize(svg, 100).expect("tiny dasharray rasterized");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (100, 100));
        assert!(elapsed.as_millis() < 200, "tiny stroke-dasharray took {elapsed:?}");
        // Stroke still renders solid black along the top edge.
        assert!(img.get_pixel(50, 0).0[3] > 0);
    }

    #[test]
    fn hostile_repeated_large_path_use_is_bounded_by_segment_budget() {
        use std::fmt::Write as _;
        use std::time::Instant;

        // 20k-segment path referenced via 6x10 <use> fan-out (1M virtual expansions).
        let mut d = String::from("M0 0");
        for i in 1..=20_000 {
            let _ = write!(d, " L{} {}", i % 100, (i / 100) % 100);
        }
        let mut svg = format!(
            "<svg viewBox=\"0 0 100 100\" xmlns=\"http://www.w3.org/2000/svg\"><defs><path id=\"p0\" d=\"{d}\" fill=\"none\" stroke=\"black\"/>"
        );
        for level in 1..=6 {
            let _ = write!(svg, "<g id=\"p{level}\">");
            for _ in 0..10 {
                let _ = write!(svg, "<use href=\"#p{}\"/>", level - 1);
            }
            svg.push_str("</g>");
        }
        svg.push_str("</defs><use href=\"#p6\"/></svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 64).expect("bounded 20k path use");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (64, 64));
        assert!(elapsed.as_millis() < 500, "6x10 <use> of 20k path took {elapsed:?}");
    }

    #[test]
    fn hostile_massive_css_stylesheet_and_groups_stay_fast() {
        use std::fmt::Write as _;
        use std::time::Instant;

        // 200k CSS rules x 5k <g> elements.
        let mut svg = String::from("<svg viewBox=\"0 0 100 100\" xmlns=\"http://www.w3.org/2000/svg\"><style>");
        for i in 0..200_000 {
            let _ = write!(svg, ".c{i}{{fill:red}}");
        }
        svg.push_str("</style>");
        for i in 0..5_000 {
            let _ = write!(svg, "<g class=\"c{i}\"/>");
        }
        svg.push_str("<rect width=\"100\" height=\"100\" class=\"c0\"/></svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 32).expect("bounded massive css");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (32, 32));
        assert_eq!(img.get_pixel(16, 16).0, [255, 0, 0, 255]);
        assert!(elapsed.as_millis() < 500, "200k CSS x 5k <g> took {elapsed:?}");
    }

    #[test]
    fn hostile_css_comma_selectors_share_decls_without_oom() {
        use std::fmt::Write as _;
        use std::time::Instant;

        // 4,096 comma-separated selectors sharing a 20,000-declaration block.
        let mut svg = String::from("<svg viewBox=\"0 0 100 100\" xmlns=\"http://www.w3.org/2000/svg\"><style>");
        for i in 0..4_096 {
            if i > 0 {
                svg.push(',');
            }
            let _ = write!(svg, "x{i}");
        }
        svg.push('{');
        for _ in 0..20_000 {
            svg.push_str("a:b;");
        }
        svg.push_str("}</style><rect width=\"100\" height=\"100\" fill=\"#112233\"/></svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 32).expect("bounded comma selectors");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (32, 32));
        assert_eq!(img.get_pixel(16, 16).0, [0x11, 0x22, 0x33, 0xff]);
        assert!(elapsed.as_millis() < 200, "comma-selector CSS took {elapsed:?}");
    }

    #[test]
    fn hostile_huge_inline_style_reused_by_500_uses_is_cached() {
        use std::fmt::Write as _;
        use std::time::Instant;

        // 1 MiB inline style on a single <rect id="a"/> referenced by 500 <use href="#a"/>.
        let mut style = String::from("fill:#cc2211;");
        while style.len() < (1 << 20) {
            style.push_str("x:1;");
        }
        let mut svg = format!(
            "<svg viewBox=\"0 0 100 100\" xmlns=\"http://www.w3.org/2000/svg\"><defs><rect id=\"a\" width=\"100\" height=\"100\" style=\"{style}\"/></defs>"
        );
        for _ in 0..500 {
            let _ = write!(svg, "<use href=\"#a\"/>");
        }
        svg.push_str("</svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 32).expect("bounded huge inline style");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (32, 32));
        assert_eq!(img.get_pixel(16, 16).0, [0xcc, 0x22, 0x11, 0xff]);
        assert!(elapsed.as_millis() < 200, "1 MiB inline style x 500 <use> took {elapsed:?}");
    }

    #[test]
    fn hostile_universal_css_many_decls_over_groups_hits_decl_budget() {
        use std::time::Instant;

        // *{...100k decls...} over 2,000 empty <g/> elements.
        let mut svg = String::from("<svg viewBox=\"0 0 100 100\" xmlns=\"http://www.w3.org/2000/svg\"><style>*{");
        for _ in 0..100_000 {
            svg.push_str("a:b;");
        }
        svg.push_str("}</style>");
        for _ in 0..2_000 {
            svg.push_str("<g/>");
        }
        svg.push_str("</svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 32).expect("bounded universal css");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (32, 32));
        assert!(elapsed.as_millis() < 200, "*{{100k decls}} x 2k <g> took {elapsed:?}");
    }

    #[test]
    fn hostile_64_entry_dasharray_across_200_long_lines_is_bounded() {
        use std::fmt::Write as _;
        use std::time::Instant;

        // 64-entry tiny dasharray across 200 2500-unit lines at 2500 px.
        for dash_val in ["0.0039", "0.004"] {
            let mut dash = String::new();
            for i in 0..64 {
                if i > 0 {
                    dash.push(',');
                }
                dash.push_str(dash_val);
            }
            let mut svg = format!(
                "<svg viewBox=\"0 0 2500 2500\" xmlns=\"http://www.w3.org/2000/svg\"><style>line{{stroke:#000;stroke-width:2;stroke-dasharray:{dash}}}</style>"
            );
            for i in 0..200 {
                let y = i * 10 + 5;
                let _ = write!(svg, "<line x1=\"0\" y1=\"{y}\" x2=\"2500\" y2=\"{y}\"/>");
            }
            svg.push_str("</svg>");

            let t0 = Instant::now();
            let img = rasterize(svg.as_bytes(), 2500).expect("bounded 64-entry dasharray");
            let elapsed = t0.elapsed();
            assert_eq!(img.dimensions(), (2500, 2500));
            assert!(img.get_pixel(1250, 5).0[3] > 0, "expected solid fallback stroke for {dash_val}");
            assert!(elapsed.as_millis() < 300, "64-entry dasharray ({dash_val}) x 200 lines took {elapsed:?}");
        }
    }

    #[test]
    fn hostile_20k_gradient_stops_across_2000_rects_is_cached_and_bounded() {
        use std::fmt::Write as _;
        use std::time::Instant;

        // <linearGradient id="g"> with 20,000 <stop> elements applied to 2,000 rects at 256 px.
        let mut svg = String::from("<svg viewBox=\"0 0 256 256\" xmlns=\"http://www.w3.org/2000/svg\"><defs><linearGradient id=\"g\">");
        for i in 0..20_000 {
            let _ = write!(svg, "<stop offset=\"{:.5}\" stop-color=\"#ff0000\"/>", (i as f64) / 20_000.0);
        }
        svg.push_str("</linearGradient><style>rect{fill:url(#g)}</style></defs>");
        for i in 0..2_000 {
            let x = (i % 64) * 4;
            let y = ((i / 64) % 64) * 4;
            let _ = write!(svg, "<rect x=\"{x}\" y=\"{y}\" width=\"4\" height=\"4\"/>");
        }
        svg.push_str("</svg>");

        let t0 = Instant::now();
        let img = rasterize(svg.as_bytes(), 256).expect("bounded 20k gradient stops");
        let elapsed = t0.elapsed();
        assert_eq!(img.dimensions(), (256, 256));
        // First rect uses cached gradient; final rect uses solid first-stop fallback after budget exhaustion.
        assert_eq!(img.get_pixel(2, 2).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel((1999 % 64) * 4 + 2, (1999 / 64) * 4 + 2).0, [255, 0, 0, 255]);
        assert!(elapsed.as_millis() < 300, "20k stops x 2000 rects took {elapsed:?}");
    }

    #[test]
    fn css_specificity_and_dasharray_zero_or_negative_semantics() {
        // LOW-1: CSS specificity (#id > tag.class > .class > tag > *).
        let spec_svg = br##"<svg viewBox="0 0 60 20" xmlns="http://www.w3.org/2000/svg">
            <style>
                #r { fill: red; }
                rect { fill: blue; }
                rect.c { fill: lime; }
                .c { fill: yellow; }
                * { fill: yellow; }
            </style>
            <rect id="r" x="0" y="0" width="20" height="20"/>
            <rect class="c" x="20" y="0" width="20" height="20"/>
            <rect x="40" y="0" width="20" height="20"/>
        </svg>"##;
        let img = rasterize(spec_svg, 60).expect("specificity svg");
        assert_eq!(img.get_pixel(10, 10).0, [255, 0, 0, 255], "#id beats later tag rule");
        assert_eq!(img.get_pixel(30, 10).0, [0, 255, 0, 255], "tag.class beats later .class rule");
        assert_eq!(img.get_pixel(50, 10).0, [0, 0, 255, 255], "tag rule applies over *");

        // LOW-2: all-zero dasharray resets inherited dash to solid; negative entry is ignored.
        let dash_svg = br##"<svg viewBox="0 0 40 30" xmlns="http://www.w3.org/2000/svg">
            <g stroke="#000" stroke-width="4" stroke-dasharray="4 4">
                <line x1="0" y1="5" x2="40" y2="5" stroke-dasharray="0"/>
                <line x1="0" y1="15" x2="40" y2="15" stroke-dasharray="0, 0"/>
                <line x1="0" y1="25" x2="40" y2="25" stroke-dasharray="4, -2"/>
            </g>
        </svg>"##;
        let dimg = rasterize(dash_svg, 40).expect("dasharray semantics svg");
        // At x=6 (middle of the 4..8 inherited gap):
        assert_eq!(dimg.get_pixel(6, 5).0[3], 255, "stroke-dasharray=0 resets to solid");
        assert_eq!(dimg.get_pixel(6, 15).0[3], 255, "stroke-dasharray=0,0 resets to solid");
        assert_eq!(dimg.get_pixel(6, 25).0[3], 0, "negative dasharray entry ignored, preserving inherited dash");
        assert_eq!(dimg.get_pixel(2, 25).0[3], 255, "inherited dash still paints on-segment");
    }

    #[test]
    fn many_inkscape_styled_elements_keep_colors_and_display_none() {
        use std::fmt::Write as _;
        // MEDIUM-1 regression: 15,000 elements each carrying 6 presentation attributes plus a
        // 10-declaration inline style (~240k attrs+decls > MAX_DECL_VISITS) must not exhaust the
        // stylesheet declaration budget, so later elements keep their fill color and `display:none`.
        let mut svg = String::from("<svg viewBox=\"0 0 80 40\" xmlns=\"http://www.w3.org/2000/svg\">");
        for i in 0..15_000 {
            let _ = write!(
                svg,
                "<rect id=\"r{i}\" x=\"0\" y=\"0\" width=\"2\" height=\"2\" style=\"fill:#0000ff;stroke:none;stroke-width:1;stroke-linecap:butt;stroke-linejoin:miter;stroke-opacity:1;fill-opacity:1;fill-rule:nonzero;opacity:1;visibility:visible\"/>"
            );
        }
        svg.push_str("<rect id=\"tail\" x=\"10\" y=\"10\" width=\"20\" height=\"20\" style=\"fill:#0000ff;stroke:none;opacity:1\"/>");
        svg.push_str("<rect id=\"hidden\" x=\"40\" y=\"10\" width=\"20\" height=\"20\" style=\"fill:#ff0000;display:none\"/>");
        svg.push_str("</svg>");

        let img = rasterize(svg.as_bytes(), 80).expect("15k Inkscape-style SVG");
        assert_eq!(img.get_pixel(20, 20).0, [0, 0, 255, 255], "tail rect must keep blue fill");
        assert_eq!(img.get_pixel(50, 20).0, [0, 0, 0, 0], "display:none rect must stay hidden");
    }
}
