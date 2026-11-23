//! Camera settings changed while running — from the local web UI or from
//! Barbie's Lab (`uplink/`) — applied live and saved to `record.conf`, so both
//! paths behave the same and the change survives a restart.

use std::fmt;

use crate::cam::{self, CamControl, Orientation};
use crate::conf;

/// Any subset of the orientation; unset fields keep their current value.
#[derive(Default, Debug)]
pub struct OrientationChange {
    pub rotation: Option<u16>,
    pub hflip: Option<bool>,
    pub vflip: Option<bool>,
}

#[derive(Debug)]
pub enum SettingsError {
    BadRotation,
    BadLensPosition,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRotation => write!(f, "rotation must be 0, 90, 180 or 270"),
            Self::BadLensPosition => {
                write!(f, "lensPosition must be \"auto\", 0–32 (dioptres: 0 = infinity) or \"default\"")
            }
        }
    }
}

/// Apply an orientation change (the capture restarts, ~1s blip) and save it.
pub fn set_orientation(cam: &CamControl, change: OrientationChange) -> Result<Orientation, SettingsError> {
    let mut o = cam.get();
    if let Some(r) = change.rotation {
        if !matches!(r, 0 | 90 | 180 | 270) {
            return Err(SettingsError::BadRotation);
        }
        o.rotation = r;
    }
    o.hflip = change.hflip.unwrap_or(o.hflip);
    o.vflip = change.vflip.unwrap_or(o.vflip);
    cam.set(o);
    log::info!("orientation set: {o:?}");
    let flag = |b: bool| if b { "1" } else { "0" };
    let rotation = o.rotation.to_string();
    save(&[("HQCAM_ROTATION", &rotation), ("HQCAM_HFLIP", flag(o.hflip)), ("HQCAM_VFLIP", flag(o.vflip))]);
    Ok(o)
}

/// The focus asked for in JSON: `null` or `"auto"` = autofocus (`Ok(None)`), a
/// number (dioptres) or `"default"` = fixed there.
pub fn lens_from_json(v: &serde_json::Value) -> Result<Option<String>, SettingsError> {
    use serde_json::Value;
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().eq_ignore_ascii_case("auto") => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(n) => Ok(Some(n.to_string())),
        _ => Err(SettingsError::BadLensPosition),
    }
}

/// Fix focus at `lens` (dioptres or `default`), or `None` for autofocus; the
/// capture restarts and the choice is saved.
pub fn set_focus(cam: &CamControl, lens: Option<&str>) -> Result<Option<String>, SettingsError> {
    let lens = match lens {
        None => None,
        Some(raw) => Some(cam::parse_lens_position(raw).ok_or(SettingsError::BadLensPosition)?),
    };
    cam.set_focus(lens.clone());
    log::info!("focus set: {}", lens.as_deref().unwrap_or("auto"));
    save(&[("HQCAM_LENS_POSITION", lens.as_deref().unwrap_or(""))]);
    Ok(lens)
}

/// Save settings to record.conf. A failure is logged, not returned: the change
/// is already live, it just won't survive a restart.
fn save(pairs: &[(&str, &str)]) {
    let path = conf::path();
    if let Err(e) = conf::set(&path, pairs) {
        log::error!("couldn't save settings to {}: {e} (applied until restart)", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn focus_json_forms() {
        assert_eq!(lens_from_json(&json!(null)).unwrap(), None);
        assert_eq!(lens_from_json(&json!("Auto")).unwrap(), None);
        assert_eq!(lens_from_json(&json!(0.5)).unwrap().as_deref(), Some("0.5"));
        assert_eq!(lens_from_json(&json!("default")).unwrap().as_deref(), Some("default"));
        assert!(lens_from_json(&json!([1])).is_err());
    }
}
