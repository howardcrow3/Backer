//! Status dot on the tray icon: blue when the last backups succeeded, amber for warnings, vermilion when one failed.
//! Colours are the design language's `success`, `accent` and `danger` (blue, not green, so success and failure stay
//! apart for colour-blind users).
//! Pure functions (no Tauri types) so they can be tested on their own.
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// No finished backup yet: no dot.
    None,
    Good,
    Warnings,
    Failed,
}

impl Health {
    pub fn tooltip(self) -> &'static str {
        match self {
            Health::None => "Backer",
            Health::Good => "Backer: last backup succeeded",
            Health::Warnings => "Backer: last backup finished with warnings",
            Health::Failed => "Backer: last backup failed",
        }
    }
    fn rgb(self) -> Option<[u8; 3]> {
        match self {
            Health::None => None,
            Health::Good => Some([0x14, 0x68, 0x9a]),
            Health::Warnings => Some([0xf4, 0xa6, 0x2a]),
            Health::Failed => Some([0xb3, 0x35, 0x0b]),
        }
    }
}

/// Worst result across jobs, using each job's most recent run. A run still in progress or cancelled says
/// nothing about success, so it is skipped.
pub fn health_of(jobs: &Value) -> Health {
    let mut h = Health::None;
    for j in jobs.as_array().into_iter().flatten() {
        let this = match j["last_run"]["status"].as_str() {
            Some("ok") => Health::Good,
            Some("warnings") => Health::Warnings,
            Some("failed") => Health::Failed,
            _ => continue,
        };
        let rank = |x: Health| match x { Health::None => 0, Health::Good => 1, Health::Warnings => 2, Health::Failed => 3 };
        if rank(this) > rank(h) {
            h = this;
        }
    }
    h
}

/// Returns a copy of an RGBA image with a filled dot (white ring for contrast) in the bottom-right corner.
pub fn with_dot(rgba: &[u8], w: u32, h: u32, health: Health) -> Vec<u8> {
    let mut out = rgba.to_vec();
    let Some(c) = health.rgb() else { return out };
    let (w_, h_) = (w as f32, h as f32);
    let r = w_.min(h_) * 0.24;
    let (cx, cy) = (w_ - r - w_ * 0.02, h_ - r - h_ * 0.02);
    let ring = (r * 0.18).max(1.0);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            // Soft 1px edge so it stays smooth when the OS scales the icon.
            let cover = (r + 0.5 - d).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            let inner = (r - ring + 0.5 - d).clamp(0.0, 1.0);
            let i = ((y * w + x) * 4) as usize;
            let col = [
                (c[0] as f32 * inner + 255.0 * (1.0 - inner)) as u8,
                (c[1] as f32 * inner + 255.0 * (1.0 - inner)) as u8,
                (c[2] as f32 * inner + 255.0 * (1.0 - inner)) as u8,
            ];
            let a0 = out[i + 3] as f32 / 255.0;
            let a = cover + a0 * (1.0 - cover);
            for k in 0..3 {
                out[i + k] = ((col[k] as f32 * cover + out[i + k] as f32 * a0 * (1.0 - cover)) / a) as u8;
            }
            out[i + 3] = (a * 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(status: &str) -> Value {
        json!({"last_run": {"status": status}})
    }

    #[test]
    fn worst_finished_result_wins() {
        assert_eq!(health_of(&json!([])), Health::None);
        assert_eq!(health_of(&json!([{"last_run": null}])), Health::None);
        assert_eq!(health_of(&json!([job("ok"), job("ok")])), Health::Good);
        assert_eq!(health_of(&json!([job("ok"), job("warnings")])), Health::Warnings);
        assert_eq!(health_of(&json!([job("warnings"), job("failed"), job("ok")])), Health::Failed);
        // A run in progress or cancelled does not change the verdict from other jobs.
        assert_eq!(health_of(&json!([job("running"), job("ok"), job("cancelled")])), Health::Good);
    }

    #[test]
    fn dot_is_drawn_bottom_right_only() {
        let (w, h) = (32u32, 32u32);
        let base = vec![0u8; (w * h * 4) as usize]; // fully transparent
        let g = with_dot(&base, w, h, Health::Good);
        let px = |img: &Vec<u8>, x: u32, y: u32| { let i = ((y * w + x) * 4) as usize; [img[i], img[i + 1], img[i + 2], img[i + 3]] };
        assert_eq!(px(&g, 2, 2), [0, 0, 0, 0], "top-left untouched");
        let centre = px(&g, 32 - 9, 32 - 9);
        assert_eq!(centre[3], 255);
        assert!(centre[2] > centre[0] && centre[2] > centre[1], "blue: {centre:?}");
        let r = with_dot(&base, w, h, Health::Failed);
        let c = px(&r, 32 - 9, 32 - 9);
        assert!(c[0] > c[1] && c[0] > c[2], "red: {c:?}");
        assert_eq!(with_dot(&base, w, h, Health::None), base);
    }
}
