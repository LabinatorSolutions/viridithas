pub mod evaluation;
mod history;
pub mod movegen;
pub mod validation;

use std::{
    fmt::{self, Debug, Display, Formatter, Write},
    sync::atomic::Ordering,
};

use anyhow::{bail, Context};

#[cfg(feature = "datagen")]
use rand::{prelude::SliceRandom, rngs::ThreadRng};

use crate::{
    board::movegen::{bishop_attacks, king_attacks, knight_attacks, pawn_attacks, rook_attacks, MoveList},
    chessmove::Move,
    cuckoo,
    historytable::ContHistIndex,
    makemove::{hash_castling, hash_ep, hash_piece, hash_side},
    nnue::network::{FeatureUpdate, MovedPiece, UpdateBuffer},
    piece::{Black, Col, Colour, Piece, PieceType, White},
    search::pv::PVariation,
    squareset::SquareSet,
    threadlocal::ThreadData,
    uci::CHESS960,
    util::{CastlingRights, CheckState, File, Rank, Square, Undo, RAY_BETWEEN},
};

use self::movegen::piecelayout::{PieceLayout, Threats};

#[derive(Clone, PartialEq, Eq)]
pub struct Board {
    /// The square-sets of all the pieces on the board.
    pub(crate) pieces: PieceLayout,
    /// An array to accelerate `Board::piece_at()`.
    piece_array: [Option<Piece>; 64],
    /// The side to move.
    side: Colour,
    /// The en passant square.
    ep_sq: Option<Square>,
    /// A mask of the rooks that can castle.
    castle_perm: CastlingRights,
    /// The number of half moves made since the last capture or pawn advance.
    fifty_move_counter: u8,
    /// The number of half moves made since the start of the game.
    ply: usize,

    /// The Zobrist hash of the board.
    key: u64,
    /// The Zobrist hash of the pawns on the board.
    pawn_key: u64,
    /// The Zobrist hash of the non-pawns on the board, split by side.
    non_pawn_key: [u64; 2],
    /// The Zobrist hash of the minor pieces on the board.
    minor_key: u64,
    /// The Zobrist hash of the major pieces on the board.
    major_key: u64,

    /// Squares that the opponent attacks
    threats: Threats,

    height: usize,
    history: Vec<Undo>,
}

impl Debug for Board {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Board")
            .field("piece_array", &self.piece_array)
            .field("side", &self.side)
            .field("ep_sq", &self.ep_sq)
            .field("fifty_move_counter", &self.fifty_move_counter)
            .field("height", &self.height)
            .field("ply", &self.ply)
            .field("key", &self.key)
            .field("threats", &self.threats)
            .field("castle_perm", &self.castle_perm)
            .finish_non_exhaustive()
    }
}

impl Board {
    pub const STARTING_FEN: &'static str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    pub const STARTING_FEN_960: &'static str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w AHah - 0 1";

    pub fn new() -> Self {
        let mut out = Self {
            pieces: PieceLayout::NULL,
            piece_array: [None; 64],
            side: Colour::White,
            ep_sq: None,
            fifty_move_counter: 0,
            height: 0,
            ply: 0,
            key: 0,
            pawn_key: 0,
            non_pawn_key: [0; 2],
            minor_key: 0,
            major_key: 0,
            threats: Threats::default(),
            castle_perm: CastlingRights::NONE,
            history: Vec::new(),
        };
        out.reset();
        out
    }

    pub const fn ep_sq(&self) -> Option<Square> {
        self.ep_sq
    }

    #[cfg(feature = "datagen")]
    pub fn ep_sq_mut(&mut self) -> &mut Option<Square> {
        &mut self.ep_sq
    }

    #[cfg(feature = "datagen")]
    pub fn turn_mut(&mut self) -> &mut Colour {
        &mut self.side
    }

    #[cfg(feature = "datagen")]
    pub fn halfmove_clock_mut(&mut self) -> &mut u8 {
        &mut self.fifty_move_counter
    }

    #[cfg(feature = "datagen")]
    pub fn set_fullmove_clock(&mut self, fullmove_clock: u16) {
        self.ply = (fullmove_clock as usize - 1) * 2 + usize::from(self.side == Colour::Black);
    }

    pub const fn zobrist_key(&self) -> u64 {
        self.key
    }

    pub const fn pawn_key(&self) -> u64 {
        self.pawn_key
    }

    pub fn non_pawn_key(&self, colour: Colour) -> u64 {
        self.non_pawn_key[colour]
    }

    pub const fn minor_key(&self) -> u64 {
        self.minor_key
    }

    pub const fn major_key(&self) -> u64 {
        self.major_key
    }

    #[cfg(debug_assertions)]
    pub const fn all_keys(&self) -> (u64, u64, [u64; 2], u64, u64) {
        (self.key, self.pawn_key, self.non_pawn_key, self.minor_key, self.major_key)
    }

    pub fn n_men(&self) -> u8 {
        #![allow(clippy::cast_possible_truncation)]
        self.pieces.occupied().count() as u8
    }

    pub const fn ply(&self) -> usize {
        self.ply
    }

    pub const fn threats(&self) -> &Threats {
        &self.threats
    }

    pub fn king_sq(&self, side: Colour) -> Square {
        debug_assert!(side == Colour::White || side == Colour::Black);
        debug_assert_eq!(self.pieces.king::<White>().count(), 1);
        debug_assert_eq!(self.pieces.king::<Black>().count(), 1);
        let sq = match side {
            Colour::White => self.pieces.king::<White>().first(),
            Colour::Black => self.pieces.king::<Black>().first(),
        };
        debug_assert_eq!(self.pieces.piece_at(sq).unwrap().colour(), side);
        debug_assert_eq!(self.pieces.piece_at(sq).unwrap().piece_type(), PieceType::King);
        sq
    }

    pub const fn in_check(&self) -> bool {
        self.threats.checkers.non_empty()
    }

    pub fn zero_height(&mut self) {
        self.height = 0;
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    pub const fn turn(&self) -> Colour {
        self.side
    }

    pub const fn castling_rights(&self) -> CastlingRights {
        self.castle_perm
    }

    #[cfg(feature = "datagen")]
    pub fn castling_rights_mut(&mut self) -> &mut CastlingRights {
        &mut self.castle_perm
    }

    pub fn generate_pos_keys(&self) -> (u64, u64, [u64; 2], u64, u64) {
        let mut key = 0;
        let mut pawn_key = 0;
        let mut non_pawn_key = [0; 2];
        let mut minor_key = 0;
        let mut major_key = 0;
        self.pieces.visit_pieces(|sq, piece| {
            hash_piece(&mut key, piece, sq);
            if piece.piece_type() == PieceType::Pawn {
                hash_piece(&mut pawn_key, piece, sq);
            } else {
                hash_piece(&mut non_pawn_key[piece.colour()], piece, sq);
                if piece.piece_type() == PieceType::King {
                    hash_piece(&mut major_key, piece, sq);
                    hash_piece(&mut minor_key, piece, sq);
                } else if matches!(piece.piece_type(), PieceType::Queen | PieceType::Rook) {
                    hash_piece(&mut major_key, piece, sq);
                } else {
                    hash_piece(&mut minor_key, piece, sq);
                }
            }
        });

        if self.side == Colour::White {
            hash_side(&mut key);
        }

        if let Some(ep_sq) = self.ep_sq {
            hash_ep(&mut key, ep_sq);
        }

        hash_castling(&mut key, self.castle_perm);

        debug_assert!(self.fifty_move_counter <= 100);

        (key, pawn_key, non_pawn_key, minor_key, major_key)
    }

    #[cfg(feature = "datagen")]
    pub fn regenerate_zobrist(&mut self) {
        (self.key, self.pawn_key, self.non_pawn_key, self.minor_key, self.major_key) = self.generate_pos_keys();
    }

    #[cfg(feature = "datagen")]
    pub fn regenerate_threats(&mut self) {
        self.threats = self.generate_threats(self.side.flip());
    }

    pub fn generate_threats(&self, side: Colour) -> Threats {
        if side == Colour::White {
            self.generate_threats_from::<White>()
        } else {
            self.generate_threats_from::<Black>()
        }
    }

    pub fn generate_threats_from<C: Col>(&self) -> Threats {
        let mut threats = SquareSet::EMPTY;
        let mut checkers = SquareSet::EMPTY;

        let their_pawns = self.pieces.pawns::<C>();
        let their_knights = self.pieces.knights::<C>();
        let their_diags = self.pieces.diags::<C>();
        let their_orthos = self.pieces.orthos::<C>();
        let their_king = self.king_sq(C::COLOUR);
        let blockers = self.pieces.occupied();

        // compute threats
        threats |= pawn_attacks::<C>(their_pawns);

        for sq in their_knights {
            threats |= knight_attacks(sq);
        }
        for sq in their_diags {
            threats |= bishop_attacks(sq, blockers);
        }
        for sq in their_orthos {
            threats |= rook_attacks(sq, blockers);
        }

        threats |= king_attacks(their_king);

        // compute checkers
        let our_king = self.king_sq(C::Opposite::COLOUR);
        let king_bb = our_king.as_set();
        let backwards_from_king = pawn_attacks::<C::Opposite>(king_bb);
        checkers |= backwards_from_king & their_pawns;

        let knight_attacks = knight_attacks(our_king);

        checkers |= knight_attacks & their_knights;

        let diag_attacks = bishop_attacks(our_king, blockers);

        checkers |= diag_attacks & their_diags;

        let ortho_attacks = rook_attacks(our_king, blockers);

        checkers |= ortho_attacks & their_orthos;

        Threats { all: threats, /* pawn: pawn_threats, minor: minor_threats, rook: rook_threats, */ checkers }
    }

    pub fn reset(&mut self) {
        self.pieces.reset();
        self.piece_array = [None; 64];
        self.side = Colour::White;
        self.ep_sq = None;
        self.fifty_move_counter = 0;
        self.height = 0;
        self.ply = 0;
        self.castle_perm = CastlingRights::NONE;
        self.key = 0;
        self.pawn_key = 0;
        self.threats = Threats::default();
        self.history.clear();
    }

    pub fn set_frc_idx(&mut self, scharnagl: usize) {
        #![allow(clippy::cast_possible_truncation)]
        assert!(scharnagl < 960, "scharnagl index out of range");
        let backrank = Self::get_scharnagl_backrank(scharnagl);
        self.reset();
        for (&piece_type, file) in backrank.iter().zip(File::ALL) {
            let sq = Square::from_rank_file(Rank::One, file);
            self.add_piece(sq, Piece::new(Colour::White, piece_type));
        }
        for file in File::ALL {
            // add pawns
            let sq = Square::from_rank_file(Rank::Two, file);
            self.add_piece(sq, Piece::new(Colour::White, PieceType::Pawn));
        }
        for (&piece_type, file) in backrank.iter().zip(File::ALL) {
            let sq = Square::from_rank_file(Rank::Eight, file);
            self.add_piece(sq, Piece::new(Colour::Black, piece_type));
        }
        for file in File::ALL {
            // add pawns
            let sq = Square::from_rank_file(Rank::Seven, file);
            self.add_piece(sq, Piece::new(Colour::Black, PieceType::Pawn));
        }
        let mut rook_indices =
            backrank.iter().enumerate().filter_map(|(i, &piece)| if piece == PieceType::Rook { Some(i) } else { None });
        let queenside_file = rook_indices.next().unwrap();
        let kingside_file = rook_indices.next().unwrap();
        self.castle_perm = CastlingRights {
            wk: Some(Square::from_rank_file(Rank::One, File::from_index(kingside_file as u8).unwrap())),
            wq: Some(Square::from_rank_file(Rank::One, File::from_index(queenside_file as u8).unwrap())),
            bk: Some(Square::from_rank_file(Rank::Eight, File::from_index(kingside_file as u8).unwrap())),
            bq: Some(Square::from_rank_file(Rank::Eight, File::from_index(queenside_file as u8).unwrap())),
        };
        (self.key, self.pawn_key, self.non_pawn_key, self.minor_key, self.major_key) = self.generate_pos_keys();
        self.threats = self.generate_threats(self.side.flip());
    }

    pub fn set_dfrc_idx(&mut self, scharnagl: usize) {
        #![allow(clippy::cast_possible_truncation)]
        assert!(scharnagl < 960 * 960, "double scharnagl index out of range");
        let white_backrank = Self::get_scharnagl_backrank(scharnagl % 960);
        let black_backrank = Self::get_scharnagl_backrank(scharnagl / 960);
        self.reset();
        for (&piece_type, file) in white_backrank.iter().zip(File::ALL) {
            let sq = Square::from_rank_file(Rank::One, file);
            self.add_piece(sq, Piece::new(Colour::White, piece_type));
        }
        for file in File::ALL {
            // add pawns
            let sq = Square::from_rank_file(Rank::Two, file);
            self.add_piece(sq, Piece::new(Colour::White, PieceType::Pawn));
        }
        for (&piece_type, file) in black_backrank.iter().zip(File::ALL) {
            let sq = Square::from_rank_file(Rank::Eight, file);
            self.add_piece(sq, Piece::new(Colour::Black, piece_type));
        }
        for file in File::ALL {
            // add pawns
            let sq = Square::from_rank_file(Rank::Seven, file);
            self.add_piece(sq, Piece::new(Colour::Black, PieceType::Pawn));
        }
        let mut white_rook_indices =
            white_backrank.iter().enumerate().filter_map(
                |(i, &piece)| {
                    if piece == PieceType::Rook {
                        Some(i)
                    } else {
                        None
                    }
                },
            );
        let white_queenside_file = white_rook_indices.next().unwrap();
        let white_kingside_file = white_rook_indices.next().unwrap();
        let mut black_rook_indices =
            black_backrank.iter().enumerate().filter_map(
                |(i, &piece)| {
                    if piece == PieceType::Rook {
                        Some(i)
                    } else {
                        None
                    }
                },
            );
        let black_queenside_file = black_rook_indices.next().unwrap();
        let black_kingside_file = black_rook_indices.next().unwrap();
        self.castle_perm = CastlingRights {
            wk: Some(Square::from_rank_file(Rank::One, File::from_index(white_kingside_file as u8).unwrap())),
            wq: Some(Square::from_rank_file(Rank::One, File::from_index(white_queenside_file as u8).unwrap())),
            bk: Some(Square::from_rank_file(Rank::Eight, File::from_index(black_kingside_file as u8).unwrap())),
            bq: Some(Square::from_rank_file(Rank::Eight, File::from_index(black_queenside_file as u8).unwrap())),
        };
        (self.key, self.pawn_key, self.non_pawn_key, self.minor_key, self.major_key) = self.generate_pos_keys();
        self.threats = self.generate_threats(self.side.flip());
    }

    pub fn get_scharnagl_backrank(scharnagl: usize) -> [PieceType; 8] {
        // White's starting array can be derived from its number N (0 ... 959) as follows (https://en.wikipedia.org/wiki/Fischer_random_chess_numbering_scheme#Direct_derivation):
        // A. Divide N by 4, yielding quotient N2 and remainder B1. Place a Bishop upon the bright square corresponding to B1 (0=b, 1=d, 2=f, 3=h).
        // B. Divide N2 by 4 again, yielding quotient N3 and remainder B2. Place a second Bishop upon the dark square corresponding to B2 (0=a, 1=c, 2=e, 3=g).
        // C. Divide N3 by 6, yielding quotient N4 and remainder Q. Place the Queen according to Q, where 0 is the first free square starting from a, 1 is the second, etc.
        // D. N4 will be a single digit, 0 ... 9. Ignoring Bishops and Queen, find the positions of two Knights within the remaining five spaces.
        //    Place the Knights according to its value by consulting the following N5N table:
        // DIGIT | Knight Positioning
        //   0   | N N - - -
        //   1   | N - N - -
        //   2   | N - - N -
        //   3   | N - - - N
        //   4   | - N N - -
        //   5   | - N - N -
        //   6   | - N - - N
        //   7   | - - N N -
        //   8   | - - N - N
        //   9   | - - - N N
        // E. There are three blank squares remaining; place a Rook in each of the outer two and the King in the middle one.
        let mut out = [None; 8];
        let n = scharnagl;
        let (n2, b1) = (n / 4, n % 4);
        match b1 {
            0 => out[File::B] = Some(PieceType::Bishop),
            1 => out[File::D] = Some(PieceType::Bishop),
            2 => out[File::F] = Some(PieceType::Bishop),
            3 => out[File::H] = Some(PieceType::Bishop),
            _ => unreachable!(),
        }
        let (n3, b2) = (n2 / 4, n2 % 4);
        match b2 {
            0 => out[File::A] = Some(PieceType::Bishop),
            1 => out[File::C] = Some(PieceType::Bishop),
            2 => out[File::E] = Some(PieceType::Bishop),
            3 => out[File::G] = Some(PieceType::Bishop),
            _ => unreachable!(),
        }
        let (n4, mut q) = (n3 / 6, n3 % 6);
        for (idx, &piece) in out.iter().enumerate() {
            if piece.is_none() {
                if q == 0 {
                    out[idx] = Some(PieceType::Queen);
                    break;
                }
                q -= 1;
            }
        }
        let remaining_slots = out.iter_mut().filter(|piece| piece.is_none());
        let selection = match n4 {
            0 => [0, 1],
            1 => [0, 2],
            2 => [0, 3],
            3 => [0, 4],
            4 => [1, 2],
            5 => [1, 3],
            6 => [1, 4],
            7 => [2, 3],
            8 => [2, 4],
            9 => [3, 4],
            _ => unreachable!(),
        };
        for (i, slot) in remaining_slots.enumerate() {
            if i == selection[0] || i == selection[1] {
                *slot = Some(PieceType::Knight);
            }
        }

        out.iter_mut()
            .filter(|piece| piece.is_none())
            .zip([PieceType::Rook, PieceType::King, PieceType::Rook])
            .for_each(|(slot, piece)| *slot = Some(piece));

        out.map(Option::unwrap)
    }

    pub fn set_from_fen(&mut self, fen: &str) -> anyhow::Result<()> {
        if !fen.is_ascii() {
            bail!(format!("FEN string is not ASCII: {fen}"));
        }

        let mut rank = Rank::Eight;
        let mut file = File::A;

        self.reset();

        let fen_chars = fen.as_bytes();
        let split_idx =
            fen_chars.iter().position(|&c| c == b' ').with_context(|| format!("FEN string is missing space: {fen}"))?;
        let (board_part, info_part) = fen_chars.split_at(split_idx);

        for &c in board_part {
            let mut count = 1;
            let piece;
            match c {
                b'P' => piece = Some(Piece::WP),
                b'R' => piece = Some(Piece::WR),
                b'N' => piece = Some(Piece::WN),
                b'B' => piece = Some(Piece::WB),
                b'Q' => piece = Some(Piece::WQ),
                b'K' => piece = Some(Piece::WK),
                b'p' => piece = Some(Piece::BP),
                b'r' => piece = Some(Piece::BR),
                b'n' => piece = Some(Piece::BN),
                b'b' => piece = Some(Piece::BB),
                b'q' => piece = Some(Piece::BQ),
                b'k' => piece = Some(Piece::BK),
                b'1'..=b'8' => {
                    piece = None;
                    count = c - b'0';
                }
                b'/' => {
                    rank = rank.sub(1).unwrap();
                    file = File::A;
                    continue;
                }
                c => {
                    bail!("FEN string is invalid, got unexpected character: \"{}\"", c as char);
                }
            }

            for _ in 0..count {
                let sq = Square::from_rank_file(rank, file);
                if let Some(piece) = piece {
                    // this is only ever run once, as count is 1 for non-empty pieces.
                    self.add_piece(sq, piece);
                }
                file = file.add(1).unwrap_or(File::H);
            }
        }

        let mut info_parts = info_part[1..].split(|&c| c == b' ');

        self.set_side(info_parts.next())?;
        self.set_castling(info_parts.next())?;
        self.set_ep(info_parts.next())?;
        self.set_halfmove(info_parts.next())?;
        self.set_fullmove(info_parts.next())?;

        (self.key, self.pawn_key, self.non_pawn_key, self.minor_key, self.major_key) = self.generate_pos_keys();
        self.threats = self.generate_threats(self.side.flip());

        Ok(())
    }

    pub fn set_startpos(&mut self) {
        let starting_fen = if CHESS960.load(Ordering::SeqCst) { Self::STARTING_FEN_960 } else { Self::STARTING_FEN };
        self.set_from_fen(starting_fen).expect("for some reason, STARTING_FEN is now broken.");
    }

    #[cfg(test)]
    pub fn from_fen(fen: &str) -> anyhow::Result<Self> {
        let mut out = Self::new();
        out.set_from_fen(fen)?;
        Ok(out)
    }

    #[cfg(test)]
    pub fn from_frc_idx(scharnagl: usize) -> Self {
        let mut out = Self::new();
        out.set_frc_idx(scharnagl);
        out
    }

    #[cfg(test)]
    pub fn from_dfrc_idx(scharnagl: usize) -> Self {
        let mut out = Self::new();
        out.set_dfrc_idx(scharnagl);
        out
    }

    fn set_side(&mut self, side_part: Option<&[u8]>) -> anyhow::Result<()> {
        self.side = match side_part {
            Some([b'w']) => Colour::White,
            Some([b'b']) => Colour::Black,
            Some(other) => {
                bail!(format!(
                    "FEN string is invalid, expected side to be 'w' or 'b', got \"{}\"",
                    std::str::from_utf8(other).unwrap_or("<invalid utf8>")
                ))
            }
            None => bail!("FEN string is invalid, expected side part."),
        };
        Ok(())
    }

    fn set_castling(&mut self, castling_part: Option<&[u8]>) -> anyhow::Result<()> {
        match castling_part {
            None => bail!("FEN string is invalid, expected castling part."),
            Some(b"-") => self.castle_perm = CastlingRights::NONE,
            Some(castling) if !CHESS960.load(Ordering::SeqCst) => {
                for &c in castling {
                    match c {
                        b'K' => self.castle_perm.wk = Some(Square::H1),
                        b'Q' => self.castle_perm.wq = Some(Square::A1),
                        b'k' => self.castle_perm.bk = Some(Square::H8),
                        b'q' => self.castle_perm.bq = Some(Square::A8),
                        _ => {
                            bail!(format!(
                                "FEN string is invalid, expected castling part to be of the form 'KQkq', got \"{}\"",
                                std::str::from_utf8(castling).unwrap_or("<invalid utf8>")
                            ))
                        }
                    }
                }
            }
            Some(shredder_castling) => {
                // valid shredder castling strings are of the form "AHah", "Bd"
                let white_king = self.king_sq(Colour::White);
                let black_king = self.king_sq(Colour::Black);
                if white_king.rank() != Rank::One && shredder_castling.iter().any(u8::is_ascii_uppercase) {
                    bail!(format!("FEN string is invalid, white king is not on the back rank, but got uppercase castling characters, implying present castling rights, got \"{}\"", std::str::from_utf8(shredder_castling).unwrap_or("<invalid utf8>")));
                }
                if black_king.rank() != Rank::Eight && shredder_castling.iter().any(u8::is_ascii_lowercase) {
                    bail!(format!("FEN string is invalid, black king is not on the back rank, but got lowercase castling characters, implying present castling rights, got \"{}\"", std::str::from_utf8(shredder_castling).unwrap_or("<invalid utf8>")));
                }
                for &c in shredder_castling {
                    match c {
                        c if c.is_ascii_uppercase() => {
                            let file = File::from_index(c - b'A').unwrap();
                            let king_file = white_king.file();
                            if file == king_file {
                                bail!(format!("FEN string is invalid, white king is on file {:?}, but got castling rights on that file - got \"{}\"", king_file, std::str::from_utf8(shredder_castling).unwrap_or("<invalid utf8>")));
                            }
                            let sq = Square::from_rank_file(Rank::One, file);
                            if file > king_file {
                                // castling rights are to the right of the king, so it's "kingside" castling rights.
                                self.castle_perm.wk = Some(sq);
                            } else {
                                // castling rights are to the left of the king, so it's "queenside" castling rights.
                                self.castle_perm.wq = Some(sq);
                            }
                        }
                        c if c.is_ascii_lowercase() => {
                            let file = File::from_index(c - b'a').unwrap();
                            let king_file = black_king.file();
                            if file == king_file {
                                bail!(format!("FEN string is invalid, black king is on file {:?}, but got castling rights on that file - got \"{}\"", king_file, std::str::from_utf8(shredder_castling).unwrap_or("<invalid utf8>")));
                            }
                            let sq = Square::from_rank_file(Rank::Eight, file);
                            if file > king_file {
                                // castling rights are to the right of the king, so it's "kingside" castling rights.
                                self.castle_perm.bk = Some(sq);
                            } else {
                                // castling rights are to the left of the king, so it's "queenside" castling rights.
                                self.castle_perm.bq = Some(sq);
                            }
                        }
                        _ => {
                            bail!(format!("FEN string is invalid, expected castling part to be of the form 'AHah', 'Bd', or '-', got \"{}\"", std::str::from_utf8(shredder_castling).unwrap_or("<invalid utf8>")));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn set_ep(&mut self, ep_part: Option<&[u8]>) -> anyhow::Result<()> {
        match ep_part {
            None => bail!("FEN string is invalid, expected en passant part.".to_string()),
            Some([b'-']) => self.ep_sq = None,
            Some(ep_sq) => {
                if ep_sq.len() != 2 {
                    bail!(format!(
                        "FEN string is invalid, expected en passant part to be of the form 'a1', got \"{}\"",
                        std::str::from_utf8(ep_sq).unwrap_or("<invalid utf8>")
                    ));
                }
                let file = ep_sq[0] - b'a';
                let rank = ep_sq[1] - b'1';
                let file = File::from_index(file);
                let rank = Rank::from_index(rank);
                if !(file.is_some() && rank.is_some()) {
                    bail!(format!(
                        "FEN string is invalid, expected en passant part to be of the form 'a1', got \"{}\"",
                        std::str::from_utf8(ep_sq).unwrap_or("<invalid utf8>")
                    ));
                }
                self.ep_sq = Some(Square::from_rank_file(rank.unwrap(), file.unwrap()));
            }
        }

        Ok(())
    }

    fn set_halfmove(&mut self, halfmove_part: Option<&[u8]>) -> anyhow::Result<()> {
        match halfmove_part {
            None => bail!("FEN string is invalid, expected halfmove clock part.".to_string()),
            Some(halfmove_clock) => {
                self.fifty_move_counter = std::str::from_utf8(halfmove_clock)
                    .with_context(|| "FEN string is invalid, expected halfmove clock part to be valid UTF-8")?
                    .parse::<u8>()
                    .with_context(|| {
                        format!(
                            "FEN string is invalid, expected halfmove clock part to be a number, got \"{}\"",
                            std::str::from_utf8(halfmove_clock).unwrap_or("<invalid utf8>")
                        )
                    })?;
            }
        }

        Ok(())
    }

    fn set_fullmove(&mut self, fullmove_part: Option<&[u8]>) -> anyhow::Result<()> {
        match fullmove_part {
            None => bail!("FEN string is invalid, expected fullmove number part.".to_string()),
            Some(fullmove_number) => {
                let fullmove_number = std::str::from_utf8(fullmove_number)
                    .with_context(|| "FEN string is invalid, expected fullmove number part to be valid UTF-8")?
                    .parse::<usize>()
                    .with_context(|| "FEN string is invalid, expected fullmove number part to be a number")?;
                self.ply = (fullmove_number - 1) * 2;
                if self.side == Colour::Black {
                    self.ply += 1;
                }
            }
        }

        Ok(())
    }

    /// Determines if `sq` is attacked by `side`
    pub fn sq_attacked(&self, sq: Square, side: Colour) -> bool {
        if side == Colour::White {
            self.sq_attacked_by::<White>(sq)
        } else {
            self.sq_attacked_by::<Black>(sq)
        }
    }

    pub fn sq_attacked_by<C: Col>(&self, sq: Square) -> bool {
        // we remove this check because the board actually *can*
        // be in an inconsistent state when we call this, as it's
        // used to determine if a move is legal, and we'd like to
        // only do a lot of the make_move work *after* we've
        // determined that the move is legal.
        // #[cfg(debug_assertions)]
        // self.check_validity().unwrap();

        if C::WHITE == (self.side == Colour::Black) {
            return self.threats.all.contains_square(sq);
        }

        let sq_bb = sq.as_set();
        let our_pawns = self.pieces.pawns::<C>();
        let our_knights = self.pieces.knights::<C>();
        let our_diags = self.pieces.diags::<C>();
        let our_orthos = self.pieces.orthos::<C>();
        let our_king = self.pieces.king::<C>();
        let blockers = self.pieces.occupied();

        // pawns
        let attacks = pawn_attacks::<C>(our_pawns);
        if (attacks & sq_bb).non_empty() {
            return true;
        }

        // knights
        let knight_attacks_from_this_square = movegen::knight_attacks(sq);
        if (our_knights & knight_attacks_from_this_square).non_empty() {
            return true;
        }

        // bishops, queens
        let diag_attacks_from_this_square = movegen::bishop_attacks(sq, blockers);
        if (our_diags & diag_attacks_from_this_square).non_empty() {
            return true;
        }

        // rooks, queens
        let ortho_attacks_from_this_square = movegen::rook_attacks(sq, blockers);
        if (our_orthos & ortho_attacks_from_this_square).non_empty() {
            return true;
        }

        // king
        let king_attacks_from_this_square = movegen::king_attacks(sq);
        if (our_king & king_attacks_from_this_square).non_empty() {
            return true;
        }

        false
    }

    /// Checks whether a move is pseudo-legal.
    /// This means that it is a legal move, except for the fact that it might leave the king in check.
    pub fn is_pseudo_legal(&self, m: Move) -> bool {
        let from = m.from();
        let to = m.to();

        let moved_piece = self.piece_at(from);
        let captured_piece = if m.is_castle() { None } else { self.piece_at(to) };
        let is_pawn_double_push = self.is_double_pawn_push(m);

        let Some(moved_piece) = moved_piece else {
            return false;
        };

        if moved_piece.colour() != self.side {
            return false;
        }

        if let Some(captured_piece) = captured_piece {
            if captured_piece.colour() == self.side {
                return false;
            }
        }

        if moved_piece.piece_type() != PieceType::Pawn && (is_pawn_double_push || m.is_ep() || m.is_promo()) {
            return false;
        }

        if moved_piece.piece_type() != PieceType::King && m.is_castle() {
            return false;
        }

        if captured_piece.is_some() && is_pawn_double_push {
            return false;
        }

        if m.is_castle() {
            return self.is_pseudo_legal_castling(m);
        }

        if moved_piece.piece_type() == PieceType::Pawn {
            let should_be_promoting = to > Square::H7 || to < Square::A2;
            if should_be_promoting && !m.is_promo() {
                return false;
            }
            if m.is_ep() {
                return Some(to) == self.ep_sq;
            } else if is_pawn_double_push {
                if from.relative_to(self.side).rank() != Rank::Two {
                    return false;
                }
                let Some(one_forward) = from.pawn_push(self.side) else {
                    return false;
                };
                return self.piece_at(one_forward).is_none() && Some(to) == one_forward.pawn_push(self.side);
            } else if captured_piece.is_none() {
                return Some(to) == from.pawn_push(self.side);
            }
            // pawn capture
            if self.side == Colour::White {
                return (pawn_attacks::<White>(from.as_set()) & to.as_set()).non_empty();
            }
            return (pawn_attacks::<Black>(from.as_set()) & to.as_set()).non_empty();
        }

        (to.as_set() & movegen::attacks_by_type(moved_piece.piece_type(), from, self.pieces.occupied())).non_empty()
    }

    pub fn is_pseudo_legal_castling(&self, m: Move) -> bool {
        // illegal if:
        // - we're not moving the king
        // - we're not doing everything on the home rank
        // - we don't have castling rights on the target square
        // - we're in check
        // - there are pieces between the king and the rook
        // - the king passes through a square that is attacked by the opponent
        // - the king ends up in check (not checked here)
        let Some(moved) = self.piece_at(m.from()) else {
            return false;
        };
        if moved.piece_type() != PieceType::King {
            return false;
        }
        let home_rank = if self.side == Colour::White { SquareSet::RANK_1 } else { SquareSet::RANK_8 };
        if (m.to().as_set() & home_rank).is_empty() {
            return false;
        }
        if (m.from().as_set() & home_rank).is_empty() {
            return false;
        }
        let (king_dst, rook_dst) = if m.to() > m.from() {
            // kingside castling.
            if Some(m.to()) != (if self.side == Colour::Black { self.castle_perm.bk } else { self.castle_perm.wk }) {
                // the to-square doesn't match the castling rights
                // (it goes to the wrong place, or the rights don't exist)
                return false;
            }
            if self.side == Colour::Black {
                (Square::G8, Square::F8)
            } else {
                (Square::G1, Square::F1)
            }
        } else {
            // queenside castling.
            if Some(m.to()) != (if self.side == Colour::Black { self.castle_perm.bq } else { self.castle_perm.wq }) {
                // the to-square doesn't match the castling rights
                // (it goes to the wrong place, or the rights don't exist)
                return false;
            }
            if self.side == Colour::Black {
                (Square::C8, Square::D8)
            } else {
                (Square::C1, Square::D1)
            }
        };

        // king_path is the path the king takes to get to its destination.
        let king_path = RAY_BETWEEN[m.from()][king_dst];
        // rook_path is the path the rook takes to get to its destination.
        let rook_path = RAY_BETWEEN[m.from()][m.to()];
        // castle_occ is the occupancy that "counts" for castling.
        let castle_occ = self.pieces.occupied() ^ m.from().as_set() ^ m.to().as_set();

        (castle_occ & (king_path | rook_path | king_dst.as_set() | rook_dst.as_set())).is_empty()
            && !self.any_attacked(king_path | m.from().as_set(), self.side.flip())
    }

    pub fn any_attacked(&self, squares: SquareSet, by: Colour) -> bool {
        if by == self.side.flip() {
            (squares & self.threats.all).non_empty()
        } else {
            for sq in squares {
                if self.sq_attacked(sq, by) {
                    return true;
                }
            }
            false
        }
    }

    pub fn add_piece(&mut self, sq: Square, piece: Piece) {
        self.pieces.set_piece_at(sq, piece);
        *self.piece_at_mut(sq) = Some(piece);
    }

    /// Gets the piece that will be moved by the given move.
    pub fn moved_piece(&self, m: Move) -> Option<Piece> {
        let idx = m.from();
        self.piece_array[idx]
    }

    /// Gets the piece that will be captured by the given move.
    pub fn captured_piece(&self, m: Move) -> Option<Piece> {
        if m.is_castle() {
            return None;
        }
        let idx = m.to();
        self.piece_array[idx]
    }

    /// Determines whether this move would be a capture in the current position.
    pub fn is_capture(&self, m: Move) -> bool {
        self.captured_piece(m).is_some()
    }

    /// Determines whether this move would be a double pawn push in the current position.
    pub fn is_double_pawn_push(&self, m: Move) -> bool {
        let from_bb = m.from().as_set();
        if (from_bb & (SquareSet::RANK_2 | SquareSet::RANK_7)).is_empty() {
            return false;
        }
        let to_bb = m.to().as_set();
        if (to_bb & (SquareSet::RANK_4 | SquareSet::RANK_5)).is_empty() {
            return false;
        }
        let Some(piece_moved) = self.moved_piece(m) else {
            return false;
        };
        piece_moved.piece_type() == PieceType::Pawn
    }

    /// Determines whether this move would be tactical in the current position.
    pub fn is_tactical(&self, m: Move) -> bool {
        m.is_promo() || m.is_ep() || self.is_capture(m)
    }

    /// Gets the piece at the given square.
    pub fn piece_at(&self, sq: Square) -> Option<Piece> {
        self.piece_array[sq]
    }

    /// Gets a mutable reference to the piece at the given square.
    pub fn piece_at_mut(&mut self, sq: Square) -> &mut Option<Piece> {
        &mut self.piece_array[sq]
    }

    pub fn make_move_simple(&mut self, m: Move) -> bool {
        self.make_move_base(m, &mut UpdateBuffer::default())
    }

    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    pub fn make_move_base(&mut self, m: Move, update_buffer: &mut UpdateBuffer) -> bool {
        #[cfg(debug_assertions)]
        self.check_validity().unwrap();

        let from = m.from();
        let mut to = m.to();
        let side = self.side;
        let Some(piece) = self.moved_piece(m) else {
            return false;
        };
        let captured = self.captured_piece(m);

        let saved_state = Undo {
            castle_perm: self.castle_perm,
            ep_square: self.ep_sq,
            fifty_move_counter: self.fifty_move_counter,
            threats: self.threats,
            cont_hist_index: Some(ContHistIndex { piece, square: m.history_to_square() }),
            piece_layout: self.pieces,
            piece_array: self.piece_array,
            key: self.key,
            pawn_key: self.pawn_key,
            non_pawn_key: self.non_pawn_key,
            minor_key: self.minor_key,
            major_key: self.major_key,
        };

        // from, to, and piece are valid unless this is a castling move,
        // as castling is encoded as king-captures-rook.
        // we sort out castling in a branch later, dw about it.
        if !m.is_castle() {
            if m.is_promo() {
                // just remove the source piece, as a different piece will be arriving here
                update_buffer.clear_piece(from, piece);
            } else {
                update_buffer.move_piece(from, to, piece);
            }
        }

        if m.is_ep() {
            let clear_at = if side == Colour::White { to.sub(8) } else { to.add(8) }.unwrap();
            let to_clear = Piece::new(side.flip(), PieceType::Pawn);
            self.pieces.clear_piece_at(clear_at, to_clear);
            update_buffer.clear_piece(clear_at, to_clear);
        } else if m.is_castle() {
            self.pieces.clear_piece_at(from, piece);
            let (rook_from, rook_to) = match to {
                _ if Some(to) == self.castle_perm.wk => {
                    to = Square::G1;
                    (self.castle_perm.wk.unwrap(), Square::F1)
                }
                _ if Some(to) == self.castle_perm.wq => {
                    to = Square::C1;
                    (self.castle_perm.wq.unwrap(), Square::D1)
                }
                _ if Some(to) == self.castle_perm.bk => {
                    to = Square::G8;
                    (self.castle_perm.bk.unwrap(), Square::F8)
                }
                _ if Some(to) == self.castle_perm.bq => {
                    to = Square::C8;
                    (self.castle_perm.bq.unwrap(), Square::D8)
                }
                _ => {
                    panic!("Invalid castle move, to: {}, castle_perm: {}", to, self.castle_perm);
                }
            };
            if from != to {
                update_buffer.move_piece(from, to, piece);
            }
            if rook_from != rook_to {
                let rook = Piece::new(side, PieceType::Rook);
                self.pieces.move_piece(rook_from, rook_to, rook);
                update_buffer.move_piece(rook_from, rook_to, rook);
            }
        }

        self.ep_sq = None;

        self.fifty_move_counter += 1;

        if let Some(captured) = captured {
            self.fifty_move_counter = 0;
            self.pieces.clear_piece_at(to, captured);
            update_buffer.clear_piece(to, captured);
        }

        if piece.piece_type() == PieceType::Pawn {
            self.fifty_move_counter = 0;
            if self.is_double_pawn_push(m)
                && (m.to().as_set().west_one() | m.to().as_set().east_one())
                    & self.pieces.all_pawns()
                    & self.pieces.occupied_co(side.flip())
                    != SquareSet::EMPTY
            {
                if side == Colour::White {
                    self.ep_sq = from.add(8);
                    debug_assert!(self.ep_sq.unwrap().rank() == Rank::Three);
                } else {
                    self.ep_sq = from.sub(8);
                    debug_assert!(self.ep_sq.unwrap().rank() == Rank::Six);
                }
            }
        }

        if let Some(promo) = m.promotion_type() {
            let promo = Piece::new(side, promo);
            debug_assert!(promo.piece_type().legal_promo());
            self.pieces.clear_piece_at(from, piece);
            self.pieces.set_piece_at(to, promo);
            update_buffer.add_piece(to, promo);
        } else if m.is_castle() {
            self.pieces.set_piece_at(to, piece); // stupid hack for piece-swapping
        } else {
            self.pieces.move_piece(from, to, piece);
        }

        self.side = self.side.flip();

        // reversed in_check fn, as we have now swapped sides
        if self.sq_attacked(self.king_sq(self.side.flip()), self.side) {
            // this would be a function but we run into borrow checker issues
            // because it's currently not smart enough to realize that we're
            // borrowing disjoint parts of the board.
            let Undo { ep_square, fifty_move_counter, piece_layout, .. } = saved_state;

            // self.height -= 1;
            // self.ply -= 1;
            self.side = self.side.flip();
            // self.key = key;
            // self.pawn_key = pawn_key;
            // self.non_pawn_key = non_pawn_key;
            // self.minor_key = minor_key;
            // self.major_key = major_key;
            // self.material_key = material_key;
            // self.castle_perm = castle_perm;
            self.ep_sq = ep_square;
            self.fifty_move_counter = fifty_move_counter;
            // self.threats = threats;
            self.pieces = piece_layout;
            // self.piece_array = piece_array;
            return false;
        }

        let mut key = self.key;
        let mut pawn_key = self.pawn_key;
        let mut non_pawn_key = self.non_pawn_key;
        let mut minor_key = self.minor_key;
        let mut major_key = self.major_key;

        // remove a previous en passant square from the hash
        if let Some(ep_sq) = saved_state.ep_square {
            hash_ep(&mut key, ep_sq);
        }

        // hash out the castling to insert it again after updating rights.
        hash_castling(&mut key, self.castle_perm);

        // update castling rights
        let mut new_rights = self.castle_perm;
        if piece == Piece::WR {
            if Some(from) == self.castle_perm.wk {
                new_rights.wk = None;
            } else if Some(from) == self.castle_perm.wq {
                new_rights.wq = None;
            }
        } else if piece == Piece::BR {
            if Some(from) == self.castle_perm.bk {
                new_rights.bk = None;
            } else if Some(from) == self.castle_perm.bq {
                new_rights.bq = None;
            }
        } else if piece == Piece::WK {
            new_rights.wk = None;
            new_rights.wq = None;
        } else if piece == Piece::BK {
            new_rights.bk = None;
            new_rights.bq = None;
        }
        new_rights.remove(to);
        self.castle_perm = new_rights;

        // apply all the updates to the zobrist hash
        if let Some(ep_sq) = self.ep_sq {
            hash_ep(&mut key, ep_sq);
        }
        hash_side(&mut key);
        for &FeatureUpdate { sq, piece } in update_buffer.subs() {
            self.piece_array[sq] = None;
            hash_piece(&mut key, piece, sq);
            if piece.piece_type() == PieceType::Pawn {
                hash_piece(&mut pawn_key, piece, sq);
            } else {
                hash_piece(&mut non_pawn_key[piece.colour()], piece, sq);
                if piece.piece_type() == PieceType::King {
                    hash_piece(&mut major_key, piece, sq);
                    hash_piece(&mut minor_key, piece, sq);
                } else if matches!(piece.piece_type(), PieceType::Queen | PieceType::Rook) {
                    hash_piece(&mut major_key, piece, sq);
                } else {
                    hash_piece(&mut minor_key, piece, sq);
                }
            }
        }
        for &FeatureUpdate { sq, piece } in update_buffer.adds() {
            self.piece_array[sq] = Some(piece);
            hash_piece(&mut key, piece, sq);
            if piece.piece_type() == PieceType::Pawn {
                hash_piece(&mut pawn_key, piece, sq);
            } else {
                hash_piece(&mut non_pawn_key[piece.colour()], piece, sq);
                if piece.piece_type() == PieceType::King {
                    hash_piece(&mut major_key, piece, sq);
                    hash_piece(&mut minor_key, piece, sq);
                } else if matches!(piece.piece_type(), PieceType::Queen | PieceType::Rook) {
                    hash_piece(&mut major_key, piece, sq);
                } else {
                    hash_piece(&mut minor_key, piece, sq);
                }
            }
        }
        // reinsert the castling rights
        hash_castling(&mut key, self.castle_perm);
        self.key = key;
        self.pawn_key = pawn_key;
        self.non_pawn_key = non_pawn_key;
        self.minor_key = minor_key;
        self.major_key = major_key;

        self.ply += 1;
        self.height += 1;

        self.threats = self.generate_threats(self.side.flip());

        self.history.push(saved_state);

        #[cfg(debug_assertions)]
        self.check_validity().unwrap();

        true
    }

    pub fn unmake_move_base(&mut self) {
        // we remove this check because the board actually *can*
        // be in an inconsistent state when we call this, as we
        // may be unmaking a move that was determined to be
        // illegal, and as such the full make_move hasn't been
        // run yet.
        // #[cfg(debug_assertions)]
        // self.check_validity().unwrap();

        let undo = self.history.last().expect("No move to unmake!");

        let Undo {
            castle_perm,
            ep_square,
            fifty_move_counter,
            threats,
            piece_layout,
            piece_array,
            key,
            pawn_key,
            non_pawn_key,
            minor_key,
            major_key,
            ..
        } = undo;

        self.height -= 1;
        self.ply -= 1;
        self.side = self.side.flip();
        self.key = *key;
        self.pawn_key = *pawn_key;
        self.non_pawn_key = *non_pawn_key;
        self.minor_key = *minor_key;
        self.major_key = *major_key;
        self.castle_perm = *castle_perm;
        self.ep_sq = *ep_square;
        self.fifty_move_counter = *fifty_move_counter;
        self.threats = *threats;
        self.pieces = *piece_layout;
        self.piece_array = *piece_array;

        self.history.pop();

        #[cfg(debug_assertions)]
        self.check_validity().unwrap();
    }

    pub fn make_nullmove(&mut self) {
        #[cfg(debug_assertions)]
        self.check_validity().unwrap();
        debug_assert!(!self.in_check());

        self.history.push(Undo { ep_square: self.ep_sq, threats: self.threats, key: self.key, ..Default::default() });

        let mut key = self.key;
        if let Some(ep_sq) = self.ep_sq {
            hash_ep(&mut key, ep_sq);
        }
        hash_side(&mut key);
        self.key = key;

        self.ep_sq = None;
        self.side = self.side.flip();
        self.ply += 1;
        self.height += 1;

        self.threats = self.generate_threats(self.side.flip());

        #[cfg(debug_assertions)]
        self.check_validity().unwrap();
    }

    pub fn unmake_nullmove(&mut self) {
        #[cfg(debug_assertions)]
        self.check_validity().unwrap();

        self.height -= 1;
        self.ply -= 1;
        self.side = self.side.flip();

        let Undo { ep_square, threats, key, .. } = self.history.last().expect("No move to unmake!");

        self.ep_sq = *ep_square;
        self.threats = *threats;
        self.key = *key;

        self.history.pop();

        #[cfg(debug_assertions)]
        self.check_validity().unwrap();
    }

    pub fn make_move_nnue(&mut self, m: Move, t: &mut ThreadData) -> bool {
        let mut update_buffer = UpdateBuffer::default();
        let Some(piece) = self.moved_piece(m) else {
            return false;
        };
        let res = self.make_move_base(m, &mut update_buffer);
        if !res {
            return false;
        }

        t.nnue.accumulators[t.nnue.current_acc].mv = MovedPiece { from: m.from(), to: m.to(), piece };
        t.nnue.accumulators[t.nnue.current_acc].update_buffer = update_buffer;

        t.nnue.current_acc += 1;

        t.nnue.accumulators[t.nnue.current_acc].correct = [false; 2];

        true
    }

    pub fn unmake_move_nnue(&mut self, t: &mut ThreadData) {
        self.unmake_move_base();
        t.nnue.current_acc -= 1;
    }

    pub fn make_move(&mut self, m: Move, t: &mut ThreadData) -> bool {
        self.make_move_nnue(m, t)
    }

    pub fn unmake_move(&mut self, t: &mut ThreadData) {
        self.unmake_move_nnue(t);
    }

    pub fn last_move_was_nullmove(&self) -> bool {
        if let Some(Undo { piece_layout, .. }) = self.history.last() {
            piece_layout.all_kings().is_empty()
        } else {
            false
        }
    }

    /// Makes a guess about the new position key after a move.
    /// This is a cheap estimate, and will fail for special moves such as promotions and castling.
    pub fn key_after(&self, m: Move) -> u64 {
        let src = m.from();
        let tgt = m.to();
        let piece = self.moved_piece(m).unwrap();
        let captured = self.piece_at(tgt);

        let mut new_key = self.key;
        hash_side(&mut new_key);
        hash_piece(&mut new_key, piece, src);
        hash_piece(&mut new_key, piece, tgt);

        if let Some(captured) = captured {
            hash_piece(&mut new_key, captured, tgt);
        }

        new_key
    }

    pub fn key_after_null_move(&self) -> u64 {
        let mut new_key = self.key;
        hash_side(&mut new_key);
        new_key
    }

    /// Parses a move in the UCI format and returns a move or a reason why it couldn't be parsed.
    pub fn parse_uci(&self, uci: &str) -> anyhow::Result<Move> {
        use crate::errors::MoveParseError::{
            IllegalMove, InvalidFromSquareFile, InvalidFromSquareRank, InvalidLength, InvalidPromotionPiece,
            InvalidToSquareFile, InvalidToSquareRank, Unknown,
        };
        let san_bytes = uci.as_bytes();
        if !(4..=5).contains(&san_bytes.len()) {
            bail!(InvalidLength(san_bytes.len()));
        }
        if !(b'a'..=b'h').contains(&san_bytes[0]) {
            bail!(InvalidFromSquareFile(san_bytes[0] as char));
        }
        if !(b'1'..=b'8').contains(&san_bytes[1]) {
            bail!(InvalidFromSquareRank(san_bytes[1] as char));
        }
        if !(b'a'..=b'h').contains(&san_bytes[2]) {
            bail!(InvalidToSquareFile(san_bytes[2] as char));
        }
        if !(b'1'..=b'8').contains(&san_bytes[3]) {
            bail!(InvalidToSquareRank(san_bytes[3] as char));
        }
        if san_bytes.len() == 5 && ![b'n', b'b', b'r', b'q', b'k'].contains(&san_bytes[4]) {
            bail!(InvalidPromotionPiece(san_bytes[4] as char));
        }

        let from = Square::from_rank_file(
            Rank::from_index(san_bytes[1] - b'1').with_context(|| Unknown)?,
            File::from_index(san_bytes[0] - b'a').with_context(|| Unknown)?,
        );
        let to = Square::from_rank_file(
            Rank::from_index(san_bytes[3] - b'1').with_context(|| Unknown)?,
            File::from_index(san_bytes[2] - b'a').with_context(|| Unknown)?,
        );

        let mut list = MoveList::new();
        self.generate_moves(&mut list);

        let frc_cleanup = !CHESS960.load(Ordering::Relaxed);
        let res = list
            .iter_moves()
            .copied()
            .find(|&m| {
                let m_to = if frc_cleanup && m.is_castle() {
                    // if we're in normal UCI mode, we'll rework our castling moves into the
                    // standard format.
                    match m.to() {
                        Square::A1 => Square::C1,
                        Square::H1 => Square::G1,
                        Square::A8 => Square::C8,
                        Square::H8 => Square::G8,
                        _ => m.to(),
                    }
                } else {
                    m.to()
                };
                m.from() == from
                    && m_to == to
                    && (san_bytes.len() == 4
                        || m.promotion_type().and_then(PieceType::promo_char).unwrap() == san_bytes[4] as char)
            })
            .with_context(|| IllegalMove(uci.to_string()));

        res
    }

    pub fn san(&mut self, m: Move) -> Option<String> {
        let check_char = match self.gives(m) {
            CheckState::None => "",
            CheckState::Check => "+",
            CheckState::Checkmate => "#",
        };
        if m.is_castle() {
            match () {
                () if m.to() > m.from() => return Some(format!("O-O{check_char}")),
                () if m.to() < m.from() => return Some(format!("O-O-O{check_char}")),
                () => unreachable!(),
            }
        }
        let to_sq = m.to();
        let moved_piece = self.piece_at(m.from())?;
        let is_capture =
            self.is_capture(m) || (moved_piece.piece_type() == PieceType::Pawn && Some(to_sq) == self.ep_sq);
        let piece_prefix = match moved_piece.piece_type() {
            PieceType::Pawn if !is_capture => "",
            PieceType::Pawn => &"abcdefgh"[m.from().file() as usize..=m.from().file() as usize],
            PieceType::Knight => "N",
            PieceType::Bishop => "B",
            PieceType::Rook => "R",
            PieceType::Queen => "Q",
            PieceType::King => "K",
        };
        let possible_ambiguous_attackers = if moved_piece.piece_type() == PieceType::Pawn {
            SquareSet::EMPTY
        } else {
            movegen::attacks_by_type(moved_piece.piece_type(), to_sq, self.pieces.occupied())
                & self.pieces.piece_bb(moved_piece)
        };
        let needs_disambiguation =
            possible_ambiguous_attackers.count() > 1 && moved_piece.piece_type() != PieceType::Pawn;
        let from_file = SquareSet::FILES[m.from().file()];
        let from_rank = SquareSet::RANKS[m.from().rank()];
        let can_be_disambiguated_by_file = (possible_ambiguous_attackers & from_file).count() == 1;
        let can_be_disambiguated_by_rank = (possible_ambiguous_attackers & from_rank).count() == 1;
        let needs_both = !can_be_disambiguated_by_file && !can_be_disambiguated_by_rank;
        let must_be_disambiguated_by_file = needs_both || can_be_disambiguated_by_file;
        let must_be_disambiguated_by_rank =
            needs_both || (can_be_disambiguated_by_rank && !can_be_disambiguated_by_file);
        let disambiguator1 = if needs_disambiguation && must_be_disambiguated_by_file {
            &"abcdefgh"[m.from().file() as usize..=m.from().file() as usize]
        } else {
            ""
        };
        let disambiguator2 = if needs_disambiguation && must_be_disambiguated_by_rank {
            &"12345678"[m.from().rank() as usize..=m.from().rank() as usize]
        } else {
            ""
        };
        let capture_sigil = if is_capture { "x" } else { "" };
        let promo_str = match m.promotion_type() {
            Some(PieceType::Knight) => "=N",
            Some(PieceType::Bishop) => "=B",
            Some(PieceType::Rook) => "=R",
            Some(PieceType::Queen) => "=Q",
            None => "",
            _ => unreachable!(),
        };
        let san =
            format!("{piece_prefix}{disambiguator1}{disambiguator2}{capture_sigil}{to_sq}{promo_str}{check_char}");
        Some(san)
    }

    pub fn gives(&mut self, m: Move) -> CheckState {
        if !self.make_move_simple(m) {
            return CheckState::None;
        }
        let gives_check = self.in_check();
        if gives_check {
            let mut ml = MoveList::new();
            self.generate_moves(&mut ml);
            for &m in ml.iter_moves() {
                if !self.make_move_simple(m) {
                    continue;
                }
                // we found a legal move, so m does not give checkmate.
                self.unmake_move_base();
                self.unmake_move_base();
                return CheckState::Check;
            }
            // we didn't return, so there were no legal moves,
            // so m gives checkmate.
            self.unmake_move_base();
            return CheckState::Checkmate;
        }
        self.unmake_move_base();
        CheckState::None
    }

    /// Has the current position occurred before in the current game?
    pub fn is_repetition(&self) -> bool {
        let mut counter = 0;
        // distance to the last irreversible move
        let moves_since_zeroing = self.fifty_move_counter() as usize;
        // a repetition is first possible at four ply back:
        for (dist_back, u) in self.history.iter().rev().enumerate().take(moves_since_zeroing).skip(3).step_by(2) {
            if u.key == self.key {
                // in-tree, can twofold:
                if dist_back < self.height {
                    return true;
                }
                // partially materialised, proper threefold:
                counter += 1;
                if counter >= 2 {
                    return true;
                }
            }
        }
        false
    }

    /// Should we consider the current position a draw?
    pub fn is_draw(&self) -> bool {
        (self.fifty_move_counter >= 100 || self.is_repetition()) && self.height != 0
    }

    pub fn pv_san(&mut self, pv: &PVariation) -> Result<String, fmt::Error> {
        let mut out = String::new();
        let mut moves_made = 0;
        for &m in pv.moves() {
            write!(out, "{} ", self.san(m).unwrap_or_else(|| "???".to_string()))?;
            self.make_move_simple(m);
            moves_made += 1;
        }
        for _ in 0..moves_made {
            self.unmake_move_base();
        }
        Ok(out)
    }

    pub fn legal_moves(&mut self) -> Vec<Move> {
        let mut move_list = MoveList::new();
        self.generate_moves(&mut move_list);
        let mut legal_moves = Vec::new();
        for &m in move_list.iter_moves() {
            if self.make_move_simple(m) {
                self.unmake_move_base();
                legal_moves.push(m);
            }
        }
        legal_moves
    }

    pub const fn fifty_move_counter(&self) -> u8 {
        self.fifty_move_counter
    }

    #[cfg(any(feature = "datagen", test))]
    pub fn has_insufficient_material<C: Col>(&self) -> bool {
        if (self.pieces.pawns::<C>() | self.pieces.rooks::<C>() | self.pieces.queens::<C>()).non_empty() {
            return false;
        }

        if self.pieces.knights::<C>().non_empty() {
            // this approach renders KNNvK as *not* being insufficient material.
            // this is because the losing side can in theory help the winning side
            // into a checkmate, despite it being impossible to /force/ mate.
            let kings = self.pieces.all_kings();
            let queens = self.pieces.all_queens();
            return self.pieces.our_pieces::<C>().count() <= 2
                && (self.pieces.their_pieces::<C>() & !kings & !queens).is_empty();
        }

        if self.pieces.bishops::<C>().non_empty() {
            let bishops = self.pieces.all_bishops();
            let same_color =
                (bishops & SquareSet::DARK_SQUARES).is_empty() || (bishops & SquareSet::LIGHT_SQUARES).is_empty();
            let pawns = self.pieces.all_pawns();
            let knights = self.pieces.all_knights();
            return same_color && pawns.is_empty() && knights.is_empty();
        }

        true
    }

    #[cfg(feature = "datagen")]
    pub const fn full_move_number(&self) -> usize {
        self.ply / 2 + 1
    }

    pub fn has_game_cycle(&self, ply: usize) -> bool {
        let end = std::cmp::min(self.fifty_move_counter() as usize, self.history.len());

        if end < 3 {
            return false;
        }

        let old_key = |i: usize| self.history[self.history.len() - i].key;

        let occ = self.pieces.occupied();
        let original_key = self.key;

        let mut other = !(original_key ^ old_key(1));

        for i in (3..=end).step_by(2) {
            let curr_key = old_key(i);

            other ^= !(curr_key ^ old_key(i - 1));
            if other != 0 {
                continue;
            }

            let diff = original_key ^ curr_key;

            let mut slot = cuckoo::h1(diff);

            if diff != cuckoo::KEYS[slot] {
                slot = cuckoo::h2(diff);
            }

            if diff != cuckoo::KEYS[slot] {
                continue;
            }

            let mv = cuckoo::MOVES[slot].unwrap();

            if (occ & RAY_BETWEEN[mv.from()][mv.to()]).is_empty() {
                // repetition is after root, done:
                if ply > i {
                    return true;
                }

                let mut piece = self.piece_at(mv.from());
                if piece.is_none() {
                    piece = self.piece_at(mv.to());
                }

                return piece.unwrap().colour() == self.side;
            }
        }

        false
    }

    #[cfg(feature = "datagen")]
    pub fn make_random_move(&mut self, rng: &mut ThreadRng, t: &mut ThreadData) -> Option<Move> {
        let mut ml = MoveList::new();
        self.generate_moves(&mut ml);
        let self::movegen::MoveListEntry { mov, .. } = ml.choose(rng)?;
        self.make_move(*mov, t);
        Some(*mov)
    }

    #[cfg(any(feature = "datagen", test))]
    pub fn is_insufficient_material(&self) -> bool {
        self.has_insufficient_material::<White>() && self.has_insufficient_material::<Black>()
    }

    #[cfg(any(feature = "datagen", test))]
    pub fn outcome(&mut self) -> GameOutcome {
        if self.fifty_move_counter >= 100 {
            return GameOutcome::Draw(DrawType::FiftyMoves);
        }
        let mut reps = 1;
        for undo in self.history.iter().rev().skip(1).step_by(2) {
            if undo.key == self.key {
                reps += 1;
                if reps == 3 {
                    return GameOutcome::Draw(DrawType::Repetition);
                }
            }
            // optimisation: if the fifty move counter was zeroed, then any prior positions will not be repetitions.
            if undo.fifty_move_counter == 0 {
                break;
            }
        }
        if self.is_insufficient_material() {
            return GameOutcome::Draw(DrawType::InsufficientMaterial);
        }
        let mut move_list = MoveList::new();
        self.generate_moves(&mut move_list);
        let mut legal_moves = false;
        for &m in move_list.iter_moves() {
            if self.make_move_simple(m) {
                self.unmake_move_base();
                legal_moves = true;
                break;
            }
        }
        if legal_moves {
            GameOutcome::Ongoing
        } else if self.in_check() {
            match self.side {
                Colour::White => GameOutcome::BlackWin(WinType::Mate),
                Colour::Black => GameOutcome::WhiteWin(WinType::Mate),
            }
        } else {
            GameOutcome::Draw(DrawType::Stalemate)
        }
    }

    #[cfg(debug_assertions)]
    pub fn assert_mated(&mut self) {
        assert!(self.in_check());
        let mut move_list = MoveList::new();
        self.generate_moves(&mut move_list);
        for &mv in move_list.iter_moves() {
            assert!(!self.make_move_simple(mv));
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GameOutcome {
    WhiteWin(WinType),
    BlackWin(WinType),
    Draw(DrawType),
    Ongoing,
}


#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WinType {
    Mate, TB, Adjudication,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DrawType {
    TB, FiftyMoves, Repetition, Stalemate, InsufficientMaterial, Adjudication
}

#[cfg(feature = "datagen")]
impl GameOutcome {
    pub const fn as_packed_u8(self) -> u8 {
        // 0 for black win, 1 for draw, 2 for white win
        match self {
            Self::WhiteWin(_) => 2,
            Self::BlackWin(_) => 0,
            Self::Draw(_) => 1,
            Self::Ongoing => panic!("Game is not over!"),
        }
    }
}

impl Default for Board {
    fn default() -> Self {
        let mut out = Self::new();
        out.set_startpos();
        out
    }
}

impl Display for Board {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        let mut counter = 0;
        for rank in Rank::ALL.into_iter().rev() {
            for file in File::ALL {
                let sq = Square::from_rank_file(rank, file);
                let piece = self.piece_at(sq);
                if let Some(piece) = piece {
                    if counter != 0 {
                        write!(f, "{counter}")?;
                    }
                    counter = 0;
                    write!(f, "{piece}")?;
                } else {
                    counter += 1;
                }
            }
            if counter != 0 {
                write!(f, "{counter}")?;
            }
            counter = 0;
            if rank != Rank::One {
                write!(f, "/")?;
            }
        }

        if self.side == Colour::White {
            write!(f, " w")?;
        } else {
            write!(f, " b")?;
        }
        write!(f, " ")?;
        if self.castle_perm == CastlingRights::NONE {
            write!(f, "-")?;
        } else {
            for (_, ch) in [self.castle_perm.wk, self.castle_perm.wq, self.castle_perm.bk, self.castle_perm.bq]
                .into_iter()
                .zip("KQkq".chars())
                .filter(|(m, _)| m.is_some())
            {
                write!(f, "{ch}")?;
            }
        }
        if let Some(ep_sq) = self.ep_sq {
            write!(f, " {ep_sq}")?;
        } else {
            write!(f, " -")?;
        }
        write!(f, " {}", self.fifty_move_counter)?;
        write!(f, " {}", self.ply / 2 + 1)?;

        Ok(())
    }
}

impl std::fmt::UpperHex for Board {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        for rank in Rank::ALL.into_iter().rev() {
            write!(f, "{} ", rank as u8 + 1)?;
            for file in File::ALL {
                let sq = Square::from_rank_file(rank, file);
                if let Some(piece) = self.piece_at(sq) {
                    write!(f, "{piece} ")?;
                } else {
                    write!(f, ". ")?;
                }
            }
            writeln!(f)?;
        }

        writeln!(f, "  a b c d e f g h")?;
        writeln!(f, "FEN: {self}")?;

        Ok(())
    }
}

mod tests {
    #[test]
    fn read_fen_validity() {
        use super::Board;

        let mut board_1 = Board::new();
        board_1.set_from_fen(Board::STARTING_FEN).expect("setfen failed.");
        board_1.check_validity().unwrap();

        let board_2 = Board::from_fen(Board::STARTING_FEN).expect("setfen failed.");
        board_2.check_validity().unwrap();

        assert_eq!(board_1, board_2);
    }

    #[test]
    fn game_end_states() {
        use super::Board;
        use super::{GameOutcome, DrawType};
        use crate::{chessmove::Move, util::Square};

        let mut fiftymove_draw =
            Board::from_fen("rnbqkb1r/pppppppp/5n2/8/3N4/8/PPPPPPPP/RNBQKB1R b KQkq - 100 2").unwrap();
        assert_eq!(fiftymove_draw.outcome(), GameOutcome::Draw(DrawType::FiftyMoves));
        let mut draw_repetition = Board::default();
        assert_eq!(draw_repetition.outcome(), GameOutcome::Ongoing);
        draw_repetition.make_move_simple(Move::new(Square::G1, Square::F3));
        draw_repetition.make_move_simple(Move::new(Square::B8, Square::C6));
        assert_eq!(draw_repetition.outcome(), GameOutcome::Ongoing);
        draw_repetition.make_move_simple(Move::new(Square::F3, Square::G1));
        draw_repetition.make_move_simple(Move::new(Square::C6, Square::B8));
        assert_eq!(draw_repetition.outcome(), GameOutcome::Ongoing);
        draw_repetition.make_move_simple(Move::new(Square::G1, Square::F3));
        draw_repetition.make_move_simple(Move::new(Square::B8, Square::C6));
        assert_eq!(draw_repetition.outcome(), GameOutcome::Ongoing);
        draw_repetition.make_move_simple(Move::new(Square::F3, Square::G1));
        draw_repetition.make_move_simple(Move::new(Square::C6, Square::B8));
        assert_eq!(draw_repetition.outcome(), GameOutcome::Draw(DrawType::Repetition));
        let mut stalemate = Board::from_fen("7k/8/6Q1/8/8/8/8/K7 b - - 0 1").unwrap();
        assert_eq!(stalemate.outcome(), GameOutcome::Draw(DrawType::Stalemate));
        let mut insufficient_material_bare_kings = Board::from_fen("8/8/5k2/8/8/2K5/8/8 b - - 0 1").unwrap();
        assert_eq!(insufficient_material_bare_kings.outcome(), GameOutcome::Draw(DrawType::InsufficientMaterial));
        let mut insufficient_material_knights = Board::from_fen("8/8/5k2/8/2N5/2K2N2/8/8 b - - 0 1").unwrap();
        assert_eq!(insufficient_material_knights.outcome(), GameOutcome::Ongoing);
        // using FIDE rules.
    }

    #[test]
    fn fen_round_trip() {
        use crate::board::Board;
        use std::{
            fs::File,
            io::{BufRead, BufReader},
        };

        let fens = BufReader::new(File::open("epds/perftsuite.epd").unwrap())
            .lines()
            .map(|l| l.unwrap().split_once(';').unwrap().0.trim().to_owned())
            .collect::<Vec<_>>();
        let mut board = Board::new();
        for fen in fens {
            board.set_from_fen(&fen).expect("setfen failed.");
            let fen_2 = board.to_string();
            assert_eq!(fen, fen_2);
        }
    }

    #[test]
    fn scharnagl_backrank_works() {
        use super::Board;
        use crate::piece::PieceType;
        let normal_chess_arrangement = Board::get_scharnagl_backrank(518);
        assert_eq!(
            normal_chess_arrangement,
            [
                PieceType::Rook,
                PieceType::Knight,
                PieceType::Bishop,
                PieceType::Queen,
                PieceType::King,
                PieceType::Bishop,
                PieceType::Knight,
                PieceType::Rook
            ]
        );
    }

    #[test]
    fn scharnagl_full_works() {
        #![allow(clippy::similar_names)]
        use super::Board;
        let normal = Board::from_fen(Board::STARTING_FEN).unwrap();
        let frc = Board::from_frc_idx(518);
        let dfrc = Board::from_dfrc_idx(518 * 960 + 518);
        assert_eq!(normal, frc);
        assert_eq!(normal, dfrc);
    }

    #[test]
    fn castling_pseudolegality() {
        use super::Board;
        use crate::chessmove::{Move, MoveFlags};
        use crate::util::Square;
        let board = Board::from_fen("1r2k2r/2pb1pp1/2pp4/p1n5/2P4p/PP2P2P/1qB2PP1/R2QKN1R w KQk - 0 20").unwrap();
        let kingside_castle = Move::new_with_flags(Square::E1, Square::H1, MoveFlags::Castle);
        assert!(!board.is_pseudo_legal(kingside_castle));
    }

    #[test]
    fn threat_generation_white() {
        use super::Board;
        use crate::squareset::SquareSet;

        let board = Board::from_fen("3k4/8/8/5N2/8/1P6/8/K1Q1RB2 b - - 0 1").unwrap();
        assert_eq!(board.threats.all, SquareSet::from_inner(0x1454_9d56_bddd_5f3f));
    }

    #[test]
    fn threat_generation_black() {
        use super::Board;
        use crate::squareset::SquareSet;

        let board = Board::from_fen("2br1q1k/8/6p1/8/2n5/8/8/4K3 w - - 0 1").unwrap();
        assert_eq!(board.threats.all, SquareSet::from_inner(0xfcfa_bbbd_6ab9_2a28));
    }

    #[test]
    fn key_after_works_for_simple_moves() {
        use super::Board;
        use crate::chessmove::Move;
        use crate::util::Square;
        let mut board = Board::default();
        let mv = Move::new(Square::E2, Square::E3);
        let key = board.key_after(mv);
        board.make_move_simple(mv);
        assert_eq!(board.key, key);
    }

    #[test]
    fn key_after_works_for_captures() {
        use super::Board;
        use crate::chessmove::Move;
        use crate::util::Square;
        let mut board = Board::from_fen("r1bqkb1r/ppp2ppp/2n5/3np1N1/2B5/8/PPPP1PPP/RNBQK2R w KQkq - 0 6").unwrap();
        // Nxf7!!
        let mv = Move::new(Square::G5, Square::F7);
        let key = board.key_after(mv);
        board.make_move_simple(mv);
        assert_eq!(board.key, key);
    }

    #[test]
    fn key_after_works_for_nullmove() {
        use super::Board;
        let mut board = Board::default();
        let key = board.key_after_null_move();
        board.make_nullmove();
        assert_eq!(board.key, key);
    }

    #[test]
    fn ep_square_edge_case() {
        use super::Board;
        use crate::chessmove::Move;
        use crate::makemove::{hash_ep, hash_piece, hash_side};
        use crate::piece::Piece;
        use crate::util::Square;
        let mut not_ep_capturable =
            Board::from_fen("rnbqkbnr/ppppp1pp/8/5p2/4P3/5N2/PPPP1PPP/RNBQKB1R b KQkq - 1 2").unwrap();
        let mut ep_capturable = Board::from_fen("rnbqkbnr/ppppp1pp/8/4Pp2/8/8/PPPP1PPP/RNBQKBNR b KQkq - 0 2").unwrap();
        let d5 = Move::new(Square::D7, Square::D5);
        let mut not_ep_capturable_key = not_ep_capturable.key;
        let mut ep_capturable_key = ep_capturable.key;
        hash_side(&mut not_ep_capturable_key);
        hash_side(&mut ep_capturable_key);
        hash_piece(&mut not_ep_capturable_key, Piece::BP, Square::D7);
        hash_piece(&mut ep_capturable_key, Piece::BP, Square::D7);
        hash_piece(&mut not_ep_capturable_key, Piece::BP, Square::D5);
        hash_piece(&mut ep_capturable_key, Piece::BP, Square::D5);

        hash_ep(&mut ep_capturable_key, Square::D6);

        assert!(not_ep_capturable.make_move_simple(d5));
        assert!(ep_capturable.make_move_simple(d5));

        assert_eq!(not_ep_capturable.ep_sq, None);
        assert_eq!(ep_capturable.ep_sq, Some(Square::D6));

        assert_eq!(not_ep_capturable.key, not_ep_capturable_key);
        assert_eq!(ep_capturable.key, ep_capturable_key);
    }

    #[test]
    fn other_ep_edge_case() {
        use super::Board;
        use crate::chessmove::Move;
        use crate::util::Square;
        let mut board = Board::from_fen("rnbqkbnr/1ppppppp/p7/P7/8/8/1PPPPPPP/RNBQKBNR b KQkq - 0 2").unwrap();
        assert!(board.make_move_simple(Move::new(Square::B7, Square::B5)));
        assert_eq!(board.ep_sq, Some(Square::B6));
    }
}
