//! Persist settings changed from the web UI back into `record.conf`.
//!
//! `record.conf` is the service's `EnvironmentFile`, so it stays the single
//! source of truth: a change made in the browser is written there and survives
//! a restart or reboot, exactly as if it had been edited by hand. Comments and
//! the order of lines are kept; only the named keys change.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the service reads its settings. `HQCAM_CONF` overrides it (tests, or
/// running by hand).
const DEFAULT_PATH: &str = "/usr/local/etc/hqcam/record.conf";

pub fn path() -> PathBuf {
    std::env::var("HQCAM_CONF").unwrap_or_else(|_| DEFAULT_PATH.into()).into()
}

/// Set each `KEY=value` in the config file, creating it if it doesn't exist.
/// Written to a temp file and renamed into place, so a power cut can't leave a
/// half-written config.
pub fn set(path: &Path, pairs: &[(&str, &str)]) -> io::Result<()> {
    let old = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let tmp = path.with_extension("conf.tmp");
    fs::write(&tmp, apply(&old, pairs))?;
    fs::rename(&tmp, path)
}

/// The text with each pair applied:
/// - every active `KEY=` line is replaced (systemd uses the last one, so all of
///   them must agree);
/// - otherwise the first commented `#KEY=` line is uncommented in place, so the
///   setting lands next to its documentation;
/// - otherwise `KEY=value` is appended.
fn apply(text: &str, pairs: &[(&str, &str)]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for (key, value) in pairs {
        let line = format!("{key}={value}");
        let prefix = format!("{key}=");
        let mut found = false;
        for l in lines.iter_mut().filter(|l| l.trim_start().starts_with(&prefix)) {
            *l = line.clone();
            found = true;
        }
        if found {
            continue;
        }
        let commented = lines.iter_mut().find(|l| {
            l.trim_start()
                .strip_prefix('#')
                .is_some_and(|rest| rest.trim_start().starts_with(&prefix))
        });
        match commented {
            Some(l) => *l = line,
            None => lines.push(line),
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_active_lines() {
        let t = "# rotation\nHQCAM_ROTATION=0\nHQCAM_WIDTH=1296\n";
        assert_eq!(apply(t, &[("HQCAM_ROTATION", "270")]), "# rotation\nHQCAM_ROTATION=270\nHQCAM_WIDTH=1296\n");
    }

    #[test]
    fn uncomments_documented_line_in_place() {
        let t = "# fixed focus\n#HQCAM_LENS_POSITION=0\nHQCAM_WIDTH=1296\n";
        assert_eq!(
            apply(t, &[("HQCAM_LENS_POSITION", "default")]),
            "# fixed focus\nHQCAM_LENS_POSITION=default\nHQCAM_WIDTH=1296\n"
        );
    }

    #[test]
    fn appends_unknown_keys_and_handles_empty_file() {
        assert_eq!(apply("", &[("HQCAM_HFLIP", "1"), ("HQCAM_VFLIP", "0")]), "HQCAM_HFLIP=1\nHQCAM_VFLIP=0\n");
    }

    #[test]
    fn prose_mentioning_a_key_is_left_alone() {
        let t = "# Set HQCAM_ROTATION=90 for portrait\n";
        assert_eq!(apply(t, &[("HQCAM_ROTATION", "180")]), "# Set HQCAM_ROTATION=90 for portrait\nHQCAM_ROTATION=180\n");
    }

    #[test]
    fn duplicate_active_lines_all_agree() {
        let t = "HQCAM_ROTATION=0\nHQCAM_ROTATION=90\n";
        assert_eq!(apply(t, &[("HQCAM_ROTATION", "180")]), "HQCAM_ROTATION=180\nHQCAM_ROTATION=180\n");
    }

    #[test]
    fn set_writes_and_creates_the_file() {
        let dir = std::env::temp_dir().join(format!("hqcam-conf-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("record.conf");
        let _ = fs::remove_file(&p);
        set(&p, &[("HQCAM_ROTATION", "90")]).unwrap();
        set(&p, &[("HQCAM_ROTATION", "180")]).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "HQCAM_ROTATION=180\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
