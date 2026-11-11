//! Plugin editor scaling: how a fixed-size editor is drawn into the main
//! area, and how touches are mapped back to the editor's own coordinates.

use serde::{Deserialize, Serialize};

/// Per-plugin editor scaling mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", content = "factor", rename_all = "lowercase")]
pub enum EditorScaling {
    /// Show the editor at its native size, centred.
    Off,
    /// Scale up (never down below 1:1 unless it does not fit) to fill the
    /// area, keeping the aspect ratio.
    #[default]
    Fit,
    /// A fixed zoom factor.
    Fixed(f32),
}

impl EditorScaling {
    pub fn label(&self) -> String {
        match self {
            EditorScaling::Off => "Off".into(),
            EditorScaling::Fit => "Fit to screen".into(),
            EditorScaling::Fixed(f) => format!("{:.0}%", f * 100.0),
        }
    }
}

/// Placement of an editor of `editor` size within an area of `area` size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaleTransform {
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub width: f32,
    pub height: f32,
}

impl ScaleTransform {
    /// Computes where and how big the editor is drawn.
    pub fn compute(editor: (f32, f32), area: (f32, f32), scaling: EditorScaling) -> Self {
        let (ew, eh) = (editor.0.max(1.0), editor.1.max(1.0));
        let fit = (area.0 / ew).min(area.1 / eh);
        let scale = match scaling {
            // Native size, but shrink if the editor would not fit at all.
            EditorScaling::Off => fit.min(1.0),
            EditorScaling::Fit => fit,
            EditorScaling::Fixed(f) => f.clamp(0.25, 8.0),
        };
        let (w, h) = (ew * scale, eh * scale);
        Self {
            scale,
            offset_x: ((area.0 - w) / 2.0).floor(),
            offset_y: ((area.1 - h) / 2.0).floor(),
            width: w,
            height: h,
        }
    }

    /// Maps a point in area coordinates to editor coordinates. Returns `None`
    /// when the point is outside the editor.
    pub fn to_editor(&self, x: f32, y: f32) -> Option<(i32, i32)> {
        let ex = (x - self.offset_x) / self.scale;
        let ey = (y - self.offset_y) / self.scale;
        let (ew, eh) = (self.width / self.scale, self.height / self.scale);
        if ex < 0.0 || ey < 0.0 || ex >= ew || ey >= eh {
            return None;
        }
        Some((ex as i32, ey as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_scales_up_and_centres() {
        let t = ScaleTransform::compute((400.0, 300.0), (1200.0, 800.0), EditorScaling::Fit);
        assert!((t.scale - 800.0 / 300.0).abs() < 1e-4);
        assert_eq!(t.offset_y, 0.0);
        assert!(t.offset_x > 0.0);
        // The editor's bottom-right pixel maps back inside the editor.
        let (x, y) = t
            .to_editor(t.offset_x + t.width - 1.0, t.height - 1.0)
            .unwrap();
        assert_eq!((x, y), (399, 299));
        assert_eq!(t.to_editor(0.0, 0.0), None);
    }

    #[test]
    fn off_keeps_native_size_unless_too_big() {
        let t = ScaleTransform::compute((400.0, 300.0), (1200.0, 800.0), EditorScaling::Off);
        assert_eq!(t.scale, 1.0);
        let t = ScaleTransform::compute((2400.0, 800.0), (1200.0, 800.0), EditorScaling::Off);
        assert_eq!(t.scale, 0.5);
    }

    #[test]
    fn fixed_factor_maps_touches_back() {
        let t =
            ScaleTransform::compute((200.0, 100.0), (1000.0, 1000.0), EditorScaling::Fixed(2.0));
        assert_eq!(t.scale, 2.0);
        assert_eq!(
            t.to_editor(t.offset_x + 100.0, t.offset_y + 50.0),
            Some((50, 25))
        );
    }

    #[test]
    fn scaling_serializes_readably() {
        assert_eq!(
            serde_json::to_string(&EditorScaling::Fixed(1.5)).unwrap(),
            r#"{"mode":"fixed","factor":1.5}"#
        );
        assert_eq!(
            serde_json::to_string(&EditorScaling::Fit).unwrap(),
            r#"{"mode":"fit"}"#
        );
    }
}
