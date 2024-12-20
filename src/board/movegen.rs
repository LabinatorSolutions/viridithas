pub mod movepicker;
pub mod piecelayout;

use arrayvec::ArrayVec;

use self::movepicker::{MainSearch, MovePickerMode};
pub use self::piecelayout::SquareIter;

use super::Board;

use std::{
    fmt::{Display, Formatter},
    ops::{Deref, DerefMut},
    sync::atomic::Ordering,
};

use crate::{
    chessmove::{Move, MoveFlags},
    lookups, magic,
    piece::{Black, Col, Colour, PieceType, White},
    squareset::SquareSet,
    uci::CHESS960,
    util::{Square, RAY_BETWEEN},
};

pub const MAX_POSITION_MOVES: usize = 218;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveListEntry {
    pub mov: Move,
    pub score: i32,
}

impl MoveListEntry {
    pub const TACTICAL_SENTINEL: i32 = 0x7FFF_FFFF;
    pub const QUIET_SENTINEL: i32 = 0x7FFF_FFFE;
}

#[derive(Clone)]
pub struct MoveList {
    // moves: [MoveListEntry; MAX_POSITION_MOVES],
    // count: usize,
    inner: ArrayVec<MoveListEntry, MAX_POSITION_MOVES>,
}

impl MoveList {
    pub fn new() -> Self {
        Self {
            inner: ArrayVec::new(),
        }
    }

    fn push<const TACTICAL: bool>(&mut self, m: Move) {
        // debug_assert!(self.count < MAX_POSITION_MOVES, "overflowed {self}");
        let score = if TACTICAL {
            MoveListEntry::TACTICAL_SENTINEL
        } else {
            MoveListEntry::QUIET_SENTINEL
        };

        self.inner.push(MoveListEntry { mov: m, score });
    }

    pub fn iter_moves(&self) -> impl Iterator<Item = &Move> {
        self.inner.iter().map(|e| &e.mov)
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl Deref for MoveList {
    type Target = [MoveListEntry];

    fn deref(&self) -> &[MoveListEntry] {
        &self.inner
    }
}

impl DerefMut for MoveList {
    fn deref_mut(&mut self) -> &mut [MoveListEntry] {
        &mut self.inner
    }
}

impl Display for MoveList {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        if self.inner.is_empty() {
            return write!(f, "MoveList: (0) []");
        }
        writeln!(f, "MoveList: ({}) [", self.inner.len())?;
        for m in &self.inner[0..self.inner.len() - 1] {
            writeln!(f, "  {} ${}, ", m.mov, m.score)?;
        }
        writeln!(
            f,
            "  {} ${}",
            self.inner[self.inner.len() - 1].mov,
            self.inner[self.inner.len() - 1].score
        )?;
        write!(f, "]")
    }
}

pub fn bishop_attacks(sq: Square, blockers: SquareSet) -> SquareSet {
    magic::get_diagonal_attacks(sq, blockers)
}
pub fn rook_attacks(sq: Square, blockers: SquareSet) -> SquareSet {
    magic::get_orthogonal_attacks(sq, blockers)
}
// pub fn queen_attacks(sq: Square, blockers: SquareSet) -> SquareSet {
//     magic::get_diagonal_attacks(sq, blockers) | magic::get_orthogonal_attacks(sq, blockers)
// }
pub fn knight_attacks(sq: Square) -> SquareSet {
    lookups::get_knight_attacks(sq)
}
pub fn king_attacks(sq: Square) -> SquareSet {
    lookups::get_king_attacks(sq)
}
pub fn pawn_attacks<C: Col>(bb: SquareSet) -> SquareSet {
    if C::WHITE {
        bb.north_east_one() | bb.north_west_one()
    } else {
        bb.south_east_one() | bb.south_west_one()
    }
}

pub fn attacks_by_type(pt: PieceType, sq: Square, blockers: SquareSet) -> SquareSet {
    match pt {
        PieceType::Bishop => magic::get_diagonal_attacks(sq, blockers),
        PieceType::Rook => magic::get_orthogonal_attacks(sq, blockers),
        PieceType::Queen => {
            magic::get_diagonal_attacks(sq, blockers) | magic::get_orthogonal_attacks(sq, blockers)
        }
        PieceType::Knight => lookups::get_knight_attacks(sq),
        PieceType::King => lookups::get_king_attacks(sq),
        PieceType::Pawn => panic!("Invalid piece type: {pt:?}"),
    }
}

impl Board {
    fn generate_pawn_caps<C: Col, Mode: MovePickerMode>(
        &self,
        move_list: &mut MoveList,
        valid_target_squares: SquareSet,
    ) {
        let our_pawns = self.pieces.pawns::<C>();
        let their_pieces = self.pieces.their_pieces::<C>();
        // to determine which pawns can capture, we shift the opponent's pieces backwards and find the intersection
        let attacking_west = if C::WHITE {
            their_pieces.south_east_one() & our_pawns
        } else {
            their_pieces.north_east_one() & our_pawns
        };
        let attacking_east = if C::WHITE {
            their_pieces.south_west_one() & our_pawns
        } else {
            their_pieces.north_west_one() & our_pawns
        };
        let valid_west = if C::WHITE {
            valid_target_squares.south_east_one()
        } else {
            valid_target_squares.north_east_one()
        };
        let valid_east = if C::WHITE {
            valid_target_squares.south_west_one()
        } else {
            valid_target_squares.north_west_one()
        };
        let promo_rank = if C::WHITE {
            SquareSet::RANK_7
        } else {
            SquareSet::RANK_2
        };
        let from_mask = attacking_west & !promo_rank & valid_west;
        let to_mask = if C::WHITE {
            from_mask.north_west_one()
        } else {
            from_mask.south_west_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            move_list.push::<true>(Move::new(from, to));
        }
        let from_mask = attacking_east & !promo_rank & valid_east;
        let to_mask = if C::WHITE {
            from_mask.north_east_one()
        } else {
            from_mask.south_east_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            move_list.push::<true>(Move::new(from, to));
        }
        let from_mask = attacking_west & promo_rank & valid_west;
        let to_mask = if C::WHITE {
            from_mask.north_west_one()
        } else {
            from_mask.south_west_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            // in quiescence search, we only generate promotions to queen.
            if Mode::CAPTURES_ONLY {
                move_list.push::<true>(Move::new_with_promo(from, to, PieceType::Queen));
            } else {
                for promo in [
                    PieceType::Queen,
                    PieceType::Rook,
                    PieceType::Bishop,
                    PieceType::Knight,
                ] {
                    move_list.push::<true>(Move::new_with_promo(from, to, promo));
                }
            }
        }
        let from_mask = attacking_east & promo_rank & valid_east;
        let to_mask = if C::WHITE {
            from_mask.north_east_one()
        } else {
            from_mask.south_east_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            // in quiescence search, we only generate promotions to queen.
            if Mode::CAPTURES_ONLY {
                move_list.push::<true>(Move::new_with_promo(from, to, PieceType::Queen));
            } else {
                for promo in [
                    PieceType::Queen,
                    PieceType::Rook,
                    PieceType::Bishop,
                    PieceType::Knight,
                ] {
                    move_list.push::<true>(Move::new_with_promo(from, to, promo));
                }
            }
        }
    }

    fn generate_ep<C: Col>(&self, move_list: &mut MoveList) {
        let Some(ep_sq) = self.ep_sq else {
            return;
        };
        let ep_bb = ep_sq.as_set();
        let our_pawns = self.pieces.pawns::<C>();
        let attacks_west = if C::WHITE {
            ep_bb.south_east_one() & our_pawns
        } else {
            ep_bb.north_east_one() & our_pawns
        };
        let attacks_east = if C::WHITE {
            ep_bb.south_west_one() & our_pawns
        } else {
            ep_bb.north_west_one() & our_pawns
        };

        if attacks_west.non_empty() {
            let from_sq = attacks_west.first();
            move_list.push::<true>(Move::new_with_flags(from_sq, ep_sq, MoveFlags::EnPassant));
        }
        if attacks_east.non_empty() {
            let from_sq = attacks_east.first();
            move_list.push::<true>(Move::new_with_flags(from_sq, ep_sq, MoveFlags::EnPassant));
        }
    }

    fn generate_pawn_forward<C: Col>(
        &self,
        move_list: &mut MoveList,
        valid_target_squares: SquareSet,
    ) {
        let start_rank = if C::WHITE {
            SquareSet::RANK_2
        } else {
            SquareSet::RANK_7
        };
        let promo_rank = if C::WHITE {
            SquareSet::RANK_7
        } else {
            SquareSet::RANK_2
        };
        let shifted_empty_squares = if C::WHITE {
            self.pieces.empty() >> 8
        } else {
            self.pieces.empty() << 8
        };
        let double_shifted_empty_squares = if C::WHITE {
            self.pieces.empty() >> 16
        } else {
            self.pieces.empty() << 16
        };
        let shifted_valid_squares = if C::WHITE {
            valid_target_squares >> 8
        } else {
            valid_target_squares << 8
        };
        let double_shifted_valid_squares = if C::WHITE {
            valid_target_squares >> 16
        } else {
            valid_target_squares << 16
        };
        let our_pawns = self.pieces.pawns::<C>();
        let pushable_pawns = our_pawns & shifted_empty_squares;
        let double_pushable_pawns = pushable_pawns & double_shifted_empty_squares & start_rank;
        let promoting_pawns = pushable_pawns & promo_rank;

        let from_mask = pushable_pawns & !promoting_pawns & shifted_valid_squares;
        let to_mask = if C::WHITE {
            from_mask.north_one()
        } else {
            from_mask.south_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            move_list.push::<false>(Move::new(from, to));
        }
        let from_mask = double_pushable_pawns & double_shifted_valid_squares;
        let to_mask = if C::WHITE {
            from_mask.north_one().north_one()
        } else {
            from_mask.south_one().south_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            move_list.push::<false>(Move::new(from, to));
        }
        let from_mask = promoting_pawns & shifted_valid_squares;
        let to_mask = if C::WHITE {
            from_mask.north_one()
        } else {
            from_mask.south_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            for promo in [
                PieceType::Queen,
                PieceType::Knight,
                PieceType::Rook,
                PieceType::Bishop,
            ] {
                move_list.push::<true>(Move::new_with_promo(from, to, promo));
            }
        }
    }

    fn generate_forward_promos<C: Col, Mode: MovePickerMode>(
        &self,
        move_list: &mut MoveList,
        valid_target_squares: SquareSet,
    ) {
        let promo_rank = if C::WHITE {
            SquareSet::RANK_7
        } else {
            SquareSet::RANK_2
        };
        let shifted_empty_squares = if C::WHITE {
            self.pieces.empty() >> 8
        } else {
            self.pieces.empty() << 8
        };
        let shifted_valid_squares = if C::WHITE {
            valid_target_squares >> 8
        } else {
            valid_target_squares << 8
        };
        let our_pawns = self.pieces.pawns::<C>();
        let pushable_pawns = our_pawns & shifted_empty_squares;
        let promoting_pawns = pushable_pawns & promo_rank;

        let from_mask = promoting_pawns & shifted_valid_squares;
        let to_mask = if C::WHITE {
            from_mask.north_one()
        } else {
            from_mask.south_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            if Mode::CAPTURES_ONLY {
                // in quiescence search, we only generate promotions to queen.
                move_list.push::<true>(Move::new_with_promo(from, to, PieceType::Queen));
            } else {
                for promo in [
                    PieceType::Queen,
                    PieceType::Knight,
                    PieceType::Rook,
                    PieceType::Bishop,
                ] {
                    move_list.push::<true>(Move::new_with_promo(from, to, promo));
                }
            }
        }
    }

    pub fn generate_moves(&self, move_list: &mut MoveList) {
        move_list.clear();
        if self.side == Colour::White {
            self.generate_moves_for::<White>(move_list);
        } else {
            self.generate_moves_for::<Black>(move_list);
        }
        debug_assert!(move_list.iter_moves().all(|m| m.is_valid()));
    }

    fn generate_moves_for<C: Col>(&self, move_list: &mut MoveList) {
        #[cfg(debug_assertions)]
        self.check_validity().unwrap();

        let their_pieces = self.pieces.their_pieces::<C>();
        let freespace = self.pieces.empty();
        let our_king_sq = self.pieces.king::<C>().first();

        if self.threats.checkers.count() > 1 {
            // we're in double-check, so we can only move the king.
            let moves = king_attacks(our_king_sq) & !self.threats.all;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(our_king_sq, to));
            }
            for to in moves & freespace {
                move_list.push::<false>(Move::new(our_king_sq, to));
            }
            return;
        }

        let valid_target_squares = if self.in_check() {
            RAY_BETWEEN[our_king_sq][self.threats.checkers.first()] | self.threats.checkers
        } else {
            SquareSet::FULL
        };

        self.generate_pawn_forward::<C>(move_list, valid_target_squares);
        self.generate_pawn_caps::<C, MainSearch>(move_list, valid_target_squares);
        self.generate_ep::<C>(move_list);

        // knights
        let our_knights = self.pieces.knights::<C>();
        for sq in our_knights {
            let moves = knight_attacks(sq) & valid_target_squares;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(sq, to));
            }
            for to in moves & freespace {
                move_list.push::<false>(Move::new(sq, to));
            }
        }

        // kings
        let moves = king_attacks(our_king_sq) & !self.threats.all;
        for to in moves & their_pieces {
            move_list.push::<true>(Move::new(our_king_sq, to));
        }
        for to in moves & freespace {
            move_list.push::<false>(Move::new(our_king_sq, to));
        }

        // bishops and queens
        let our_diagonal_sliders = self.pieces.diags::<C>();
        let blockers = self.pieces.occupied();
        for sq in our_diagonal_sliders {
            let moves = bishop_attacks(sq, blockers) & valid_target_squares;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(sq, to));
            }
            for to in moves & freespace {
                move_list.push::<false>(Move::new(sq, to));
            }
        }

        // rooks and queens
        let our_orthogonal_sliders = self.pieces.orthos::<C>();
        for sq in our_orthogonal_sliders {
            let moves = rook_attacks(sq, blockers) & valid_target_squares;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(sq, to));
            }
            for to in moves & freespace {
                move_list.push::<false>(Move::new(sq, to));
            }
        }

        if !self.in_check() {
            self.generate_castling_moves_for::<C>(move_list);
        }
    }

    pub fn generate_captures<Mode: MovePickerMode>(&self, move_list: &mut MoveList) {
        move_list.clear();
        if self.side == Colour::White {
            self.generate_captures_for::<White, Mode>(move_list);
        } else {
            self.generate_captures_for::<Black, Mode>(move_list);
        }
        debug_assert!(move_list.iter_moves().all(|m| m.is_valid()));
    }

    fn generate_captures_for<C: Col, Mode: MovePickerMode>(&self, move_list: &mut MoveList) {
        #[cfg(debug_assertions)]
        self.check_validity().unwrap();

        let their_pieces = self.pieces.their_pieces::<C>();
        let our_king_sq = self.pieces.king::<C>().first();

        if self.threats.checkers.count() > 1 {
            // we're in double-check, so we can only move the king.
            let moves = king_attacks(our_king_sq) & !self.threats.all;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(our_king_sq, to));
            }
            return;
        }

        let valid_target_squares = if self.in_check() {
            RAY_BETWEEN[our_king_sq][self.threats.checkers.first()] | self.threats.checkers
        } else {
            SquareSet::FULL
        };

        // promotions
        self.generate_forward_promos::<C, Mode>(move_list, valid_target_squares);

        // pawn captures and capture promos
        self.generate_pawn_caps::<C, Mode>(move_list, valid_target_squares);
        self.generate_ep::<C>(move_list);

        // knights
        let our_knights = self.pieces.knights::<C>();
        let their_pieces = self.pieces.their_pieces::<C>();
        for sq in our_knights {
            let moves = knight_attacks(sq) & valid_target_squares;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(sq, to));
            }
        }

        // kings
        let moves = king_attacks(our_king_sq) & !self.threats.all;
        for to in moves & their_pieces {
            move_list.push::<true>(Move::new(our_king_sq, to));
        }

        // bishops and queens
        let our_diagonal_sliders = self.pieces.diags::<C>();
        let blockers = self.pieces.occupied();
        for sq in our_diagonal_sliders {
            let moves = bishop_attacks(sq, blockers) & valid_target_squares;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(sq, to));
            }
        }

        // rooks and queens
        let our_orthogonal_sliders = self.pieces.orthos::<C>();
        for sq in our_orthogonal_sliders {
            let moves = rook_attacks(sq, blockers) & valid_target_squares;
            for to in moves & their_pieces {
                move_list.push::<true>(Move::new(sq, to));
            }
        }
    }

    fn generate_castling_moves_for<C: Col>(&self, move_list: &mut MoveList) {
        let occupied = self.pieces.occupied();

        if CHESS960.load(Ordering::Relaxed) {
            let king_sq = self.king_sq(C::COLOUR);
            if self.sq_attacked(king_sq, C::Opposite::COLOUR) {
                return;
            }

            let castling_kingside = self.castle_perm.kingside(C::COLOUR);
            if let Some(castling_kingside) = castling_kingside {
                let king_dst = Square::G1.relative_to(C::COLOUR);
                let rook_dst = Square::F1.relative_to(C::COLOUR);
                self.try_generate_frc_castling::<C>(
                    king_sq,
                    castling_kingside,
                    king_dst,
                    rook_dst,
                    occupied,
                    move_list,
                );
            }

            let castling_queenside = self.castle_perm.queenside(C::COLOUR);
            if let Some(castling_queenside) = castling_queenside {
                let king_dst = Square::C1.relative_to(C::COLOUR);
                let rook_dst = Square::D1.relative_to(C::COLOUR);
                self.try_generate_frc_castling::<C>(
                    king_sq,
                    castling_queenside,
                    king_dst,
                    rook_dst,
                    occupied,
                    move_list,
                );
            }
        } else {
            const WK_FREESPACE: SquareSet = Square::F1.as_set().add_square(Square::G1);
            const WQ_FREESPACE: SquareSet = Square::B1
                .as_set()
                .add_square(Square::C1)
                .add_square(Square::D1);
            const BK_FREESPACE: SquareSet = Square::F8.as_set().add_square(Square::G8);
            const BQ_FREESPACE: SquareSet = Square::B8
                .as_set()
                .add_square(Square::C8)
                .add_square(Square::D8);

            let k_freespace = if C::WHITE { WK_FREESPACE } else { BK_FREESPACE };
            let q_freespace = if C::WHITE { WQ_FREESPACE } else { BQ_FREESPACE };
            let from = Square::E1.relative_to(C::COLOUR);
            let k_to = Square::H1.relative_to(C::COLOUR);
            let q_to = Square::A1.relative_to(C::COLOUR);
            let k_thru = Square::F1.relative_to(C::COLOUR);
            let q_thru = Square::D1.relative_to(C::COLOUR);
            let k_perm = self.castle_perm.kingside(C::COLOUR);
            let q_perm = self.castle_perm.queenside(C::COLOUR);

            // stupid hack to avoid redoing or eagerly doing hard work.
            let mut cache = None;

            if k_perm.is_some()
                && (occupied & k_freespace).is_empty()
                && {
                    let got_attacked_king = self.sq_attacked_by::<C::Opposite>(from);
                    cache = Some(got_attacked_king);
                    !got_attacked_king
                }
                && !self.sq_attacked_by::<C::Opposite>(k_thru)
            {
                move_list.push::<false>(Move::new_with_flags(from, k_to, MoveFlags::Castle));
            }

            if q_perm.is_some()
                && (occupied & q_freespace).is_empty()
                && !cache.unwrap_or_else(|| self.sq_attacked_by::<C::Opposite>(from))
                && !self.sq_attacked_by::<C::Opposite>(q_thru)
            {
                move_list.push::<false>(Move::new_with_flags(from, q_to, MoveFlags::Castle));
            }
        }
    }

    fn try_generate_frc_castling<C: Col>(
        &self,
        king_sq: Square,
        castling_sq: Square,
        king_dst: Square,
        rook_dst: Square,
        occupied: SquareSet,
        move_list: &mut MoveList,
    ) {
        let king_path = RAY_BETWEEN[king_sq][king_dst];
        let rook_path = RAY_BETWEEN[king_sq][castling_sq];
        let relevant_occupied = occupied ^ king_sq.as_set() ^ castling_sq.as_set();
        if (relevant_occupied & (king_path | rook_path | king_dst.as_set() | rook_dst.as_set()))
            .is_empty()
            && !self.any_attacked(king_path, C::Opposite::COLOUR)
        {
            move_list.push::<false>(Move::new_with_flags(
                king_sq,
                castling_sq,
                MoveFlags::Castle,
            ));
        }
    }

    pub fn generate_quiets(&self, move_list: &mut MoveList) {
        // we don't need to clear the move list here because we're only adding to it.
        if self.side == Colour::White {
            self.generate_quiets_for::<White>(move_list);
        } else {
            self.generate_quiets_for::<Black>(move_list);
        }
        debug_assert!(move_list.iter_moves().all(|m| m.is_valid()));
    }

    fn generate_pawn_quiet<C: Col>(
        &self,
        move_list: &mut MoveList,
        valid_target_squares: SquareSet,
    ) {
        let start_rank = if C::WHITE {
            SquareSet::RANK_2
        } else {
            SquareSet::RANK_7
        };
        let promo_rank = if C::WHITE {
            SquareSet::RANK_7
        } else {
            SquareSet::RANK_2
        };
        let shifted_empty_squares = if C::WHITE {
            self.pieces.empty() >> 8
        } else {
            self.pieces.empty() << 8
        };
        let double_shifted_empty_squares = if C::WHITE {
            self.pieces.empty() >> 16
        } else {
            self.pieces.empty() << 16
        };
        let shifted_valid_squares = if C::WHITE {
            valid_target_squares >> 8
        } else {
            valid_target_squares << 8
        };
        let double_shifted_valid_squares = if C::WHITE {
            valid_target_squares >> 16
        } else {
            valid_target_squares << 16
        };
        let our_pawns = self.pieces.pawns::<C>();
        let pushable_pawns = our_pawns & shifted_empty_squares;
        let double_pushable_pawns = pushable_pawns & double_shifted_empty_squares & start_rank;
        let promoting_pawns = pushable_pawns & promo_rank;

        let from_mask = pushable_pawns & !promoting_pawns & shifted_valid_squares;
        let to_mask = if C::WHITE {
            from_mask.north_one()
        } else {
            from_mask.south_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            move_list.push::<false>(Move::new(from, to));
        }
        let from_mask = double_pushable_pawns & double_shifted_valid_squares;
        let to_mask = if C::WHITE {
            from_mask.north_one().north_one()
        } else {
            from_mask.south_one().south_one()
        };
        for (from, to) in from_mask.into_iter().zip(to_mask) {
            move_list.push::<false>(Move::new(from, to));
        }
    }

    fn generate_quiets_for<C: Col>(&self, move_list: &mut MoveList) {
        let freespace = self.pieces.empty();
        let our_king_sq = self.pieces.king::<C>().first();
        let blockers = self.pieces.occupied();

        if self.threats.checkers.count() > 1 {
            // we're in double-check, so we can only move the king.
            let moves = king_attacks(our_king_sq) & !self.threats.all;
            for to in moves & freespace {
                move_list.push::<false>(Move::new(our_king_sq, to));
            }
            return;
        }

        let valid_target_squares = if self.in_check() {
            RAY_BETWEEN[our_king_sq][self.threats.checkers.first()] | self.threats.checkers
        } else {
            SquareSet::FULL
        };

        // pawns
        self.generate_pawn_quiet::<C>(move_list, valid_target_squares);

        // knights
        let our_knights = self.pieces.knights::<C>();
        for sq in our_knights {
            let moves = knight_attacks(sq) & valid_target_squares;
            for to in moves & !blockers {
                move_list.push::<false>(Move::new(sq, to));
            }
        }

        // kings
        let moves = king_attacks(our_king_sq) & !self.threats.all;
        for to in moves & !blockers {
            move_list.push::<false>(Move::new(our_king_sq, to));
        }

        // bishops and queens
        let our_diagonal_sliders = self.pieces.diags::<C>();
        for sq in our_diagonal_sliders {
            let moves = bishop_attacks(sq, blockers) & valid_target_squares;
            for to in moves & !blockers {
                move_list.push::<false>(Move::new(sq, to));
            }
        }

        // rooks and queens
        let our_orthogonal_sliders = self.pieces.orthos::<C>();
        for sq in our_orthogonal_sliders {
            let moves = rook_attacks(sq, blockers) & valid_target_squares;
            for to in moves & !blockers {
                move_list.push::<false>(Move::new(sq, to));
            }
        }

        // castling
        if !self.in_check() {
            self.generate_castling_moves_for::<C>(move_list);
        }
    }
}

#[cfg(test)]
pub fn synced_perft(pos: &mut Board, depth: usize) -> u64 {
    #![allow(clippy::to_string_in_format_args)]
    #[cfg(debug_assertions)]
    pos.check_validity().unwrap();

    if depth == 0 {
        return 1;
    }

    let mut ml = MoveList::new();
    pos.generate_moves(&mut ml);
    let mut ml_staged = MoveList::new();
    pos.generate_captures::<MainSearch>(&mut ml_staged);
    pos.generate_quiets(&mut ml_staged);

    let mut full_moves_vec = ml.to_vec();
    let mut staged_moves_vec = ml_staged.to_vec();
    full_moves_vec.sort_unstable_by_key(|m| m.mov);
    staged_moves_vec.sort_unstable_by_key(|m| m.mov);
    let eq = full_moves_vec == staged_moves_vec;
    assert!(
        eq,
        "full and staged move lists differ in {}, \nfull list: \n[{}], \nstaged list: \n[{}]",
        pos.to_string(),
        {
            let mut mvs = Vec::new();
            for m in full_moves_vec {
                mvs.push(format!(
                    "{}{}",
                    pos.san(m.mov).unwrap(),
                    if m.score == MoveListEntry::TACTICAL_SENTINEL {
                        "T"
                    } else {
                        "Q"
                    }
                ));
            }
            mvs.join(", ")
        },
        {
            let mut mvs = Vec::new();
            for m in staged_moves_vec {
                mvs.push(format!(
                    "{}{}",
                    pos.san(m.mov).unwrap(),
                    if m.score == MoveListEntry::TACTICAL_SENTINEL {
                        "T"
                    } else {
                        "Q"
                    }
                ));
            }
            mvs.join(", ")
        }
    );

    let mut count = 0;
    for &m in ml.iter_moves() {
        if !pos.make_move_simple(m) {
            continue;
        }
        count += synced_perft(pos, depth - 1);
        pos.unmake_move_base();
    }

    count
}

mod tests {
    #[test]
    fn staged_matches_full() {
        use super::*;
        use crate::bench;

        let mut pos = Board::default();

        for fen in bench::BENCH_POSITIONS {
            pos.set_from_fen(fen).unwrap();
            synced_perft(&mut pos, 2);
        }
    }
}
