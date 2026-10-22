//! Equalizer presets: bands of peaking and shelving filters for the player.

use crate::Biquad;

pub struct Preset {
    /// Stored in the config.
    pub id: &'static str,
    pub name: &'static str,
    /// Bands: (frequency in Hz, gain in dB).
    bands: &'static [(u32, f64)],
    shape: Shape,
}

/// How the bands are turned into filters.
#[derive(Clone, Copy)]
enum Shape {
    /// Peaking, one octave wide.
    Octave,
    /// As the music.yandex.ru player does it: low shelf, peaking with Q = 1, high shelf.
    Site,
}

pub const PRESETS: &[Preset] = &[
    Preset { id: "flat", name: "Обычный", bands: &[], shape: Shape::Octave },
    Preset { id: "bass", name: "Больше баса", bands: &[(60, 6.0), (150, 4.0), (400, -1.0)], shape: Shape::Octave },
    Preset { id: "vocal", name: "Вокал", bands: &[(150, -2.0), (1000, 2.0), (3000, 4.0), (6000, 2.0)], shape: Shape::Octave },
    Preset { id: "rock", name: "Рок", bands: &[(80, 4.0), (250, 1.0), (1000, -2.0), (4000, 3.0), (10000, 4.0)], shape: Shape::Octave },
    Preset {
        id: "electronic",
        name: "Электроника",
        bands: &[(60, 5.0), (200, 2.0), (1000, -1.0), (6000, 3.0), (12000, 4.0)],
        shape: Shape::Octave,
    },
    Preset { id: "soft", name: "Мягкий", bands: &[(4000, -3.0), (10000, -5.0)], shape: Shape::Octave },
    // The «SPEAKERS» preset of the music.yandex.ru player (preamp 1).
    Preset {
        id: "laptop",
        name: "Колонки ноутбука",
        bands: &[
            (60, 2.0),
            (170, 5.5),
            (310, 2.5),
            (600, -1.5),
            (1000, -1.0),
            (3000, 0.5),
            (6000, 2.0),
            (12000, 4.5),
            (14000, 6.0),
            (16000, 7.0),
        ],
        shape: Shape::Site,
    },
];

/// Index of the preset with this id (the flat one if unknown).
pub fn index(id: &str) -> usize {
    PRESETS.iter().position(|p| p.id == id).unwrap_or(0)
}

/// Presets are the same if their ids are.
impl PartialEq for Preset {
    fn eq(&self, other: &Preset) -> bool {
        self.id == other.id
    }
}

impl Preset {
    pub fn is_flat(&self) -> bool {
        self.bands.is_empty()
    }

    /// The bands as filters of the own player; none for the flat preset.
    pub fn biquads(&self, rate: f64) -> Vec<Biquad> {
        let last = self.bands.len().saturating_sub(1);
        self.bands
            .iter()
            .enumerate()
            .map(|(i, &(f, g))| {
                let f = f64::from(f);
                match self.shape {
                    Shape::Octave => Biquad::peaking_octaves(f, 1.0, g, rate),
                    Shape::Site if i == 0 => Biquad::shelf(false, f, 1.0, g, rate),
                    Shape::Site if i == last => Biquad::shelf(true, f, 1.0, g, rate),
                    Shape::Site => Biquad::peaking_q(f, 1.0, g, rate),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets() {
        assert_eq!(index("rock"), 3);
        assert_eq!(index("unknown"), 0);
        assert!(PRESETS[0].is_flat() && PRESETS[0].biquads(44100.0).is_empty());
        // The response of a preset at a frequency, dB: the bands in a row.
        let db = |id: &str, f: f64| -> f64 {
            PRESETS[index(id)].biquads(44100.0).iter().map(|b| 20.0 * b.response(f, 44100.0).log10()).sum()
        };
        assert!((db("soft", 10000.0) + 5.0).abs() < 0.6 && db("soft", 100.0).abs() < 0.1);
        // «Колонки ноутбука»: low shelf of +2 dB below 60 Hz, high shelf up to +7 dB.
        assert_eq!(PRESETS[index("laptop")].biquads(44100.0).len(), 10);
        assert!((db("laptop", 10.0) - 2.0).abs() < 0.3, "{}", db("laptop", 10.0));
        assert!((7.0..8.0).contains(&db("laptop", 20000.0)), "{}", db("laptop", 20000.0));
        let ids: std::collections::HashSet<_> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), PRESETS.len());
    }
}
