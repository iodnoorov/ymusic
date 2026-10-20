//! Equalizer presets, applied as an mpv audio filter (ffmpeg `equalizer` bands).

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

impl Preset {
    pub fn is_flat(&self) -> bool {
        self.bands.is_empty()
    }

    /// Value for mpv's `af` property; empty for the flat preset.
    pub fn filter(&self) -> String {
        if self.is_flat() {
            return String::new();
        }
        let last = self.bands.len() - 1;
        let bands: Vec<String> = self
            .bands
            .iter()
            .enumerate()
            .map(|(i, (f, g))| match self.shape {
                Shape::Octave => format!("equalizer=f={f}:t=o:w=1:g={g}"),
                // Web Audio shelves have a slope of 1 (their Q is not used).
                Shape::Site if i == 0 => format!("lowshelf=f={f}:t=s:w=1:g={g}"),
                Shape::Site if i == last => format!("highshelf=f={f}:t=s:w=1:g={g}"),
                Shape::Site => format!("equalizer=f={f}:t=q:w=1:g={g}"),
            })
            .collect();
        // The limiter keeps boosted bands from clipping.
        format!("@eq:lavfi=[{},alimiter=limit=0.9]", bands.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets() {
        assert_eq!(index("rock"), 3);
        assert_eq!(index("unknown"), 0);
        assert_eq!(PRESETS[0].filter(), "");
        assert_eq!(
            PRESETS[index("soft")].filter(),
            "@eq:lavfi=[equalizer=f=4000:t=o:w=1:g=-3,equalizer=f=10000:t=o:w=1:g=-5,alimiter=limit=0.9]"
        );
        let laptop = PRESETS[index("laptop")].filter();
        assert!(laptop.starts_with("@eq:lavfi=[lowshelf=f=60:t=s:w=1:g=2,equalizer=f=170:t=q:w=1:g=5.5,"), "{laptop}");
        assert!(laptop.ends_with(",highshelf=f=16000:t=s:w=1:g=7,alimiter=limit=0.9]"), "{laptop}");
        let ids: std::collections::HashSet<_> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), PRESETS.len());
    }
}
