//! Colours. One dark theme for the PoC; a light theme follows the same names.

pub const BG: u32 = 0x0D0F13;
pub const BAR: u32 = 0x0D0F13;
pub const PANEL: u32 = 0x12151B;
pub const CARD: u32 = 0x161A21;
pub const CARD_HOVER: u32 = 0x1B2029;
pub const FIELD: u32 = 0x171B22;
pub const BORDER: u32 = 0x252B35;
pub const BORDER_STRONG: u32 = 0x343C49;
pub const TEXT: u32 = 0xE9EDF3;
pub const MUTED: u32 = 0x9099A8;
pub const FAINT: u32 = 0x5E6776;
pub const ACCENT: u32 = 0x7B93FF;
pub const ACCENT_TEXT: u32 = 0x0B0E1A;
pub const RUNNING: u32 = 0x3DD68C;
pub const BUSY: u32 = 0xF4B740;
pub const FAILED: u32 = 0xFF6B6B;
pub const OFF: u32 = 0x6B7482;
pub const SCREEN: u32 = 0x07080A;

/// A calm hue per OS family or preset, for posters of machines that have
/// never shown a screen.
pub fn os_tint(key: &str) -> u32 {
    const HUES: [u32; 8] = [0x5B7CFA, 0x2FB6A3, 0xE0794B, 0xB565D9, 0x4FA3E0, 0xD9A23D, 0x5FBF5A, 0xE05A7A];
    let k = key.to_ascii_lowercase();
    let pick = |s: &str| k.contains(s);
    if pick("windows") {
        return 0x3A8DDE;
    }
    if pick("fedora") {
        return 0x4F7FD9;
    }
    if pick("ubuntu") {
        return 0xE0683E;
    }
    if pick("debian") {
        return 0xD23F63;
    }
    if pick("arch") {
        return 0x3EA6D6;
    }
    let h = k.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    HUES[(h % HUES.len() as u32) as usize]
}
