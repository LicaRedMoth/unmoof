//! Lays out what was fed and checks the plan adds up: every rendered piece has the planned
//! length, and the pieces sum to the planned size. A clean `Err` anywhere is fine.

use unmoof::{Piece, Remuxer};

pub fn finish_and_check(rm: Remuxer, mp3: bool, files: &[&[u8]]) {
    let Ok(plan) = (if mp3 { rm.finish_mp3() } else { rm.finish() }) else { return };
    let mut total = plan.header.len() as u64;
    for (i, p) in plan.pieces.iter().enumerate() {
        total += p.len();
        if let Piece::Render { file, len, .. } = p {
            let Some(data) = files.get(*file as usize) else { panic!("a piece names a file never fed") };
            if let Ok(bytes) = plan.render(i, data) {
                assert_eq!(bytes.len() as u64, *len, "a rendered piece is not the length planned");
            }
        }
    }
    assert_eq!(total, plan.size, "the pieces do not add up to the planned size");
}
