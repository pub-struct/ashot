//! The JSON annotation spec — the contract agents program against.
//!
//! Accepted top-level forms: a bare array of annotations, or an object
//! `{"annotations": [...]}`. All coordinates are pixels in the target image.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Shared styling; every field optional, defaults applied at render time.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Style {
    /// `#RGB`, `#RRGGBB`, `#RRGGBBAA`, or a basic color name ("red", "blue", ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f32>,
    /// 0.0–1.0; when set, rect/ellipse interiors are filled with the color at
    /// this opacity in addition to the stroke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_opacity: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Annotation {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(flatten)]
        style: Style,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Ellipse {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(flatten)]
        style: Style,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Arrow {
        from: [f32; 2],
        to: [f32; 2],
        #[serde(flatten)]
        style: Style,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Numbered badge (①②③…). `number` is auto-assigned in spec order when omitted.
    Marker {
        x: f32,
        y: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u32>,
        /// Badge radius in px (default 16).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<f32>,
        #[serde(flatten)]
        style: Style,
    },
    Text {
        x: f32,
        y: f32,
        text: String,
        /// Font size in px (default 24).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<f32>,
        #[serde(flatten)]
        style: Style,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SpecDoc {
    annotations: Vec<Annotation>,
}

/// Parse a spec from JSON text (bare array or `{"annotations": [...]}`).
pub fn parse_spec(input: &str) -> Result<Vec<Annotation>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidSpec("empty spec".into()));
    }
    let mut annotations = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<Annotation>>(trimmed)
            .map_err(|e| Error::InvalidSpec(e.to_string()))?
    } else {
        serde_json::from_str::<SpecDoc>(trimmed)
            .map_err(|e| Error::InvalidSpec(e.to_string()))?
            .annotations
    };
    assign_marker_numbers(&mut annotations);
    Ok(annotations)
}

/// Give every number-less marker the next free number, in spec order.
fn assign_marker_numbers(annotations: &mut [Annotation]) {
    let mut next = annotations
        .iter()
        .filter_map(|a| match a {
            Annotation::Marker { number: Some(n), .. } => Some(*n),
            _ => None,
        })
        .max()
        .map_or(1, |m| m + 1);
    for a in annotations.iter_mut() {
        if let Annotation::Marker { number: number @ None, .. } = a {
            *number = Some(next);
            next += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_array() {
        let spec = parse_spec(r##"[{"type":"rect","x":1,"y":2,"w":3,"h":4,"color":"#ff0000"}]"##).unwrap();
        assert_eq!(spec.len(), 1);
        match &spec[0] {
            Annotation::Rect { x, style, .. } => {
                assert_eq!(*x, 1.0);
                assert_eq!(style.color.as_deref(), Some("#ff0000"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_wrapped_object() {
        let spec = parse_spec(r#"{"annotations":[{"type":"text","x":10,"y":20,"text":"hi"}]}"#).unwrap();
        assert_eq!(spec.len(), 1);
    }

    #[test]
    fn auto_numbers_markers() {
        let spec = parse_spec(
            r#"[{"type":"marker","x":0,"y":0},{"type":"marker","x":1,"y":1,"number":5},{"type":"marker","x":2,"y":2}]"#,
        )
        .unwrap();
        let numbers: Vec<u32> = spec
            .iter()
            .map(|a| match a {
                Annotation::Marker { number, .. } => number.unwrap(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(numbers, vec![6, 5, 7]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_spec("not json").is_err());
        assert!(parse_spec(r#"[{"type":"hexagon","x":0}]"#).is_err());
    }
}
