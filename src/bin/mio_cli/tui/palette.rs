//! Catppuccin Mocha based agent identity palette.
//!
//! Every agent is addressed by a letter label (`A`..`Z`, then `AA`..`AF`) and
//! owns a two-stop gradient. Agents `A`..`H` use the eight most separated
//! Mocha accents verbatim (red, peach, yellow, green, sky, blue, mauve, pink).
//! `I`..`AF` were picked offline by greedy farthest-point sampling in OKLab
//! over the Mocha accent lightness/chroma curve, at four lightness tiers,
//! rejecting candidates too close to Mocha Base, Text, Subtext0 or Lavender
//! (the user's color). Earlier letters are therefore always the most distinct.
//! The table is fixed so colors never drift between platforms or releases.
//! Dark terminals only.

/// Maximum agents in one room.
pub const MAX_AGENTS: usize = 32;

/// sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Mocha base colors used by the interface chrome.
pub mod mocha {
    use super::Rgb;
    pub const TEXT: Rgb = Rgb(0xcd, 0xd6, 0xf4);
    pub const SUBTEXT0: Rgb = Rgb(0xa6, 0xad, 0xc8);
    pub const OVERLAY0: Rgb = Rgb(0x6c, 0x70, 0x86);
    pub const SURFACE1: Rgb = Rgb(0x45, 0x47, 0x5a);
    pub const SURFACE0: Rgb = Rgb(0x31, 0x32, 0x44);
    pub const BASE: Rgb = Rgb(0x1e, 0x1e, 0x2e);
    pub const MANTLE: Rgb = Rgb(0x18, 0x18, 0x25);
    pub const CRUST: Rgb = Rgb(0x11, 0x11, 0x1b);
    pub const LAVENDER: Rgb = Rgb(0xb4, 0xbe, 0xfe);
    pub const RED: Rgb = Rgb(0xf3, 0x8b, 0xa8);
    pub const PEACH: Rgb = Rgb(0xfa, 0xb3, 0x87);
    pub const YELLOW: Rgb = Rgb(0xf9, 0xe2, 0xaf);
    pub const GREEN: Rgb = Rgb(0xa6, 0xe3, 0xa1);
    pub const SKY: Rgb = Rgb(0x89, 0xdc, 0xeb);
    pub const BLUE: Rgb = Rgb(0x89, 0xb4, 0xfa);
    pub const MAUVE: Rgb = Rgb(0xcb, 0xa6, 0xf7);
    pub const PINK: Rgb = Rgb(0xf5, 0xc2, 0xe7);
    pub const MAROON: Rgb = Rgb(0xeb, 0xa0, 0xac);
    pub const SAPPHIRE: Rgb = Rgb(0x74, 0xc7, 0xec);
}

/// One agent's visual identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentColor {
    pub index: usize,
    /// Primary color: label text, composer bar, status spinner.
    pub base: Rgb,
    /// Second gradient stop: hue rotated 30 degrees, slightly lighter.
    pub accent: Rgb,
    /// xterm-256 fallback, distinct across all 32 agents.
    pub ansi256: u8,
}

impl AgentColor {
    /// Color at `t` in `0.0..=1.0` along the agent gradient (OKLab mix).
    pub fn gradient(&self, t: f32) -> Rgb {
        mix(self.base, self.accent, t)
    }

    /// Composer background tint over the terminal background.
    pub fn tint(&self, background: Rgb) -> Rgb {
        blend(self.base, background, 0.14)
    }
}

const AGENTS: [AgentColor; MAX_AGENTS] = [
    AgentColor {
        index: 0,
        base: Rgb(0xf3, 0x8b, 0xa8),
        accent: Rgb(0xff, 0xaa, 0x97),
        ansi256: 211,
    }, // A red
    AgentColor {
        index: 1,
        base: Rgb(0xfa, 0xb3, 0x87),
        accent: Rgb(0xff, 0xca, 0xc6),
        ansi256: 216,
    }, // B peach
    AgentColor {
        index: 2,
        base: Rgb(0xf9, 0xe2, 0xaf),
        accent: Rgb(0xf5, 0xfe, 0xd1),
        ansi256: 223,
    }, // C yellow
    AgentColor {
        index: 3,
        base: Rgb(0xa6, 0xe3, 0xa1),
        accent: Rgb(0xe4, 0xeb, 0xa3),
        ansi256: 150,
    }, // D green
    AgentColor {
        index: 4,
        base: Rgb(0x89, 0xdc, 0xeb),
        accent: Rgb(0xc3, 0xe5, 0xff),
        ansi256: 116,
    }, // E sky
    AgentColor {
        index: 5,
        base: Rgb(0x89, 0xb4, 0xfa),
        accent: Rgb(0x82, 0xd2, 0xf8),
        ansi256: 111,
    }, // F blue
    AgentColor {
        index: 6,
        base: Rgb(0xcb, 0xa6, 0xf7),
        accent: Rgb(0xf6, 0xb4, 0xe7),
        ansi256: 183,
    }, // G mauve
    AgentColor {
        index: 7,
        base: Rgb(0xf5, 0xc2, 0xe7),
        accent: Rgb(0xef, 0xe1, 0xff),
        ansi256: 218,
    }, // H pink
    AgentColor {
        index: 8,
        base: Rgb(0x5c, 0xc0, 0x9d),
        accent: Rgb(0x6e, 0xcf, 0xd2),
        ansi256: 72,
    }, // I h168-0.12
    AgentColor {
        index: 9,
        base: Rgb(0xda, 0x8b, 0x54),
        accent: Rgb(0xed, 0x9a, 0x93),
        ansi256: 173,
    }, // J h54-0.12
    AgentColor {
        index: 10,
        base: Rgb(0xb2, 0x80, 0xd6),
        accent: Rgb(0xdb, 0x8f, 0xc2),
        ansi256: 140,
    }, // K h310-0.12
    AgentColor {
        index: 11,
        base: Rgb(0x37, 0xa8, 0xcd),
        accent: Rgb(0x58, 0xbf, 0xbe),
        ansi256: 38,
    }, // L h224-0.12
    AgentColor {
        index: 12,
        base: Rgb(0xb9, 0xbb, 0x6b),
        accent: Rgb(0xaa, 0xd7, 0xa0),
        ansi256: 143,
    }, // M h110-0.12
    AgentColor {
        index: 13,
        base: Rgb(0x61, 0xc3, 0xd8),
        accent: Rgb(0x87, 0xd8, 0xcb),
        ansi256: 74,
    }, // N h214-0.07
    AgentColor {
        index: 14,
        base: Rgb(0x85, 0x95, 0xe1),
        accent: Rgb(0xbb, 0x9e, 0xdf),
        ansi256: 104,
    }, // O h274-0.12
    AgentColor {
        index: 15,
        base: Rgb(0xd5, 0x75, 0xa1),
        accent: Rgb(0xcd, 0x94, 0xd5),
        ansi256: 175,
    }, // P h352-0.12
    AgentColor {
        index: 16,
        base: Rgb(0xcd, 0x92, 0xc7),
        accent: Rgb(0xea, 0xa5, 0xb9),
        ansi256: 176,
    }, // Q h330-0.12
    AgentColor {
        index: 17,
        base: Rgb(0xdb, 0xcb, 0x89),
        accent: Rgb(0xff, 0xd6, 0xad),
        ansi256: 186,
    }, // R h96-0.07
    AgentColor {
        index: 18,
        base: Rgb(0xdd, 0x70, 0x64),
        accent: Rgb(0xdf, 0x94, 0x59),
        ansi256: 167,
    }, // S h28-0.12
    AgentColor {
        index: 19,
        base: Rgb(0xae, 0xed, 0xd8),
        accent: Rgb(0xdb, 0xfc, 0xd7),
        ansi256: 158,
    }, // T h172+0.04
    AgentColor {
        index: 20,
        base: Rgb(0x88, 0xce, 0x87),
        accent: Rgb(0x7f, 0xe3, 0xc8),
        ansi256: 114,
    }, // U h144-0.07
    AgentColor {
        index: 21,
        base: Rgb(0xfa, 0xa7, 0xae),
        accent: Rgb(0xfd, 0xbf, 0xe2),
        ansi256: 217,
    }, // V h14+0.04
    AgentColor {
        index: 22,
        base: Rgb(0xd8, 0xa6, 0x66),
        accent: Rgb(0xce, 0xc6, 0x85),
        ansi256: 179,
    }, // W h72-0.12
    AgentColor {
        index: 23,
        base: Rgb(0xe1, 0xa9, 0xd2),
        accent: Rgb(0xdc, 0xc5, 0xf7),
        ansi256: 182,
    }, // X h336-0.07
    AgentColor {
        index: 24,
        base: Rgb(0xac, 0x98, 0xec),
        accent: Rgb(0xdc, 0xa4, 0xe0),
        ansi256: 141,
    }, // Y h294-0.07
    AgentColor {
        index: 25,
        base: Rgb(0x73, 0xcd, 0xbc),
        accent: Rgb(0xa9, 0xdc, 0xb3),
        ansi256: 80,
    }, // Z h180-0.07
    AgentColor {
        index: 26,
        base: Rgb(0xd6, 0xe2, 0x9c),
        accent: Rgb(0xcd, 0xfc, 0xcf),
        ansi256: 187,
    }, // AA h116+0
    AgentColor {
        index: 27,
        base: Rgb(0xeb, 0x85, 0x75),
        accent: Rgb(0xf2, 0x9b, 0xb6),
        ansi256: 174,
    }, // AB h30-0.07
    AgentColor {
        index: 28,
        base: Rgb(0xfe, 0xc9, 0xa3),
        accent: Rgb(0xfc, 0xe7, 0xb6),
        ansi256: 222,
    }, // AC h56+0.04
    AgentColor {
        index: 29,
        base: Rgb(0x62, 0xa5, 0xe7),
        accent: Rgb(0x5f, 0xc2, 0xdf),
        ansi256: 75,
    }, // AD h250-0.07
    AgentColor {
        index: 30,
        base: Rgb(0x74, 0xbf, 0x74),
        accent: Rgb(0x66, 0xd4, 0xb7),
        ansi256: 107,
    }, // AE h144-0.12
    AgentColor {
        index: 31,
        base: Rgb(0xe6, 0x76, 0x87),
        accent: Rgb(0xe4, 0x92, 0xc6),
        ansi256: 204,
    }, // AF h12-0.07
];

/// How much color the terminal gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
    /// `NO_COLOR`: agents are told apart by letter labels only.
    None,
}

impl ColorMode {
    /// Pick a mode from environment lookups: a non-empty `NO_COLOR` disables
    /// color, `COLORTERM=truecolor|24bit` enables truecolor, anything else
    /// gets the 256-color table.
    pub fn detect(var: impl Fn(&str) -> Option<String>) -> Self {
        if var("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            return Self::None;
        }
        match var("COLORTERM").as_deref() {
            Some("truecolor" | "24bit") => Self::TrueColor,
            _ => Self::Ansi256,
        }
    }
}

/// Letter label for an agent index: `0 -> A`, `25 -> Z`, `26 -> AA`.
pub fn label(index: usize) -> Option<String> {
    match index {
        0..26 => Some(char::from(b'A' + index as u8).to_string()),
        26..MAX_AGENTS => Some(format!("A{}", char::from(b'A' + (index - 26) as u8))),
        _ => None,
    }
}

/// Inverse of [`label`]. Only upper-case labels are agent names; lower-case
/// text after `@` is left to file completion.
pub fn index_of(label: &str) -> Option<usize> {
    let bytes = label.as_bytes();
    let index = match bytes {
        [c] if c.is_ascii_uppercase() => usize::from(c - b'A'),
        [b'A', c] if c.is_ascii_uppercase() => 26 + usize::from(c - b'A'),
        _ => return None,
    };
    (index < MAX_AGENTS).then_some(index)
}

/// Identity for an agent index, or `None` beyond [`MAX_AGENTS`].
pub fn agent(index: usize) -> Option<AgentColor> {
    AGENTS.get(index).copied()
}

/// All agent identities in label order.
pub fn all() -> &'static [AgentColor; MAX_AGENTS] {
    &AGENTS
}

/// `alpha` of `fg` over `bg`, in sRGB like terminal compositing.
pub fn blend(fg: Rgb, bg: Rgb, alpha: f32) -> Rgb {
    let channel =
        |f: u8, b: u8| (f32::from(f) * alpha + f32::from(b) * (1.0 - alpha)).round() as u8;
    Rgb(
        channel(fg.0, bg.0),
        channel(fg.1, bg.1),
        channel(fg.2, bg.2),
    )
}

/// Perceptual mix in OKLab.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let (a, b) = (Lab::from_rgb(a), Lab::from_rgb(b));
    Lab {
        l: a.l + (b.l - a.l) * t,
        a: a.a + (b.a - a.a) * t,
        b: a.b + (b.b - a.b) * t,
    }
    .to_rgb()
}

/// OKLab distance, used by tests and the 256-color fallback.
pub fn distance(a: Rgb, b: Rgb) -> f32 {
    let (a, b) = (Lab::from_rgb(a), Lab::from_rgb(b));
    ((a.l - b.l).powi(2) + (a.a - b.a).powi(2) + (a.b - b.b).powi(2)).sqrt()
}

/// Shimmer intensity for character `position` of `width` at animation
/// `phase` (`0.0..1.0`, one sweep per period). Cosine band like codex.
pub fn shimmer(position: usize, width: usize, phase: f32) -> f32 {
    const PADDING: f32 = 8.0;
    const HALF_BAND: f32 = 4.0;
    let period = width as f32 + PADDING * 2.0;
    let center = phase.rem_euclid(1.0) * period;
    let distance = (position as f32 + PADDING - center).abs();
    if distance > HALF_BAND {
        0.0
    } else {
        0.5 * (1.0 + (std::f32::consts::PI * distance / HALF_BAND).cos())
    }
}

/// Braille spinner frames.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Spinner frame for a tick, offset per agent so a room does not spin in
/// lockstep.
pub fn spinner(tick: usize, agent: usize) -> &'static str {
    SPINNER[(tick + agent * 3) % SPINNER.len()]
}

/// Nearest xterm-256 color for terminals without truecolor.
pub fn nearest_ansi256(color: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| (i16::from(**level) - i16::from(v)).abs())
            .map_or(0, |(index, _)| index)
    };
    let (r, g, b) = (level(color.0), level(color.1), level(color.2));
    let cube = Rgb(LEVELS[r], LEVELS[g], LEVELS[b]);
    let cube_index = 16 + 36 * r + 6 * g + b;
    let gray_step = ((u16::from(color.0) + u16::from(color.1) + u16::from(color.2)) / 3)
        .saturating_sub(8)
        .min(230)
        / 10;
    let gray_value = (8 + gray_step * 10) as u8;
    let gray = Rgb(gray_value, gray_value, gray_value);
    if distance(gray, color) < distance(cube, color) {
        232 + gray_step as u8
    } else {
        cube_index as u8
    }
}

/// An `@label` mention found in input text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mention {
    pub agent: usize,
    /// Byte range of `@label`, for highlighting.
    pub start: usize,
    pub end: usize,
}

/// Find `@A`..`@AF` mentions. `@` must not follow an ASCII word character
/// (so `a@B.com` is not a mention), and the label must not continue as a
/// path or identifier (`@README`, `@B/src`, `@A.md`, `@B-2`).
pub fn mentions(text: &str) -> Vec<Mention> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'@' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            continue;
        }
        let label_end = bytes[index..]
            .iter()
            .position(|byte| !byte.is_ascii_uppercase())
            .map_or(bytes.len(), |offset| index + offset);
        let continues = match bytes.get(label_end) {
            Some(byte) if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'-') => {
                true
            }
            Some(b'.') => bytes
                .get(label_end + 1)
                .is_some_and(u8::is_ascii_alphanumeric),
            _ => false,
        };
        if !continues && let Some(agent) = index_of(&text[index..label_end]) {
            found.push(Mention {
                agent,
                start,
                end: label_end,
            });
        }
        index = label_end.max(index);
    }
    found
}

/// Distinct mentioned agents in first-mention order.
pub fn mentioned_agents(text: &str) -> Vec<usize> {
    let mut agents = Vec::new();
    for mention in mentions(text) {
        if !agents.contains(&mention.agent) {
            agents.push(mention.agent);
        }
    }
    agents
}

#[derive(Clone, Copy, Debug)]
struct Lab {
    l: f32,
    a: f32,
    b: f32,
}

fn to_linear(channel: u8) -> f32 {
    let c = f32::from(channel) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn from_linear(channel: f32) -> f32 {
    if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

impl Lab {
    fn from_rgb(rgb: Rgb) -> Self {
        let (r, g, b) = (to_linear(rgb.0), to_linear(rgb.1), to_linear(rgb.2));
        let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
        let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
        let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
        Self {
            l: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
            a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
            b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
        }
    }

    fn linear(self) -> [f32; 3] {
        let l = (self.l + 0.396_337_78 * self.a + 0.215_803_76 * self.b).powi(3);
        let m = (self.l - 0.105_561_346 * self.a - 0.063_854_17 * self.b).powi(3);
        let s = (self.l - 0.089_484_18 * self.a - 1.291_485_5 * self.b).powi(3);
        [
            4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
            -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
            -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
        ]
    }

    fn to_rgb(self) -> Rgb {
        let [r, g, b] = self.linear();
        let channel = |c: f32| (from_linear(c.clamp(0.0, 1.0)) * 255.0).round() as u8;
        Rgb(channel(r), channel(g), channel(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip_for_every_agent() {
        for index in 0..MAX_AGENTS {
            let label = label(index).unwrap();
            assert_eq!(index_of(&label), Some(index), "{label}");
        }
        assert_eq!(label(0).as_deref(), Some("A"));
        assert_eq!(label(25).as_deref(), Some("Z"));
        assert_eq!(label(26).as_deref(), Some("AA"));
        assert_eq!(label(31).as_deref(), Some("AF"));
        assert_eq!(label(32), None);
        assert_eq!(index_of("AG"), None);
        assert_eq!(index_of("b"), None);
        assert_eq!(index_of("BA"), None);
        assert_eq!(index_of(""), None);
    }

    #[test]
    fn first_eight_agents_are_mocha_accents_and_far_apart() {
        let accents = [
            mocha::RED,
            mocha::PEACH,
            mocha::YELLOW,
            mocha::GREEN,
            mocha::SKY,
            mocha::BLUE,
            mocha::MAUVE,
            mocha::PINK,
        ];
        for (index, accent) in accents.iter().enumerate() {
            assert_eq!(agent(index).unwrap().base, *accent);
        }
        for (i, a) in accents.iter().enumerate() {
            for b in &accents[i + 1..] {
                assert!(distance(*a, *b) > 0.08, "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn all_32_agents_are_distinct_and_readable_on_mocha() {
        let palette = all();
        for (index, color) in palette.iter().enumerate() {
            assert_eq!(color.index, index);
        }
        let name = |color: &AgentColor| label(color.index).unwrap();
        for (i, a) in palette.iter().enumerate() {
            for b in &palette[i + 1..] {
                let gap = distance(a.base, b.base);
                assert!(gap > 0.045, "{} {} too close: {gap}", name(a), name(b));
                // Earlier letters are picked first, so the first 16 are wider apart.
                if b.index < 16 {
                    assert!(gap > 0.07, "{} {} too close: {gap}", name(a), name(b));
                }
                assert_ne!(a.ansi256, b.ansi256, "{} {}", name(a), name(b));
            }
            assert!(distance(a.base, mocha::BASE) > 0.45, "{}", name(a));
            for chrome in [mocha::TEXT, mocha::SUBTEXT0, mocha::LAVENDER] {
                assert!(distance(a.base, chrome) > 0.05, "{} vs {chrome:?}", name(a));
            }
            assert!(
                distance(a.base, a.accent) > 0.03,
                "flat gradient {}",
                name(a)
            );
            assert!((16..=255).contains(&a.ansi256), "{}", name(a));
        }
        assert_eq!(agent(MAX_AGENTS), None);
    }

    #[test]
    fn gradient_and_tint_stay_between_their_endpoints() {
        let a = agent(3).unwrap();
        assert_eq!(a.gradient(0.0), a.base);
        assert_eq!(a.gradient(1.0), a.accent);
        let tint = a.tint(mocha::BASE);
        assert!(distance(tint, mocha::BASE) < distance(tint, a.base));
    }

    #[test]
    fn color_mode_follows_no_color_and_colorterm() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(ColorMode::detect(env(&[])), ColorMode::Ansi256);
        assert_eq!(
            ColorMode::detect(env(&[("COLORTERM", "truecolor")])),
            ColorMode::TrueColor
        );
        assert_eq!(
            ColorMode::detect(env(&[("COLORTERM", "24bit"), ("NO_COLOR", "1")])),
            ColorMode::None
        );
        assert_eq!(
            ColorMode::detect(env(&[("COLORTERM", "24bit"), ("NO_COLOR", "")])),
            ColorMode::TrueColor
        );
    }

    #[test]
    fn mentions_accept_letter_labels_only() {
        let agents = |text: &str| mentioned_agents(text);
        assert_eq!(agents("@B 和 @E 协商后端与 API"), vec![1, 4]);
        assert_eq!(agents("@B,@B 再看看@C。"), vec![1, 2]);
        assert_eq!(agents("交给@AA和@AF"), vec![26, 31]);
        assert_eq!(agents("(@D) done."), vec![3]);
        assert_eq!(agents("@B."), vec![1]);
        assert!(agents("mail a@B.com").is_empty());
        assert!(agents("@README @B/src @A.md @B-2 @b @AG @Bx").is_empty());
        let found = mentions("hi @C!");
        assert_eq!(
            found,
            vec![Mention {
                agent: 2,
                start: 3,
                end: 5
            }]
        );
    }

    #[test]
    fn shimmer_peaks_at_the_band_center() {
        let width = 20;
        let phase = (5.0 + 8.0) / (width as f32 + 16.0);
        assert!((shimmer(5, width, phase) - 1.0).abs() < 1e-4);
        assert_eq!(shimmer(15, width, phase), 0.0);
        assert_ne!(spinner(0, 0), spinner(0, 1));
    }

    #[test]
    fn ansi256_fallback_picks_the_obvious_slot() {
        assert_eq!(nearest_ansi256(Rgb(255, 0, 0)), 196);
        assert_eq!(nearest_ansi256(Rgb(0, 0, 0)), 16);
        assert_eq!(nearest_ansi256(Rgb(128, 128, 128)), 244);
    }
}
