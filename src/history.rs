use std::sync::atomic::Ordering;

use crate::{
    chess::{
        chessmove::Move,
        piece::{Colour, Piece, PieceType},
        squareset::SquareSet,
        types::Square,
        CHESS960,
    },
    historytable::{
        cont_history_bonus, cont_history_malus, main_history_bonus, main_history_malus,
        tactical_history_bonus, tactical_history_malus, update_history, CORRECTION_HISTORY_GRAIN,
        CORRECTION_HISTORY_MAX, CORRECTION_HISTORY_WEIGHT_SCALE,
    },
    search::parameters::Config,
    threadlocal::ThreadData,
    util::MAX_PLY,
};

use crate::chess::board::{movegen::MoveListEntry, Board};

impl ThreadData<'_> {
    /// Update the history counters of a batch of moves.
    pub fn update_history(
        &mut self,
        conf: &Config,
        pos: &Board,
        moves_to_adjust: &[Move],
        best_move: Move,
        depth: i32,
    ) {
        let threats = pos.state.threats.all;
        for &m in moves_to_adjust {
            let from = m.from();
            let piece_moved = pos.state.mailbox[from];
            let to = m.history_to_square();
            let val = self.main_history.get_mut(
                piece_moved.unwrap(),
                to,
                threats.contains_square(from),
                threats.contains_square(to),
            );
            let delta = if m == best_move {
                main_history_bonus(conf, depth)
            } else {
                -main_history_malus(conf, depth)
            };
            update_history(val, delta);
        }
    }

    /// Update the history counters for a single move.
    pub fn update_history_single(
        &mut self,
        from: Square,
        to: Square,
        moved: Piece,
        threats: SquareSet,
        delta: i32,
    ) {
        let val = self.main_history.get_mut(
            moved,
            to,
            threats.contains_square(from),
            threats.contains_square(to),
        );
        update_history(val, delta);
    }

    /// Get the history scores for a batch of moves.
    pub(super) fn get_history_scores(&self, pos: &Board, ms: &mut [MoveListEntry]) {
        let threats = pos.state.threats.all;
        for m in ms {
            let from = m.mov.from();
            let piece_moved = pos.state.mailbox[from];
            let to = m.mov.history_to_square();
            m.score += i32::from(self.main_history.get(
                piece_moved.unwrap(),
                to,
                threats.contains_square(from),
                threats.contains_square(to),
            ));
        }
    }

    /// Get the history score for a single move.
    pub fn get_history_score(&self, pos: &Board, m: Move) -> i32 {
        let from = m.from();
        let piece_moved = pos.state.mailbox[from];
        let to = m.history_to_square();
        let threats = pos.state.threats.all;
        i32::from(self.main_history.get(
            piece_moved.unwrap(),
            to,
            threats.contains_square(from),
            threats.contains_square(to),
        ))
    }

    /// Update the tactical history counters of a batch of moves.
    pub fn update_tactical_history(
        &mut self,
        conf: &Config,
        pos: &Board,
        moves_to_adjust: &[Move],
        best_move: Move,
        depth: i32,
    ) {
        for &m in moves_to_adjust {
            let piece_moved = pos.state.mailbox[m.from()];
            let capture = caphist_piece_type(pos, m);
            debug_assert!(
                piece_moved.is_some(),
                "Invalid piece moved by move {} in position \n{pos:X}",
                m.display(CHESS960.load(Ordering::Relaxed))
            );
            let to = m.to();
            let val = self
                .tactical_history
                .get_mut(piece_moved.unwrap(), to, capture);
            let delta = if m == best_move {
                tactical_history_bonus(conf, depth)
            } else {
                -tactical_history_malus(conf, depth)
            };
            update_history(val, delta);
        }
    }

    /// Get the tactical history scores for a batch of moves.
    pub(super) fn get_tactical_history_scores(&self, pos: &Board, ms: &mut [MoveListEntry]) {
        for m in ms {
            let piece_moved = pos.state.mailbox[m.mov.from()];
            let capture = caphist_piece_type(pos, m.mov);
            let to = m.mov.to();
            m.score += i32::from(self.tactical_history.get(piece_moved.unwrap(), to, capture));
        }
    }

    /// Get the tactical history score for a single move.
    pub fn get_tactical_history_score(&self, pos: &Board, m: Move) -> i32 {
        let piece_moved = pos.state.mailbox[m.from()];
        let capture = caphist_piece_type(pos, m);
        let to = m.to();
        i32::from(self.tactical_history.get(piece_moved.unwrap(), to, capture))
    }

    /// Update the continuation history counters of a batch of moves.
    pub fn update_continuation_history(
        &mut self,
        conf: &Config,
        pos: &Board,
        moves_to_adjust: &[Move],
        best_move: Move,
        depth: i32,
        index: usize,
    ) {
        let height = pos.height();
        if height <= index {
            return;
        }
        let Some(ss) = self.ss.get(height - index - 1) else {
            return;
        };
        let cmh_block = self.continuation_history.get_index_mut(ss.conthist_index);
        for &m in moves_to_adjust {
            let to = m.history_to_square();
            let piece = pos.state.mailbox[m.from()].unwrap();

            let delta = if m == best_move {
                cont_history_bonus(conf, depth, index)
            } else {
                -cont_history_malus(conf, depth, index)
            };
            update_history(cmh_block.get_mut(piece, to), delta);
        }
    }

    /// Update the continuation history counter for a single move.
    pub fn update_continuation_history_single(
        &mut self,
        pos: &Board,
        to: Square,
        moved: Piece,
        delta: i32,
        index: usize,
    ) {
        let height = pos.height();
        if height <= index {
            return;
        }
        let Some(ss) = self.ss.get(height - index - 1) else {
            return;
        };
        let cmh_block = self.continuation_history.get_index_mut(ss.conthist_index);
        update_history(cmh_block.get_mut(moved, to), delta);
    }

    /// Get the continuation history scores for a batch of moves.
    pub(super) fn get_continuation_history_scores(
        &self,
        pos: &Board,
        ms: &mut [MoveListEntry],
        index: usize,
    ) {
        let height = pos.height();
        if height <= index {
            return;
        }
        let Some(ss) = self.ss.get(height - index - 1) else {
            return;
        };
        let cmh_block = self.continuation_history.get_index(ss.conthist_index);
        for m in ms {
            let to = m.mov.history_to_square();
            let piece = pos.state.mailbox[m.mov.from()].unwrap();
            m.score += i32::from(cmh_block.get(piece, to));
        }
    }

    /// Get the continuation history score for a single move.
    pub fn get_continuation_history_score(&self, pos: &Board, m: Move, index: usize) -> i32 {
        let height = pos.height();
        if height <= index {
            return 0;
        }
        let Some(ss) = self.ss.get(height - index - 1) else {
            return 0;
        };
        let cmh_block = self.continuation_history.get_index(ss.conthist_index);
        let to = m.history_to_square();
        let piece = pos.state.mailbox[m.from()].unwrap();
        i32::from(cmh_block.get(piece, to))
    }

    /// Add a killer move.
    pub fn insert_killer(&mut self, pos: &Board, m: Move) {
        debug_assert!(pos.height() < MAX_PLY);
        let idx = pos.height();
        self.killer_move_table[idx] = Some(m);
    }

    /// Update the correction history for a pawn pattern.
    pub fn update_correction_history(&mut self, pos: &Board, depth: i32, diff: i32) {
        use Colour::{Black, White};
        fn update(entry: &mut i32, new_weight: i32, scaled_diff: i32) {
            let update =
                *entry * (CORRECTION_HISTORY_WEIGHT_SCALE - new_weight) + scaled_diff * new_weight;
            *entry = i32::clamp(
                update / CORRECTION_HISTORY_WEIGHT_SCALE,
                -CORRECTION_HISTORY_MAX,
                CORRECTION_HISTORY_MAX,
            );
        }
        let scaled_diff = diff * CORRECTION_HISTORY_GRAIN;
        let new_weight = 16.min(1 + depth);
        debug_assert!(new_weight <= CORRECTION_HISTORY_WEIGHT_SCALE);
        let us = pos.turn();

        let keys = &pos.state.keys;

        update(
            self.pawn_corrhist.get_mut(us, keys.pawn),
            new_weight,
            scaled_diff,
        );
        update(
            self.nonpawn_corrhist[White].get_mut(us, keys.non_pawn[White]),
            new_weight,
            scaled_diff,
        );
        update(
            self.nonpawn_corrhist[Black].get_mut(us, keys.non_pawn[Black]),
            new_weight,
            scaled_diff,
        );
        update(
            self.minor_corrhist.get_mut(us, keys.minor),
            new_weight,
            scaled_diff,
        );
        update(
            self.major_corrhist.get_mut(us, keys.major),
            new_weight,
            scaled_diff,
        );
    }

    /// Adjust a raw evaluation using statistics from the correction history.
    #[allow(clippy::cast_possible_truncation)]
    pub fn correction(&self, conf: &Config, pos: &Board) -> i32 {
        let keys = &pos.state.keys;
        let pawn = self.pawn_corrhist.get(pos.turn(), keys.pawn);
        let white =
            self.nonpawn_corrhist[Colour::White].get(pos.turn(), keys.non_pawn[Colour::White]);
        let black =
            self.nonpawn_corrhist[Colour::Black].get(pos.turn(), keys.non_pawn[Colour::Black]);
        let minor = self.minor_corrhist.get(pos.turn(), keys.minor);
        let major = self.major_corrhist.get(pos.turn(), keys.major);
        let adjustment = pawn * i64::from(conf.pawn_corrhist_weight)
            + major * i64::from(conf.major_corrhist_weight)
            + minor * i64::from(conf.minor_corrhist_weight)
            + (white + black) * i64::from(conf.nonpawn_corrhist_weight);
        (adjustment / 1024) as i32 / CORRECTION_HISTORY_GRAIN
    }
}

pub fn caphist_piece_type(pos: &Board, mv: Move) -> PieceType {
    if mv.is_ep() || mv.is_promo() {
        // it's fine to make all promos of type PAWN,
        // because you'd never usually capture pawns on
        // the back ranks, so these slots are free in
        // the capture history table.
        PieceType::Pawn
    } else {
        debug_assert!(!mv.is_castle(), "shouldn't be using caphist for castling.");
        pos.state.mailbox[mv.to()]
            .expect("you weren't capturing anything!")
            .piece_type()
    }
}
