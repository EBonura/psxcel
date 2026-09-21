//! Frozen arithmetic oracle from the pre-harmonisation PSXcel renderer.
use super::lerp;

fn legacy(a: (u8, u8, u8), b: (u8, u8, u8), num: u16, den: u16) -> (u8, u8, u8) {
    let (num, den) = (num.min(den) as i32, den.max(1) as i32);
    let ch = |x: u8, y: u8| (x as i32 + (y as i32 - x as i32) * num / den) as u8;
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

#[test]
fn shared_lerp_preserves_zero_denominator() {
    for num in [0, 1, 40, u16::MAX] {
        let a = (1, 128, 254);
        let b = (255, 0, 7);
        assert_eq!(lerp(a, b, num, 0), legacy(a, b, num, 0));
    }
}

#[test]
fn shared_lerp_preserves_channel_boundaries_and_rounding() {
    for x in 0..=255u8 {
        for y in 0..=255u8 {
            let a = (x, y, 255 - x);
            let b = (y, x, 255 - y);
            for den in [1, 2, 3, 32, 40, u16::MAX] {
                for num in [0, 1, den / 2, den, den.saturating_add(1), u16::MAX] {
                    assert_eq!(lerp(a, b, num, den), legacy(a, b, num, den));
                }
            }
        }
    }
}
