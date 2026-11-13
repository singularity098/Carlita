//! The icons' artwork. Icons are SVG path data in `ui/icons.slint`; this
//! wraps them in SVG documents that Slint rasterises on the CPU (resvg) at
//! the size they are shown. Skia's GPU backends draw `Path` elements without
//! proper edge smoothing, which left the icons jagged; images come out
//! smooth on every renderer.

use slint::{ComponentHandle, Image, SharedString};
use std::cell::RefCell;
use std::collections::HashMap;

use crate::{IconArt, MainWindow};

type Key = (SharedString, SharedString, u32, u32);

thread_local! {
    // Bindings call `draw` often; parse each icon only once.
    static CACHE: RefCell<HashMap<Key, Image>> = RefCell::new(HashMap::new());
}

pub fn install(ui: &MainWindow) {
    ui.global::<IconArt>().on_draw(draw);
}

fn draw(line: SharedString, solid: SharedString, weight: f32, grid: f32) -> Image {
    let key = (
        line.clone(),
        solid.clone(),
        weight.to_bits(),
        grid.to_bits(),
    );
    if let Some(image) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return image;
    }
    let image = Image::load_from_svg_data(svg(&line, &solid, weight, grid).as_bytes())
        .unwrap_or_else(|e| {
            log::warn!("icon could not be drawn: {e:?}");
            Image::default()
        });
    CACHE.with(|c| c.borrow_mut().insert(key, image.clone()));
    image
}

/// An SVG document `grid` units square: `solid` filled (even-odd) and `line`
/// stroked `weight` wide with round ends, both white so `colorize` can tint
/// them.
fn svg(line: &str, solid: &str, weight: f32, grid: f32) -> String {
    let mut out = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{grid}" height="{grid}" viewBox="0 0 {grid} {grid}">"#
    );
    if !solid.is_empty() {
        out += &format!(
            r#"<path d="{}" fill="white" fill-rule="evenodd"/>"#,
            attr(solid)
        );
    }
    if !line.is_empty() {
        out += &format!(
            r#"<path d="{}" fill="none" stroke="white" stroke-width="{weight}" stroke-linecap="round" stroke-linejoin="round"/>"#,
            attr(line)
        );
    }
    out + "</svg>"
}

/// Path data holds only commands and numbers; drop anything that could end
/// the attribute.
fn attr(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '"' | '<' | '>' | '&'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_has_both_paths() {
        let s = svg("M 1 1 L 2 2", "M 0 0 Z", 2.0, 24.0);
        assert!(s.starts_with(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24">"#
        ));
        assert!(s.contains(r#"<path d="M 0 0 Z" fill="white" fill-rule="evenodd"/>"#));
        assert!(s.contains(r#"<path d="M 1 1 L 2 2" fill="none" stroke="white" stroke-width="2""#));
        assert!(s.ends_with("</svg>"));
    }

    #[test]
    fn empty_paths_are_left_out() {
        assert_eq!(svg("", "", 2.0, 14.0).matches("<path").count(), 0);
        assert_eq!(svg("M 1 1", "", 1.4, 14.0).matches("<path").count(), 1);
    }

    #[test]
    fn svg_parses() {
        let s = svg(
            "M 17.66 6.6 A 8 8 0 1 1 6.34 6.6 M 12 2.8 L 12 11.5",
            "",
            2.2,
            24.0,
        );
        let image = Image::load_from_svg_data(s.as_bytes()).unwrap();
        assert_eq!((image.size().width, image.size().height), (24, 24));
    }
}
