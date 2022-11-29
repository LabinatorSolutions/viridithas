use std::{io::BufRead, path::Path};

use crate::{
    board::{evaluation::parameters::EvalParams, Board},
    chessmove::Move,
    searchinfo::{SearchInfo, SearchLimit},
    threadlocal::ThreadData,
};

const CONTROL_GREEN: &str = "\u{001b}[32m";
const CONTROL_RED: &str = "\u{001b}[31m";
const CONTROL_RESET: &str = "\u{001b}[0m";

struct EpdPosition {
    fen: String,
    best_moves: Vec<Move>,
    id: String,
}

pub fn gamut(epd_path: impl AsRef<Path>, params: EvalParams, time: u64) {
    let mut board = Board::new();
    board.alloc_tables();
    board.set_eval_params(params);
    let file = std::fs::File::open(epd_path).unwrap();
    let mut reader = std::io::BufReader::new(file);

    let mut line = String::new();
    let mut positions = Vec::new();

    while reader.read_line(&mut line).expect("Got invalid UTF-8") > 0 {
        let fen = line.split_whitespace().take(4).chain(Some("1 1")).collect::<Vec<_>>();
        let fen = fen.join(" ");
        board.set_from_fen(&fen).unwrap_or_else(|err| panic!("Invalid FEN: {fen}\n - {err}"));
        let fen_out = board.fen();
        assert_eq!(fen, fen_out);
        let best_move_idx =
            line.find("bm").unwrap_or_else(|| panic!("no bestmove found in {line}"));
        let best_moves = &line[best_move_idx + 3..];
        let end_of_best_moves =
            best_moves.find(';').unwrap_or_else(|| panic!("no end of bestmove found in {line}"));
        let best_moves = &best_moves[..end_of_best_moves].split(' ').collect::<Vec<_>>();
        let best_moves = best_moves
            .iter()
            .map(|best_move| {
                board
                    .parse_san(best_move)
                    .unwrap_or_else(|err| panic!("invalid bestmove: {best_move}, {err}"))
            })
            .collect::<Vec<_>>();
        let id_idx = line.find("id").unwrap_or_else(|| panic!("no id found in {line}"));
        let id = line[id_idx + 4..]
            .split(|c| c == '"')
            .next()
            .unwrap_or_else(|| panic!("no id found in {line}"))
            .to_string();
        positions.push(EpdPosition { fen, best_moves, id });
        line.clear();
    }

    let n_positions = positions.len();
    println!("successfully parsed {n_positions} positions!");

    let successes = run_on_positions(positions, board, time);

    println!("{}/{} passed", successes, n_positions);
}

fn run_on_positions(positions: Vec<EpdPosition>, mut board: Board, time: u64) -> i32 {
    let mut thread_data = vec![ThreadData::new()];
    let mut successes = 0;
    let maxfenlen = positions.iter().map(|pos| pos.fen.len()).max().unwrap();
    let maxidlen = positions.iter().map(|pos| pos.id.len()).max().unwrap();
    for EpdPosition { fen, best_moves, id } in positions {
        board.set_from_fen(&fen).unwrap();
        board.clear_tt();
        for t in &mut thread_data {
            t.nnue.refresh_acc(&board);
            t.alloc_tables();
        }
        
        let mut info = SearchInfo {
            print_to_stdout: false,
            limit: SearchLimit::TimeOrCorrectMoves(time, best_moves.clone()),
            ..SearchInfo::default()
        };
        let (_, bm) = board.search_position::<true>(&mut info, &mut thread_data);
        let passed = best_moves.contains(&bm);
        let color = if passed { CONTROL_GREEN } else { CONTROL_RED };
        let failinfo = if passed { String::new() } else { format!(", {CONTROL_RED}program chose {bm}{CONTROL_RESET}") };
        let move_strings = best_moves.iter().map(
            |&m| if m == bm { format!("{CONTROL_GREEN}{m}{CONTROL_RESET}") } else { m.to_string() }
        ).collect::<Vec<_>>().join(", ");
        println!(
            "{id:midl$} {color}{}{CONTROL_RESET} {fen:mfl$} [{move_strings}]{failinfo}",
            if passed { "PASS" } else { "FAIL" },
            midl = maxidlen,
            mfl = maxfenlen,
        );
        if passed {
            successes += 1;
        }
    }
    successes
}
