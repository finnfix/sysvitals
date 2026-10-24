//! Backdrop-material fallback rules for the GPUI panels.
//!
//! The panels paint like the host OS's own settings windows whenever the
//! platform offers a system backdrop material. The choice itself is a pure
//! threshold decision over the OS build number and the user's transparency
//! preference, so it lives here (unit-testable on any host); the probes that
//! read those values (OS version API, registry / defaults key) live in the
//! app crates.
//!
//! Windows capability map (from the pinned platform layer's behavior):
//! - build >= 22621 (Windows 11 22H2+): the documented
//!   `DWMWA_SYSTEMBACKDROP_TYPE` attribute applies a real Mica backdrop.
//! - build 19041..=22000 (Windows 10 2004+ / early Windows 11): Mica is not
//!   available; the legacy composition attribute applies an acrylic blur.
//!   Below 19041 the acrylic variant is unreliable, so stay opaque.
//! - Transparency effects disabled in the personalization settings: honor the
//!   user's choice and stay opaque.
//!
//! macOS: the window blur material has shipped since 10.10 and every
//! supported target has it; the accessibility "reduce transparency" switch
//! disables it, exactly like the Windows transparency switch removes the
//! backdrop.
//!
//! Anything unknown (probe failure, older system, unsupported host) is
//! `Opaque`, which is the pre-material rendering — every fallback keeps the
//! panel legible with the library palette instead of showing a half-painted
//! window.

/// The backdrop material a panel should request from the platform layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Material {
    /// No material: the window paints its own opaque background. This is
    /// also the safe fallback for any unsupported configuration.
    Opaque,
    /// Wallpaper-tint backdrop with no live blur. Windows-only, 22H2+.
    Mica,
    /// Live backdrop blur (Windows acrylic / macOS window blur material).
    Blur,
}

/// First Windows build where the documented system-backdrop attribute
/// applies a real Mica material.
pub const MICA_MIN_BUILD: u32 = 22621;
/// First Windows build where the legacy acrylic composition attribute is
/// reliable. Anything below falls back to `Material::Opaque`.
pub const ACRYLIC_MIN_BUILD: u32 = 19041;

/// Pick the Windows backdrop material from the OS build number and the
/// "transparency effects" preference.
///
/// `build` is `None` when the version probe fails; `transparency_enabled`
/// must only be `false` when the registry explicitly says the user turned
/// transparency effects off (a missing key means enabled).
pub fn choose_windows_material(build: Option<u32>, transparency_enabled: bool) -> Material {
    if !transparency_enabled {
        return Material::Opaque;
    }
    match build {
        Some(b) if b >= MICA_MIN_BUILD => Material::Mica,
        Some(b) if b >= ACRYLIC_MIN_BUILD => Material::Blur,
        _ => Material::Opaque,
    }
}

/// Pick the macOS backdrop material from the "reduce transparency"
/// accessibility preference. The blur material exists on every supported
/// host, so the only reason to stay opaque is the user's own choice.
pub fn choose_macos_material(transparency_enabled: bool) -> Material {
    if transparency_enabled {
        Material::Blur
    } else {
        Material::Opaque
    }
}

/// Alpha for the theme background tint painted over a live material, so the
/// desktop shows through without collapsing text contrast against a busy
/// background. Kept here so both panels share one value and the choice is
/// unit-testable. 0.55 (was 0.72) after on-device feedback that Mica read as
/// a flat fill: the extra 17 points of transparency is what makes the
/// wallpaper tint legible while text stays readable on both palettes.
pub const MATERIAL_TINT_ALPHA: f32 = 0.55;

/// Dark-mode tint alpha. The dark palette itself is already a dark layer; at
/// 0.55 it hides most of the Mica wallpaper tint (on-device: light mode looked
/// right, dark mode looked flat). 0.40 keeps body text readable while letting
/// the dark Mica tint through.
pub const MATERIAL_TINT_ALPHA_DARK: f32 = 0.40;

/// Pick the tint alpha for the current palette mode.
pub fn material_tint_alpha(dark: bool) -> f32 {
    if dark {
        MATERIAL_TINT_ALPHA_DARK
    } else {
        MATERIAL_TINT_ALPHA
    }
}

/// Whether the material needs the theme background tinted translucent.
/// `Opaque` never tints, so the fallback path keeps the pre-material
/// appearance untouched.
pub fn should_tint(material: Material) -> bool {
    !matches!(material, Material::Opaque)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_off_stays_opaque_on_modern_builds() {
        // Even a Mica-capable build honors the user's disabled transparency.
        assert_eq!(
            choose_windows_material(Some(26100), false),
            Material::Opaque
        );
    }

    #[test]
    fn mica_from_22h2_onward() {
        assert_eq!(
            choose_windows_material(Some(MICA_MIN_BUILD), true),
            Material::Mica
        );
        assert_eq!(choose_windows_material(Some(26100), true), Material::Mica);
    }

    #[test]
    fn blur_between_19041_and_mica() {
        assert_eq!(
            choose_windows_material(Some(ACRYLIC_MIN_BUILD), true),
            Material::Blur
        );
        // Windows 11 21H2 predates the Mica attribute: acrylic blur is the
        // best available backdrop, even though it is noisier than Mica.
        assert_eq!(choose_windows_material(Some(22000), true), Material::Blur);
        // One below the Mica floor is still acrylic territory.
        assert_eq!(
            choose_windows_material(Some(MICA_MIN_BUILD - 1), true),
            Material::Blur
        );
    }

    #[test]
    fn old_builds_and_probe_failure_stay_opaque() {
        assert_eq!(choose_windows_material(Some(18362), true), Material::Opaque);
        assert_eq!(
            choose_windows_material(Some(ACRYLIC_MIN_BUILD - 1), true),
            Material::Opaque
        );
        assert_eq!(choose_windows_material(None, true), Material::Opaque);
    }

    #[test]
    fn macos_honors_transparency_flag() {
        assert_eq!(choose_macos_material(true), Material::Blur);
        assert_eq!(choose_macos_material(false), Material::Opaque);
    }

    #[test]
    fn opaque_never_tints_the_theme() {
        assert!(!should_tint(Material::Opaque));
        assert!(should_tint(Material::Mica));
        assert!(should_tint(Material::Blur));
    }

    #[test]
    fn dark_mode_uses_a_clearer_tint() {
        assert_eq!(material_tint_alpha(false), MATERIAL_TINT_ALPHA);
        assert_eq!(material_tint_alpha(true), MATERIAL_TINT_ALPHA_DARK);
    }
}
