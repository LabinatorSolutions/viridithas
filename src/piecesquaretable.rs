pub mod sftables;

use crate::{board::evaluation::S, definitions::Square::A1, lookups::piece_name};

pub fn pst_value(piece: u8, sq: u8, pst: &[[S; 64]; 13]) -> S {
    debug_assert!(crate::validate::piece_valid(piece));
    debug_assert!(crate::validate::square_on_board(sq));
    unsafe { *pst.get_unchecked(piece as usize).get_unchecked(sq as usize) }
}

pub fn _render_pst_table(pst: &[[i32; 64]; 13]) {
    #![allow(clippy::needless_range_loop, clippy::cast_possible_truncation)]
    for piece in 0..13 {
        println!("{}", piece_name(piece as u8).unwrap());
        println!("eval on a1 (bottom left) {}", pst[piece][A1 as usize]);
        for row in (0..8).rev() {
            for col in 0..8 {
                let sq = row * 8 + col;
                let pst_val = pst[piece][sq];
                print!("{:>5}", pst_val);
            }
            println!();
        }
    }
}

mod tests {
    #[test]
    fn psts_are_mirrored_properly() {
        #![allow(clippy::similar_names, clippy::cast_possible_truncation)]
        use super::*;
        use crate::definitions::square_name;
        let psts = super::sftables::construct_sf_pst();
        for white_piece in 1..7 {
            let white_pst = &psts[white_piece];
            let black_pst = &psts[white_piece + 6];
            for sq in 0..64 {
                assert_eq!(
                    white_pst[sq as usize],
                    -black_pst[crate::definitions::flip_rank(sq) as usize],
                    "pst mirroring failed on square {} for piece {}",
                    square_name(sq as u8).unwrap(),
                    piece_name(white_piece as u8).unwrap()
                );
            }
        }
    }
}
