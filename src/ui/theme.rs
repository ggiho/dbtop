use ratatui::style::Color;

type Rgb = (u8, u8, u8);

/// Three colour stops, blended linearly.
#[derive(Clone, Copy)]
pub struct Gradient([Rgb; 3]);

impl Gradient {
    /// `t` is clamped to `0.0..=1.0`.
    pub fn at(&self, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0) * 2.0;
        let (from, to, f) = if t < 1.0 {
            (self.0[0], self.0[1], t)
        } else {
            (self.0[1], self.0[2], t - 1.0)
        };
        let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * f).round() as u8;
        Color::Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
    }
}

pub struct Theme {
    pub border: Color,
    pub text: Color,
    pub dim: Color,
    pub accent: Color,
    pub selection: Color,
    pub warn: Color,
    pub crit: Color,
    /// For values with no good or bad direction, e.g. throughput.
    pub cool: Gradient,
    /// For load: green when idle, red when saturated.
    pub heat: Gradient,
    /// SQL text, in the same palette.
    pub keyword: Color,
    pub function: Color,
    pub string: Color,
    pub number: Color,
    pub comment: Color,
    /// A name set in quotes: `status`, "Order".
    pub quoted: Color,
    pub operator: Color,
    /// A placeholder or a variable: ?, $1, @name.
    pub param: Color,
}

pub const THEME: Theme = Theme {
    border: Color::Rgb(0x5c, 0x63, 0x70),
    text: Color::Rgb(0xdc, 0xdf, 0xe4),
    dim: Color::Rgb(0x8a, 0x91, 0x9e),
    accent: Color::Rgb(0x61, 0xaf, 0xef),
    selection: Color::Rgb(0x3e, 0x44, 0x52),
    warn: Color::Rgb(0xe5, 0xc0, 0x7b),
    crit: Color::Rgb(0xe0, 0x6c, 0x75),
    cool: Gradient([(0x56, 0xb6, 0xc2), (0x61, 0xaf, 0xef), (0xc6, 0x78, 0xdd)]),
    heat: Gradient([(0x98, 0xc3, 0x79), (0xe5, 0xc0, 0x7b), (0xe0, 0x6c, 0x75)]),
    keyword: Color::Rgb(0xc6, 0x78, 0xdd),
    function: Color::Rgb(0x61, 0xaf, 0xef),
    string: Color::Rgb(0x98, 0xc3, 0x79),
    number: Color::Rgb(0xd1, 0x9a, 0x66),
    comment: Color::Rgb(0x7f, 0x84, 0x8e),
    quoted: Color::Rgb(0xe5, 0xc0, 0x7b),
    operator: Color::Rgb(0x56, 0xb6, 0xc2),
    param: Color::Rgb(0xe0, 0x6c, 0x75),
};

#[cfg(test)]
mod tests {
    use super::*;

    const G: Gradient = Gradient([(0, 0, 0), (100, 100, 100), (200, 0, 50)]);

    #[test]
    fn hits_the_stops() {
        assert_eq!(G.at(0.0), Color::Rgb(0, 0, 0));
        assert_eq!(G.at(0.5), Color::Rgb(100, 100, 100));
        assert_eq!(G.at(1.0), Color::Rgb(200, 0, 50));
    }

    #[test]
    fn blends_between_stops_and_clamps() {
        assert_eq!(G.at(0.25), Color::Rgb(50, 50, 50));
        assert_eq!(G.at(0.75), Color::Rgb(150, 50, 75));
        assert_eq!(G.at(-3.0), G.at(0.0));
        assert_eq!(G.at(9.0), G.at(1.0));
    }
}
