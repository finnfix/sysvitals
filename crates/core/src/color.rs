//! Perceptual color helpers shared by the GPUI panels.
//!
//! The panels let the primary button color follow the OS accent. Choosing a
//! readable label color needs the WCAG relative luminance, not the HSL
//! lightness: a saturated yellow (`#FFB900`) has an HSL lightness near 0.5 yet
//! is far brighter than a mid-blue, so an HSL threshold paints white text on
//! it. This module keeps that decision pure and unit-testable.

/// Convert a Windows `ABGR` color DWORD (the byte order used by the
/// Explorer accent registry values) into a packed `0xRRGGBB` color.
///
/// The alpha byte is dropped: the accent is meaningful independently of the
/// translucency flag Windows stores alongside it.
pub fn abgr_to_rgb(abgr: u32) -> u32 {
    ((abgr & 0xFF) << 16) | (abgr & 0xFF00) | ((abgr >> 16) & 0xFF)
}

/// WCAG relative luminance of a packed `0xRRGGBB` color.
pub fn relative_luminance(hex: u32) -> f64 {
    fn linearize(channel: u8) -> f64 {
        let c = f64::from(channel) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    let r = linearize(((hex >> 16) & 0xFF) as u8);
    let g = linearize(((hex >> 8) & 0xFF) as u8);
    let b = linearize((hex & 0xFF) as u8);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Whether a dark label is more readable on `hex` than a light one.
///
/// Uses WCAG contrast ratios with a slight bias toward the light label: the
/// dark label must be at least 15% better, not merely equal. That keeps the
/// OS-like white-on-blue look for mid-tone accents (the default Windows blue
/// is almost exactly at the mathematical crossover) while still switching to
/// dark text on genuinely bright accents such as gold/yellow.
pub fn prefers_dark_foreground(hex: u32) -> bool {
    let l = relative_luminance(hex);
    let contrast_black = (l + 0.05) / 0.05;
    let contrast_white = 1.05 / (l + 0.05);
    contrast_black > contrast_white * 1.15
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bright_gold_gets_a_dark_label() {
        // HSL lightness of #FFB900 is 0.5, which is why an HSL threshold used
        // to choose white here; its WCAG luminance is ~0.56.
        assert!(prefers_dark_foreground(0xFFB900));
    }

    #[test]
    fn default_blue_gets_a_light_label() {
        assert!(!prefers_dark_foreground(0x0078D4));
    }

    #[test]
    fn black_and_white_land_on_opposite_sides() {
        assert!(!prefers_dark_foreground(0x000000));
        assert!(prefers_dark_foreground(0xFFFFFF));
    }

    #[test]
    fn explorer_accent_abgr_is_unpacked_to_rgb() {
        // 0xAABBGGRR: Windows stores the accent as ABGR, so a gold accent
        // (#867215) is written as 0xFF157286.
        assert_eq!(abgr_to_rgb(0xFF15_7286), 0x0086_7215);
        // Default Windows blue #0078D4 is stored as ABGR 0xFFD47800.
        assert_eq!(abgr_to_rgb(0xFFD4_7800), 0x0000_78D4);
    }

    #[test]
    fn luminance_of_primaries_matches_reference_values() {
        assert!((relative_luminance(0x000000) - 0.0).abs() < 1e-9);
        assert!((relative_luminance(0xFFFFFF) - 1.0).abs() < 1e-9);
        assert!((relative_luminance(0xFF0000) - 0.2126).abs() < 1e-4);
        assert!((relative_luminance(0x00FF00) - 0.7152).abs() < 1e-4);
        assert!((relative_luminance(0x0000FF) - 0.0722).abs() < 1e-4);
    }
}
