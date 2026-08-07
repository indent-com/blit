//! The one AV1 level table.
//!
//! The level a stream declares (`seq_level_idx` in the sequence header) and
//! the level the server announces in the WebCodecs codec string
//! (`av01.P.LLM.08`) must come from the same computation: the client
//! configures `VideoDecoder` from the announced string, and Chromium picks a
//! decoder backend by the configured level — a stream that then declares a
//! *different* level is rejected by level-gating backends (macOS hardware
//! AV1 tops out at 5.1).  They previously came from two different formulas
//! (MaxPicSize-only in the encoder, display-rate-only in the server), which
//! disagreed exactly at the 5.1/6.0 area boundary and invented a MaxPicSize
//! for level 4.1 that Table A.3 does not have.

/// Lowest AV1 level (`seq_level_idx`) whose Table A.3 limits admit the
/// coded picture at 60 fps: MaxPicSize, MaxHSize, MaxVSize and
/// MaxDisplayRate all bind.  60 fps is the compositor's ceiling, so a level
/// picked here is conformant for every frame rate we emit.
pub fn av1_level_idx(width: u32, height: u32) -> u32 {
    let pic = width as u64 * height as u64;
    let rate = pic * 60;
    // (seq_level_idx, MaxPicSize, MaxHSize, MaxVSize, MaxDisplayRate)
    // Levels whose limits duplicate the previous row for these four fields
    // (5.3, 6.3) can never be picked and are folded into the fallthrough.
    const LEVELS: &[(u32, u64, u32, u32, u64)] = &[
        (0, 147_456, 2048, 1152, 4_423_680),          // 2.0
        (1, 278_784, 2816, 1584, 8_363_520),          // 2.1
        (4, 665_856, 4352, 2448, 19_975_680),         // 3.0
        (5, 1_065_024, 5504, 3096, 31_950_720),       // 3.1
        (8, 2_359_296, 6144, 3456, 70_778_880),       // 4.0
        (9, 2_359_296, 6144, 3456, 141_557_760),      // 4.1
        (12, 8_912_896, 8192, 4352, 267_386_880),     // 5.0
        (13, 8_912_896, 8192, 4352, 534_773_760),     // 5.1
        (14, 8_912_896, 8192, 4352, 1_069_547_520),   // 5.2
        (16, 35_651_584, 16384, 8704, 1_069_547_520), // 6.0
        (17, 35_651_584, 16384, 8704, 2_139_095_040), // 6.1
        (18, 35_651_584, 16384, 8704, 4_278_190_080), // 6.2
    ];
    for &(idx, max_pic, max_w, max_h, max_rate) in LEVELS {
        if pic <= max_pic && width <= max_w && height <= max_h && rate <= max_rate {
            return idx;
        }
    }
    19 // 6.3, the largest level Table A.3 defines
}

#[cfg(test)]
mod tests {
    use super::av1_level_idx;

    #[test]
    fn level_matches_table_a3_at_60fps() {
        // 2.0's MaxDisplayRate is its MaxPicSize x30 — at 60 fps even the
        // smallest streams start at 2.1.
        assert_eq!(av1_level_idx(426, 240), 1);
        // 1080p60 exceeds level 4.0's MaxDisplayRate — 4.1 shares its
        // MaxPicSize and doubles the rate.
        assert_eq!(av1_level_idx(1920, 1080), 9);
        // 1440p fits 5.0's display rate; the old encoder table declared
        // 4.1 here, above 4.1's real MaxPicSize.
        assert_eq!(av1_level_idx(2560, 1440), 12);
        assert_eq!(av1_level_idx(3840, 2160), 13);
        // 8K30 is 6.0 but 8K60-class rates need 6.1.
        assert_eq!(av1_level_idx(7680, 4320), 17);
        assert_eq!(av1_level_idx(8192, 4352), 17);
    }

    #[test]
    fn the_5_1_to_6_0_area_boundary_is_shared_with_the_announcement() {
        // The reported "fails under ~2200 CSS px" threshold: at 2073 px
        // tall, 4298 px wide is the last 5.1 area.  Both sides of the old
        // formula split (8,912,896 vs 9,123,840) now agree.
        assert_eq!(av1_level_idx(4298, 2073), 13);
        assert_eq!(av1_level_idx(4300, 2073), 16);
    }
}
