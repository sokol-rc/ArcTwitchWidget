//! The one place the ARC Live mark is loaded from, so the window, the taskbar
//! and the tray always show the same icon.
//!
//! The bytes are raw RGBA produced by `scripts/generate-icon.py`; keeping them
//! decoded avoids pulling an image decoder into the application.

const ICON_SIZE: usize = 128;
const ICON_RGBA: &[u8] = include_bytes!("../../../assets/icon-128.rgba");

/// The mark at its native size, for the window and the taskbar.
pub fn window_icon() -> eframe::egui::IconData {
    eframe::egui::IconData {
        rgba: ICON_RGBA.to_vec(),
        width: ICON_SIZE as u32,
        height: ICON_SIZE as u32,
    }
}

/// The mark box-filtered down to `size`, which must divide 128 evenly.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn icon_rgba(size: usize) -> Vec<u8> {
    let factor = ICON_SIZE / size;
    debug_assert_eq!(ICON_SIZE % size, 0, "icon size must divide {ICON_SIZE}");
    let samples = (factor * factor) as u32;
    let mut pixels = Vec::with_capacity(size * size * 4);
    for row in 0..size {
        for column in 0..size {
            let mut channels = [0u32; 4];
            for sub_row in 0..factor {
                for sub_column in 0..factor {
                    let source =
                        ((row * factor + sub_row) * ICON_SIZE + column * factor + sub_column) * 4;
                    for (total, value) in channels.iter_mut().zip(&ICON_RGBA[source..source + 4]) {
                        *total += u32::from(*value);
                    }
                }
            }
            pixels.extend(channels.map(|total| (total / samples) as u8));
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_icon_asset_in_sync_with_its_declared_size() {
        assert_eq!(ICON_RGBA.len(), ICON_SIZE * ICON_SIZE * 4);
    }

    #[test]
    fn scales_the_icon_down_to_the_tray_size() {
        assert_eq!(icon_rgba(32).len(), 32 * 32 * 4);
        assert_eq!(icon_rgba(16).len(), 16 * 16 * 4);
    }
}
