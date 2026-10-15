// Render a tray icon as a 32x32 PNG-encoded RGBA buffer.
// Design: a circular activity ring around a Claude-orange center.
// The ring fill represents *used* (filled clockwise from 12 o'clock).
// Center color shifts from green → amber → rose as used % grows.

use image::{ImageBuffer, Rgba, RgbaImage};

const SIZE: u32 = 32;
const CENTER: f32 = SIZE as f32 / 2.0;
const RING_OUTER: f32 = 14.5;
const RING_INNER: f32 = 11.5;
const CORE_RADIUS: f32 = 9.5;

#[derive(Clone, Copy)]
pub enum Mood {
    Ok,         // used < 留意值
    Warn,       // 留意值 ≤ used < 撞牆值
    Danger,     // used ≥ 撞牆值
    Loading,    // refreshing
    NeedsLogin, // session expired
    Idle,       // never scraped
}

pub fn render(used_percent: Option<f32>, mood: Mood) -> RgbaImage {
    let used_pct = used_percent
        .map(|u| u.clamp(0.0, 100.0))
        .unwrap_or(0.0);

    let core = match mood {
        Mood::Ok => [76, 175, 120, 255],
        Mood::Warn => [228, 165, 65, 255],
        Mood::Danger => [225, 95, 105, 255],
        Mood::Loading => [200, 130, 75, 255],
        Mood::NeedsLogin => [200, 130, 75, 255],
        Mood::Idle => [160, 160, 170, 255],
    };

    let ring_active = match mood {
        Mood::Ok => [120, 215, 165, 255],
        Mood::Warn => [255, 205, 110, 255],
        Mood::Danger => [255, 130, 145, 255],
        Mood::Loading => [220, 165, 100, 255],
        Mood::NeedsLogin => [225, 95, 105, 255],
        Mood::Idle => [180, 180, 190, 255],
    };
    let ring_inactive = [255, 255, 255, 60];

    let mut img: RgbaImage = ImageBuffer::from_pixel(SIZE, SIZE, Rgba([0, 0, 0, 0]));

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - CENTER;
            let dy = y as f32 + 0.5 - CENTER;
            let dist = (dx * dx + dy * dy).sqrt();

            let pixel = if dist <= CORE_RADIUS {
                Some(Rgba(core))
            } else if (RING_INNER..=RING_OUTER).contains(&dist) {
                let mut angle = dy.atan2(dx).to_degrees();
                angle += 90.0;
                if angle < 0.0 {
                    angle += 360.0;
                }
                let angle = angle.rem_euclid(360.0);
                let used_angle = used_pct / 100.0 * 360.0;

                let active = angle <= used_angle;
                Some(Rgba(if active { ring_active } else { ring_inactive }))
            } else {
                None
            };

            if let Some(p) = pixel {
                img.put_pixel(x, y, p);
            }
        }
    }

    if matches!(mood, Mood::Loading) {
        for d in 0..6u32 {
            let x = (CENTER as i32) + d as i32;
            let y = 2;
            if (0..SIZE as i32).contains(&x) && (0..SIZE as i32).contains(&y) {
                img.put_pixel(x as u32, y as u32, Rgba([255, 255, 255, 230]));
            }
        }
    }

    if matches!(mood, Mood::NeedsLogin) {
        let cx = CENTER as i32;
        for y in 10..18 {
            img.put_pixel(cx as u32, y as u32, Rgba([255, 255, 255, 250]));
            img.put_pixel((cx + 1) as u32, y as u32, Rgba([255, 255, 255, 250]));
        }
        for y in 20..23 {
            img.put_pixel(cx as u32, y as u32, Rgba([255, 255, 255, 250]));
            img.put_pixel((cx + 1) as u32, y as u32, Rgba([255, 255, 255, 250]));
        }
    }

    img
}

pub fn to_png_bytes(img: &RgbaImage) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    img.write_to(&mut cursor, image::ImageFormat::Png).ok();
    buf
}

/// v1.2 (D74 決策點 4): tray colour steps at the same 留意值／撞牆值 as the
/// widget and the toasts — one source for every surface.
pub fn mood_from_state(
    status: crate::types::ScrapeStatus,
    used: Option<f32>,
    th: crate::types::AlertThresholds,
) -> Mood {
    use crate::types::ScrapeStatus;
    match status {
        ScrapeStatus::Loading => Mood::Loading,
        ScrapeStatus::NeedsLogin => Mood::NeedsLogin,
        ScrapeStatus::Idle => Mood::Idle,
        ScrapeStatus::Error => Mood::Idle,
        ScrapeStatus::Ok => match used {
            Some(u) if u >= th.wall => Mood::Danger,
            Some(u) if u >= th.notice => Mood::Warn,
            Some(_) => Mood::Ok,
            None => Mood::Idle,
        },
    }
}
