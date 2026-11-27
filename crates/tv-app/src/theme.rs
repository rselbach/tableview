use rune::{Color, oklch};
use rune_kit::{Kit, PaletteSeed, Radius};
use tv_core::{AccentColor, EnvTag};

/// Builds TableView's cross-platform OKLCH design tokens for light or dark appearance.
pub fn kit(dark: bool) -> Kit {
    let seed = if dark {
        PaletteSeed {
            bg: oklch(0.165, 0.006, 255.0),
            fg: oklch(0.935, 0.005, 255.0),
            accent: oklch(0.66, 0.145, 240.0),
        }
    } else {
        PaletteSeed {
            bg: oklch(0.985, 0.003, 255.0),
            fg: oklch(0.18, 0.008, 255.0),
            accent: oklch(0.52, 0.16, 245.0),
        }
    };

    let mut k = Kit::from_palette(seed);
    k.radius = Radius {
        xs: 3.0,
        sm: 4.0,
        md: 6.0,
        lg: 10.0,
        xl: 12.0,
    };
    k.metrics.divider_thickness = Some(1.0);
    k
}

/// Visual tokens for connection environment tags and status strips.
#[derive(Clone, Copy, Debug)]
pub struct EnvPalette {
    pub strip: Color,
    pub pill_bg: Color,
    pub pill_fg: Color,
    pub pill_border: Color,
    pub dot: Color,
}

/// Resolves semantic colors for an environment tag or user-chosen status color.
pub fn env_colors(env: EnvTag, color: AccentColor, dark: bool) -> EnvPalette {
    let resolved = match color {
        AccentColor::Neutral => env.default_color(),
        other => other,
    };
    let hue = match resolved {
        AccentColor::Neutral => return neutral_env_palette(dark),
        AccentColor::Green => 150.0,
        AccentColor::Blue => 242.0,
        AccentColor::Amber => 75.0,
        AccentColor::Red => 25.0,
        AccentColor::Purple => 295.0,
    };
    if dark {
        EnvPalette {
            strip: oklch(0.68, 0.16, hue),
            pill_bg: oklch(0.28, 0.055, hue),
            pill_fg: oklch(0.84, 0.10, hue),
            pill_border: oklch(0.45, 0.09, hue),
            dot: oklch(0.72, 0.16, hue),
        }
    } else {
        EnvPalette {
            strip: oklch(0.58, 0.17, hue),
            pill_bg: oklch(0.94, 0.04, hue),
            pill_fg: oklch(0.38, 0.13, hue),
            pill_border: oklch(0.82, 0.08, hue),
            dot: oklch(0.56, 0.17, hue),
        }
    }
}

fn neutral_env_palette(dark: bool) -> EnvPalette {
    if dark {
        EnvPalette {
            strip: oklch(0.42, 0.01, 255.0),
            pill_bg: oklch(0.24, 0.008, 255.0),
            pill_fg: oklch(0.78, 0.008, 255.0),
            pill_border: oklch(0.34, 0.008, 255.0),
            dot: oklch(0.62, 0.01, 255.0),
        }
    } else {
        EnvPalette {
            strip: oklch(0.70, 0.01, 255.0),
            pill_bg: oklch(0.93, 0.006, 255.0),
            pill_fg: oklch(0.36, 0.01, 255.0),
            pill_border: oklch(0.84, 0.008, 255.0),
            dot: oklch(0.52, 0.01, 255.0),
        }
    }
}

/// Colors for the disconnected or failed banner pill in the workspace toolbar.
pub fn disconnected_banner_colors(dark: bool) -> EnvPalette {
    if dark {
        EnvPalette {
            strip: oklch(0.62, 0.19, 22.0),
            pill_bg: oklch(0.29, 0.08, 22.0),
            pill_fg: oklch(0.88, 0.09, 22.0),
            pill_border: oklch(0.52, 0.14, 22.0),
            dot: oklch(0.68, 0.19, 22.0),
        }
    } else {
        EnvPalette {
            strip: oklch(0.58, 0.21, 22.0),
            pill_bg: oklch(0.89, 0.08, 20.0),
            pill_fg: oklch(0.38, 0.15, 22.0),
            pill_border: oklch(0.74, 0.14, 20.0),
            dot: oklch(0.58, 0.20, 22.0),
        }
    }
}

/// Returns the swatch color rendered inside the connection editor's color picker row.
pub fn swatch_color(color: AccentColor, dark: bool) -> Color {
    match color {
        AccentColor::Neutral => {
            if dark {
                oklch(0.45, 0.01, 255.0)
            } else {
                oklch(0.86, 0.008, 255.0)
            }
        }
        AccentColor::Green => oklch(0.72, 0.13, 145.0),
        AccentColor::Blue => oklch(0.68, 0.13, 252.0),
        AccentColor::Amber => oklch(0.82, 0.12, 88.0),
        AccentColor::Red => oklch(0.74, 0.14, 22.0),
        AccentColor::Purple => oklch(0.70, 0.14, 298.0),
    }
}

/// Visual tokens for the docked auto-update card in the launcher rail and workspace sidebar.
#[derive(Clone, Copy, Debug)]
pub struct UpdateCardPalette {
    pub bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub dot: Color,
    pub accent_bg: Color,
    pub accent_fg: Color,
}

/// Resolves blue accent tokens for the docked auto-update card in dark or light mode.
pub fn update_card_colors(dark: bool) -> UpdateCardPalette {
    if dark {
        UpdateCardPalette {
            bg: oklch(0.255, 0.055, 240.0),
            fg: oklch(0.87, 0.08, 240.0),
            muted: oklch(0.72, 0.02, 245.0),
            border: oklch(0.46, 0.105, 240.0),
            dot: oklch(0.66, 0.145, 240.0),
            accent_bg: oklch(0.62, 0.155, 240.0),
            accent_fg: oklch(0.99, 0.002, 240.0),
        }
    } else {
        UpdateCardPalette {
            bg: oklch(0.935, 0.042, 245.0),
            fg: oklch(0.35, 0.14, 245.0),
            muted: oklch(0.46, 0.02, 245.0),
            border: oklch(0.75, 0.10, 245.0),
            dot: oklch(0.52, 0.16, 245.0),
            accent_bg: oklch(0.52, 0.16, 245.0),
            accent_fg: oklch(0.99, 0.002, 245.0),
        }
    }
}
