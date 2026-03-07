use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
};

use serde::Deserialize;

use super::provider::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    X,
    O,
}

impl Mark {
    pub(super) fn symbol(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::O => "O",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct Board {
    cells: [Option<ProviderKind>; 9],
}

impl Board {
    pub(super) fn render_for_prompt<F>(&self, mark_for: F) -> String
    where
        F: Fn(ProviderKind) -> &'static str,
    {
        self.render(|index, occupant| match occupant {
            Some(provider) => mark_for(provider).to_owned(),
            None => (index + 1).to_string(),
        })
    }

    pub(super) fn render_final<F>(&self, mark_for: F) -> String
    where
        F: Fn(ProviderKind) -> &'static str,
    {
        self.render(|_, occupant| match occupant {
            Some(provider) => mark_for(provider).to_owned(),
            None => ".".to_owned(),
        })
    }

    pub(super) fn available_squares(&self) -> String {
        self.cells
            .iter()
            .enumerate()
            .filter(|(_, occupant)| occupant.is_none())
            .map(|(index, _)| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn apply_move(
        &mut self,
        provider: ProviderKind,
        square: usize,
    ) -> Result<(), String> {
        if !(1..=9).contains(&square) {
            return Err(format!("square {square} is outside the valid range 1-9"));
        }

        let index = square - 1;
        if let Some(occupant) = self.cells[index] {
            return Err(format!(
                "square {square} is already occupied by {}",
                occupant.label()
            ));
        }

        self.cells[index] = Some(provider);
        Ok(())
    }

    pub(super) fn winner(&self) -> Option<ProviderKind> {
        const LINES: [[usize; 3]; 8] = [
            [0, 1, 2],
            [3, 4, 5],
            [6, 7, 8],
            [0, 3, 6],
            [1, 4, 7],
            [2, 5, 8],
            [0, 4, 8],
            [2, 4, 6],
        ];

        for [a, b, c] in LINES {
            let Some(provider) = self.cells[a] else {
                continue;
            };

            if self.cells[b] == Some(provider) && self.cells[c] == Some(provider) {
                return Some(provider);
            }
        }

        None
    }

    pub(super) fn is_full(&self) -> bool {
        self.cells.iter().all(Option::is_some)
    }

    fn render<F>(&self, tile: F) -> String
    where
        F: Fn(usize, Option<ProviderKind>) -> String,
    {
        let mut rows = Vec::new();
        for row in 0..3 {
            let base = row * 3;
            rows.push(format!(
                " {} | {} | {} ",
                tile(base, self.cells[base]),
                tile(base + 1, self.cells[base + 1]),
                tile(base + 2, self.cells[base + 2]),
            ));
        }

        rows.join("\n---+---+---\n")
    }
}

#[derive(Debug, Clone)]
pub(super) struct TurnRecord {
    pub(super) number: usize,
    pub(super) player: ProviderKind,
    pub(super) model: String,
    pub(super) square: usize,
    pub(super) raw_response: String,
    pub(super) cache_summary: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) enum GameOutcome {
    Win {
        winner: ProviderKind,
        detail: String,
    },
    Draw,
}

#[derive(Debug, Deserialize)]
struct JsonMove {
    #[serde(default)]
    r#move: Option<usize>,
    #[serde(default)]
    square: Option<usize>,
    #[serde(default)]
    position: Option<usize>,
    #[serde(default)]
    cell: Option<usize>,
}

pub(super) fn parse_move(text: &str) -> Result<usize, String> {
    if let Some(square) = parse_move_token(text) {
        return Ok(square);
    }

    if let Some(square) = parse_json_move(text) {
        return Ok(square);
    }

    if let Some(inner) = strip_code_fence(text) {
        if let Some(square) = parse_move_token(inner) {
            return Ok(square);
        }

        if let Some(square) = parse_json_move(inner) {
            return Ok(square);
        }
    }

    Err(format!(
        "expected a single square number from 1 to 9, got `{text}`",
    ))
}

fn parse_move_token(text: &str) -> Option<usize> {
    let token = text
        .trim()
        .trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '`' | '"' | '\''));
    let square = token.parse::<usize>().ok()?;
    (1..=9).contains(&square).then_some(square)
}

fn parse_json_move(text: &str) -> Option<usize> {
    let move_reply = serde_json::from_str::<JsonMove>(text.trim()).ok()?;
    [
        move_reply.r#move,
        move_reply.square,
        move_reply.position,
        move_reply.cell,
    ]
    .into_iter()
    .flatten()
    .find(|square| (1..=9).contains(square))
}

fn strip_code_fence(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("```")?;
    let newline = rest.find('\n')?;
    let rest = &rest[newline + 1..];
    let suffix = rest.rfind("```")?;
    Some(rest[..suffix].trim())
}

pub(super) fn random_x_player() -> ProviderKind {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u8(1);

    if hasher.finish() & 1 == 0 {
        ProviderKind::OpenAI
    } else {
        ProviderKind::Anthropic
    }
}
