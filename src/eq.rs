//! Equalizer presets, applied as an mpv audio filter (ffmpeg `equalizer` bands).

pub struct Preset {
    /// Stored in the config.
    pub id: &'static str,
    pub name: &'static str,
    /// Peaking bands: (frequency in Hz, gain in dB), one octave wide.
    bands: &'static [(u32, f64)],
}

pub const PRESETS: &[Preset] = &[
    Preset { id: "flat", name: "Обычный", bands: &[] },
    Preset { id: "bass", name: "Больше баса", bands: &[(60, 6.0), (150, 4.0), (400, -1.0)] },
    Preset { id: "vocal", name: "Вокал", bands: &[(150, -2.0), (1000, 2.0), (3000, 4.0), (6000, 2.0)] },
    Preset { id: "rock", name: "Рок", bands: &[(80, 4.0), (250, 1.0), (1000, -2.0), (4000, 3.0), (10000, 4.0)] },
    Preset {
        id: "electronic",
        name: "Электроника",
        bands: &[(60, 5.0), (200, 2.0), (1000, -1.0), (6000, 3.0), (12000, 4.0)],
    },
    Preset { id: "soft", name: "Мягкий", bands: &[(4000, -3.0), (10000, -5.0)] },
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
        let bands: Vec<String> = self.bands.iter().map(|(f, g)| format!("equalizer=f={f}:t=o:w=1:g={g}")).collect();
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
        let ids: std::collections::HashSet<_> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), PRESETS.len());
    }
}
