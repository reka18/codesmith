//! Colors: hex parsing and formatting, the ANSI palette, tints and OKLCH
//! (ported from OpenCode's `theme/color.ts` and `@opencode/theme/tui/color`).

/// An 8-bit RGBA color (OpenTUI keeps floats; every value a theme can name
/// is a whole byte, so the two agree).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 255 }
    }

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }

    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);

    /// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.
    pub fn from_hex(s: &str) -> Option<Rgba> {
        let h = s.strip_prefix('#')?;
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let nib = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
        let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some(match h.len() {
            3 => Rgba::rgb(nib(0)?, nib(1)?, nib(2)?),
            4 => Rgba::new(nib(0)?, nib(1)?, nib(2)?, nib(3)?),
            6 => Rgba::rgb(byte(0)?, byte(2)?, byte(4)?),
            8 => Rgba::new(byte(0)?, byte(2)?, byte(4)?, byte(6)?),
            _ => return None,
        })
    }

    /// `#rrggbb`, with `aa` appended when not opaque.
    pub fn hex(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }

    pub fn rf(self) -> f64 {
        self.r as f64 / 255.0
    }
    pub fn gf(self) -> f64 {
        self.g as f64 / 255.0
    }
    pub fn bf(self) -> f64 {
        self.b as f64 / 255.0
    }

    /// Perceived brightness from 0 to 1 (the weights OpenCode uses).
    pub fn luminance(self) -> f64 {
        0.299 * self.rf() + 0.587 * self.gf() + 0.114 * self.bf()
    }

    pub fn oklch(self) -> Oklch {
        rgb_to_oklch(self.rf(), self.gf(), self.bf())
    }
}

/// Whether `s` is a hex color as theme files spell them.
pub fn is_hex(s: &str) -> bool {
    s.strip_prefix('#').is_some_and(|h| {
        matches!(h.len(), 3 | 4 | 6 | 8) && h.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// The xterm 256-color palette (OpenCode's `ansiToRgba`).
pub fn ansi(code: u64) -> Rgba {
    const BASE: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    if code < 16 {
        return Rgba::from_hex(BASE[code as usize]).unwrap_or_default();
    }
    if code < 232 {
        let i = code - 16;
        let v = |x: u64| if x == 0 { 0 } else { (x * 40 + 55) as u8 };
        return Rgba::rgb(v(i / 36), v((i / 6) % 6), v(i % 6));
    }
    if code < 256 {
        let g = ((code - 232) * 10 + 8) as u8;
        return Rgba::rgb(g, g, g);
    }
    Rgba::rgb(0, 0, 0)
}

/// `base` moved toward `overlay` by `alpha`, opaque.
pub fn tint(base: Rgba, overlay: Rgba, alpha: f64) -> Rgba {
    let mix = |b: u8, o: u8| (b as f64 + (o as f64 - b as f64) * alpha).round() as u8;
    Rgba::rgb(
        mix(base.r, overlay.r),
        mix(base.g, overlay.g),
        mix(base.b, overlay.b),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

pub fn rgb_to_oklch(red: f64, green: f64, blue: f64) -> Oklch {
    let (lr, lg, lb) = (
        srgb_to_linear(red),
        srgb_to_linear(green),
        srgb_to_linear(blue),
    );
    let l = (0.4122214708 * lr + 0.5363325363 * lg + 0.0514459929 * lb).cbrt();
    let m = (0.2119034982 * lr + 0.6806995451 * lg + 0.1073969566 * lb).cbrt();
    let s = (0.0883024619 * lr + 0.2817188376 * lg + 0.6299787005 * lb).cbrt();
    let lightness = 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s;
    let a = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
    let b = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
    let chroma = (a * a + b * b).sqrt();
    let angle = b.atan2(a).to_degrees();
    Oklch {
        l: lightness,
        c: chroma,
        h: if angle < 0.0 { angle + 360.0 } else { angle },
    }
}

fn oklch_to_rgb(c: Oklch) -> (f64, f64, f64) {
    let a = c.c * c.h.to_radians().cos();
    let b = c.c * c.h.to_radians().sin();
    let l = c.l + 0.3963377774 * a + 0.2158037573 * b;
    let m = c.l - 0.1055613458 * a - 0.0638541728 * b;
    let s = c.l - 0.0894841775 * a - 1.291485548 * b;
    let (l, m, s) = (l * l * l, m * m * m, s * s * s);
    (
        linear_to_srgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        linear_to_srgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        linear_to_srgb(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
    )
}

fn in_gamut((r, g, b): (f64, f64, f64)) -> bool {
    (0.0..=1.0).contains(&r) && (0.0..=1.0).contains(&g) && (0.0..=1.0).contains(&b)
}

fn fit(c: Oklch) -> Oklch {
    let base = Oklch {
        l: c.l.clamp(0.0, 1.0),
        c: c.c.max(0.0),
        h: ((c.h % 360.0) + 360.0) % 360.0,
    };
    if in_gamut(oklch_to_rgb(base)) {
        return base;
    }
    for i in 0..24 {
        let next = Oklch {
            c: base.c * 0.9f64.powi(i + 1),
            ..base
        };
        if in_gamut(oklch_to_rgb(next)) {
            return next;
        }
    }
    Oklch { c: 0.0, ..base }
}

/// The OKLCH color brought into the sRGB gamut, as `#rrggbb`.
pub fn oklch_to_hex(c: Oklch) -> String {
    let (r, g, b) = oklch_to_rgb(fit(c));
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgba::rgb(byte(r), byte(g), byte(b)).hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        assert_eq!(Rgba::from_hex("#123456"), Some(Rgba::rgb(18, 52, 86)));
        assert_eq!(Rgba::from_hex("#fff"), Some(Rgba::rgb(255, 255, 255)));
        assert_eq!(Rgba::from_hex("#11223344").unwrap().hex(), "#11223344");
        assert_eq!(Rgba::from_hex("opaque"), None);
        assert!(is_hex("#abcd") && !is_hex("#abcde"));
        assert_eq!(ansi(6), Rgba::rgb(0, 128, 128));
        assert_eq!(ansi(196), Rgba::rgb(255, 0, 0));
        assert_eq!(ansi(244), Rgba::rgb(128, 128, 128));
        let c = Rgba::from_hex("#3b82f6").unwrap();
        assert_eq!(oklch_to_hex(c.oklch()), "#3b82f6");
    }
}
