//! Delta §9.10: provider image budget + undecodable-image degrade.
//!
//! # The two failures this prevents
//!
//! A request that carries more image frames than the provider's
//! per-request cap is rejected outright — the model never sees the
//! transcript the frames were meant to preserve. A request that
//! carries a frame whose base64 body is not a decodable PNG is
//! likewise rejected; a single corrupt frame otherwise rejects the
//! entire request and wedges the session.
//!
//! Both defenses are conservative: they drop any frame that fails a
//! cheap structural check, and if more than `max_frames` remain,
//! drop from the *front* of the vec (the oldest, since the engine
//! appends in transcript order) down to the cap.
//!
//! # What this does NOT do
//!
//! kod carries images in exactly one place today: the `image_frames`
//! field on [`crate::request::CompletionRequest`], populated by the
//! engine's §4.5 snapcompact pass. The design's other two hiding
//! places — native `providerPayload` items replayed verbatim, and
//! `providerMetadata.screenshot` — have no analogue here: kod's tool
//! results are text, and the native-compaction block is an opaque
//! string, not an image-bearing payload. If a future revision adds an
//! inline image content variant to `ChatMessage`, it must call
//! [`apply_image_budget`] on that vec too.
//!
//! The design's decode-verdict LRU (512 entries, keyed
//! `mime:len:hash`) is not here: at kod's scale a frame is validated
//! once per request, not once per token, and the check is a byte
//! comparison against an 11-character prefix. An LRU would add a
//! dependency and a lock for no measurable win.

use crate::request::ImageFrame;

/// The maximum number of image frames a single request carries.
pub const MAX_FRAMES_PER_REQUEST: usize = 8;

/// The maximum size, in base64 bytes, of a single frame.
pub const MAX_FRAME_BYTES: usize = 5 * 1024 * 1024;

/// The 11-character base64 prefix of every well-formed PNG. It
/// encodes the 8-byte PNG signature (`\x89PNG\r\n\x1a\n`) exactly:
/// 11 base64 chars carry 8 bytes plus 2 leftover bits, and those
/// 2 bits are the top of the first IHDR length byte — always zero
/// for a chunk under 2^24 bytes, i.e. every PNG a rasterizer
/// produces. The 12th base64 character depends on that length field
/// and is therefore not part of the constant prefix.
const PNG_BASE64_PREFIX: &str = "iVBORw0KGgo";

/// A budget policy. `Default` is the module's constants.
#[derive(Debug, Clone, Copy)]
pub struct ImageBudgetPolicy {
    pub max_frames: usize,
    pub max_bytes_per_frame: usize,
}

impl Default for ImageBudgetPolicy {
    fn default() -> Self {
        Self {
            max_frames: MAX_FRAMES_PER_REQUEST,
            max_bytes_per_frame: MAX_FRAME_BYTES,
        }
    }
}

/// What [`apply_image_budget`] did to a frame vec.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetReport {
    pub dropped_undecodable: usize,
    pub dropped_oversize: usize,
    pub dropped_over_cap: usize,
}

impl BudgetReport {
    pub fn any_dropped(&self) -> bool {
        self.total_dropped() > 0
    }
    pub fn total_dropped(&self) -> usize {
        self.dropped_undecodable + self.dropped_oversize + self.dropped_over_cap
    }
}

/// Cheap PNG base64 check: prefix + length ≡ 0 (mod 4).
pub fn is_likely_png_base64(body: &str) -> bool {
    body.starts_with(PNG_BASE64_PREFIX) && body.len().is_multiple_of(4)
}

/// Apply `policy` to `frames` in place. Returns a report.
pub fn apply_image_budget(
    frames: &mut Vec<ImageFrame>,
    policy: &ImageBudgetPolicy,
) -> BudgetReport {
    let mut report = BudgetReport::default();

    frames.retain(|f| {
        let ok = is_likely_png_base64(&f.png_base64);
        if !ok {
            report.dropped_undecodable += 1;
        }
        ok
    });

    frames.retain(|f| {
        let ok = f.png_base64.len() <= policy.max_bytes_per_frame;
        if !ok {
            report.dropped_oversize += 1;
        }
        ok
    });

    if frames.len() > policy.max_frames {
        let excess = frames.len() - policy.max_frames;
        frames.drain(..excess);
        report.dropped_over_cap = excess;
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> ImageFrame {
        ImageFrame {
            png_base64: body.to_string(),
            media_type: "image/png".to_string(),
        }
    }

    /// 11-char prefix + 1 char = 12 chars, a multiple of 4.
    fn valid_png() -> String {
        format!("{PNG_BASE64_PREFIX}A")
    }

    /// Length > MAX_FRAME_BYTES and ≡ 0 (mod 4).
    fn oversize_png() -> String {
        format!("{PNG_BASE64_PREFIX}{}", "A".repeat(MAX_FRAME_BYTES + 1))
    }

    #[test]
    fn valid_png_passes() {
        assert!(is_likely_png_base64(&valid_png()));
        assert_eq!(valid_png().len(), 12);
    }

    #[test]
    fn non_png_base64_is_rejected() {
        // A JPEG's base64 begins `/9j/4AAQ`, not `iVBOR…`.
        assert!(!is_likely_png_base64("/9j/4AAQSkZJRg=="));
        // Starts with the PNG prefix but the length is not a multiple
        // of 4 — a truncated body.
        assert!(!is_likely_png_base64("iVBORw0KGgoAAA"));
        assert_eq!("iVBORw0KGgoAAA".len(), 14);
    }

    #[test]
    fn empty_frames_is_a_noop() {
        let mut frames: Vec<ImageFrame> = Vec::new();
        let report = apply_image_budget(&mut frames, &ImageBudgetPolicy::default());
        assert_eq!(report, BudgetReport::default());
        assert!(frames.is_empty());
    }

    #[test]
    fn oversize_frame_dropped() {
        let mut frames = vec![frame(&valid_png()), frame(&oversize_png())];
        let report = apply_image_budget(&mut frames, &ImageBudgetPolicy::default());
        assert_eq!(report.dropped_oversize, 1);
        assert_eq!(report.dropped_undecodable, 0);
        assert_eq!(report.dropped_over_cap, 0);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn undecodable_frame_dropped() {
        let mut frames = vec![frame("not a png"), frame(&valid_png())];
        let report = apply_image_budget(&mut frames, &ImageBudgetPolicy::default());
        assert_eq!(report.dropped_undecodable, 1);
        assert_eq!(report.dropped_oversize, 0);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn over_cap_drops_oldest_first() {
        let policy = ImageBudgetPolicy {
            max_frames: 2,
            max_bytes_per_frame: MAX_FRAME_BYTES,
        };
        let mut frames = Vec::new();
        for i in 0..5 {
            frames.push(frame(&format!("{PNG_BASE64_PREFIX}{i:05}")));
        }
        let report = apply_image_budget(&mut frames, &policy);
        assert_eq!(report.dropped_over_cap, 3);
        assert_eq!(report.dropped_undecodable, 0);
        assert_eq!(report.dropped_oversize, 0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].png_base64, format!("{PNG_BASE64_PREFIX}00003"));
        assert_eq!(frames[1].png_base64, format!("{PNG_BASE64_PREFIX}00004"));
    }

    #[test]
    fn report_aggregates_all_three_rules() {
        let policy = ImageBudgetPolicy {
            max_frames: 1,
            max_bytes_per_frame: 32,
        };
        let mut frames = vec![frame("junk"), frame(&valid_png()), frame(&oversize_png())];
        let report = apply_image_budget(&mut frames, &policy);
        assert!(report.any_dropped());
        assert_eq!(report.dropped_undecodable, 1);
        assert_eq!(report.dropped_oversize, 1);
        assert_eq!(report.dropped_over_cap, 0);
        assert_eq!(report.total_dropped(), 2);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].png_base64, valid_png());
    }
}
