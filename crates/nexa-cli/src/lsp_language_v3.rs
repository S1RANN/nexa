use nexa_syntax::{
    Keyword, Lexed, LineColumn, LineIndex, TextEncoding, TextSize, TokenKind, lex_nexa,
};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
struct Api {
    owner: &'static str,
    label: &'static str,
    detail: &'static str,
    docs: &'static str,
}

const TYPES: &[(&str, &str)] = &[
    (
        "Array",
        "A growable, index-ordered collection. Dynamic for yields element values.",
    ),
    (
        "Buffer",
        "A fixed-length, index-ordered collection. Dynamic for yields element values.",
    ),
    (
        "Map",
        "A hash map with unspecified iteration order. Dynamic for yields (key, value) pairs.",
    ),
    (
        "Set",
        "A hash set of distinct elements with unspecified iteration order.",
    ),
];

const APIS: &[Api] = &[
    Api {
        owner: "Set",
        label: "new",
        detail: "fn new() -> Set<T>",
        docs: "Creates an empty Set<T>.",
    },
    Api {
        owner: "Set",
        label: "len",
        detail: "fn len() -> i32",
        docs: "Returns the number of distinct elements.",
    },
    Api {
        owner: "Set",
        label: "contains",
        detail: "fn contains(value: T) -> bool",
        docs: "Returns true when the set contains value.",
    },
    Api {
        owner: "Set",
        label: "insert",
        detail: "fn insert(value: T) -> bool",
        docs: "Inserts value and returns true only when it was absent.",
    },
    Api {
        owner: "Set",
        label: "remove",
        detail: "fn remove(value: T) -> bool",
        docs: "Removes value and returns true only when it was present.",
    },
    Api {
        owner: "Set",
        label: "clear",
        detail: "fn clear()",
        docs: "Removes every element. Clearing an empty set is a no-op.",
    },
    Api {
        owner: "Array",
        label: "first",
        detail: "fn first() -> Option<T>",
        docs: "Returns the first element, or None when the array is empty.",
    },
    Api {
        owner: "Array",
        label: "last",
        detail: "fn last() -> Option<T>",
        docs: "Returns the last element, or None when the array is empty.",
    },
    Api {
        owner: "Array",
        label: "swap",
        detail: "fn swap(a: i32, b: i32) -> bool",
        docs: "Swaps two elements and traps when either index is out of bounds.",
    },
    Api {
        owner: "Array",
        label: "reverse",
        detail: "fn reverse() -> bool",
        docs: "Reverses the array in place.",
    },
    Api {
        owner: "Map",
        label: "is_empty",
        detail: "fn is_empty() -> bool",
        docs: "Returns true when the map has no entries.",
    },
    Api {
        owner: "Map",
        label: "get_or",
        detail: "fn get_or(key: K, default: V) -> V",
        docs: "Returns the stored value, or default when key is absent.",
    },
    Api {
        owner: "Map",
        label: "insert_if_absent",
        detail: "fn insert_if_absent(key: K, value: V) -> bool",
        docs: "Inserts only when key is absent and reports whether insertion occurred.",
    },
    Api {
        owner: "Buffer",
        label: "is_empty",
        detail: "fn is_empty() -> bool",
        docs: "Returns true when the buffer has zero elements.",
    },
    Api {
        owner: "Buffer",
        label: "fill",
        detail: "fn fill(value: T) -> bool",
        docs: "Replaces every buffer element with value.",
    },
];

pub(super) fn completion_items(source: &str, offset: usize) -> Vec<Value> {
    if let Some((receiver, is_static)) = completion_receiver(source, offset.min(source.len())) {
        let owner = if is_static {
            type_name(receiver)
        } else {
            declared_owner(source, offset, receiver)
        };
        return owner
            .into_iter()
            .flat_map(|owner| APIS.iter().filter(move |api| api.owner == owner))
            .filter(|api| (is_static && api.label == "new") || (!is_static && api.label != "new"))
            .map(completion_item)
            .collect();
    }

    TYPES
        .iter()
        .map(|(label, docs)| {
            json!({
                "label": label,
                "kind": 7,
                "detail": format!("{label}<...>"),
                "documentation": docs
            })
        })
        .chain(APIS.iter().map(completion_item))
        .collect()
}

fn completion_item(api: &Api) -> Value {
    json!({
        "label": api.label,
        "kind": 2,
        "detail": format!("{} · {}", api.owner, api.detail),
        "documentation": api.docs
    })
}

fn completion_receiver(source: &str, offset: usize) -> Option<(&str, bool)> {
    let prefix = source.get(..offset)?;
    let mut cursor = prefix.len();
    while cursor > 0 {
        let character = prefix[..cursor].chars().next_back()?;
        if is_word(character) {
            cursor -= character.len_utf8();
        } else {
            break;
        }
    }
    let before_partial = prefix[..cursor].trim_end();
    let (before_receiver, is_static) = if let Some(value) = before_partial.strip_suffix("::") {
        (value, true)
    } else if let Some(value) = before_partial.strip_suffix('.') {
        (value, false)
    } else {
        return None;
    };
    final_word(before_receiver).map(|receiver| (receiver, is_static))
}

fn final_word(source: &str) -> Option<&str> {
    let source = source.trim_end();
    let end = source.len();
    let start = source
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            (!is_word(character)).then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    (start < end).then_some(&source[start..end])
}

fn type_name(name: &str) -> Option<&'static str> {
    TYPES
        .iter()
        .find_map(|(candidate, _)| (*candidate == name).then_some(*candidate))
}

fn declared_owner(source: &str, offset: usize, receiver: &str) -> Option<&'static str> {
    let lexed = lex_nexa(source.get(..offset)?).ok()?;
    let significant = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let mut owner = None;
    for window in significant.windows(3) {
        if window[0].kind != TokenKind::Identifier
            || lexed.source.slice(window[0].range)? != receiver
            || window[1].kind != TokenKind::Colon
            || window[2].kind != TokenKind::Identifier
        {
            continue;
        }
        owner = type_name(lexed.source.slice(window[2].range)?);
    }
    owner
}

pub(super) fn hover(source: &str, offset: usize) -> Option<Value> {
    let (start, end) = word_range(source, offset)?;
    let word = source.get(start..end)?;
    let contents = TYPES
        .iter()
        .find_map(|(name, docs)| (*name == word).then(|| format!("`{name}<...>`\n\n{docs}")))
        .or_else(|| {
            let owner = receiver_before_word(source, start)
                .and_then(|name| declared_owner(source, start, name));
            APIS.iter()
                .find(|api| api.label == word && owner.is_none_or(|owner| api.owner == owner))
                .map(|api| {
                    format!(
                        "`{}::{}` — `{}`\n\n{}",
                        api.owner, api.label, api.detail, api.docs
                    )
                })
        })?;
    let start = utf16_position(source, start)?;
    let end = utf16_position(source, end)?;
    Some(json!({
        "contents": {"kind": "markdown", "value": contents},
        "range": {
            "start": {"line": start.line, "character": start.column},
            "end": {"line": end.line, "character": end.column}
        }
    }))
}

fn receiver_before_word(source: &str, word_start: usize) -> Option<&str> {
    final_word(source.get(..word_start)?.trim_end().strip_suffix('.')?)
}

fn word_range(source: &str, offset: usize) -> Option<(usize, usize)> {
    let mut offset = offset.min(source.len());
    while !source.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }
    if offset == source.len() || !source[offset..].chars().next().is_some_and(is_word) {
        let previous = source.get(..offset)?.chars().next_back()?;
        if is_word(previous) {
            offset -= previous.len_utf8();
        } else {
            return None;
        }
    }
    let mut start = offset;
    while let Some(character) = source.get(..start)?.chars().next_back() {
        if !is_word(character) {
            break;
        }
        start -= character.len_utf8();
    }
    let mut end = offset;
    while let Some(character) = source.get(end..)?.chars().next() {
        if !is_word(character) {
            break;
        }
        end += character.len_utf8();
    }
    Some((start, end))
}

fn is_word(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn utf16_position(source: &str, offset: usize) -> Option<LineColumn> {
    let source_text = nexa_syntax::SourceText::new(source).ok()?;
    LineIndex::new(&source_text).line_column(
        TextSize::new(u32::try_from(offset).ok()?),
        TextEncoding::Utf16,
    )
}

pub(super) fn semantic_tokens(source: &str) -> Vec<u32> {
    let Ok(lexed) = lex_nexa(source) else {
        return Vec::new();
    };
    let line_index = LineIndex::new(&lexed.source);
    let significant = lexed
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !token.kind.is_trivia())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let mut absolute = Vec::<(u32, u32, u32, u32)>::new();
    for (index, token) in lexed.tokens.iter().enumerate() {
        let Some(token_type) = semantic_token_type(&lexed, &significant, index) else {
            continue;
        };
        let start = token.range.start.to_usize();
        let end = token.range.end.to_usize();
        let Some(first_line) = line_index
            .line_column(token.range.start, TextEncoding::Utf16)
            .map(|position| position.line)
        else {
            continue;
        };
        let Some(last_line) = line_index
            .line_column(token.range.end, TextEncoding::Utf16)
            .map(|position| position.line)
        else {
            continue;
        };
        for line in first_line..=last_line {
            let Some(line_start) = line_index.line_start(line) else {
                continue;
            };
            let Some(line_end) = line_index.line_end(line) else {
                continue;
            };
            let segment_start = start.max(line_start.to_usize());
            let segment_end = end.min(line_end.to_usize());
            if segment_start >= segment_end {
                continue;
            }
            let Some(column) = line_index
                .line_column(
                    TextSize::new(u32::try_from(segment_start).unwrap_or(u32::MAX)),
                    TextEncoding::Utf16,
                )
                .map(|position| position.column)
            else {
                continue;
            };
            let length = u32::try_from(source[segment_start..segment_end].encode_utf16().count())
                .unwrap_or(u32::MAX);
            absolute.push((line, column, length, token_type));
        }
    }

    let mut encoded = Vec::with_capacity(absolute.len() * 5);
    let mut previous_line = 0;
    let mut previous_column = 0;
    for (line, column, length, token_type) in absolute {
        let delta_line = line.saturating_sub(previous_line);
        let delta_column = if delta_line == 0 {
            column.saturating_sub(previous_column)
        } else {
            column
        };
        encoded.extend([delta_line, delta_column, length, token_type, 0]);
        previous_line = line;
        previous_column = column;
    }
    encoded
}

fn semantic_token_type(lexed: &Lexed, significant: &[usize], index: usize) -> Option<u32> {
    let token = lexed.tokens[index];
    match token.kind {
        TokenKind::Keyword(_) => Some(0),
        TokenKind::StringStart
        | TokenKind::StringText
        | TokenKind::StringEnd
        | TokenKind::InterpolationStart
        | TokenKind::InterpolationEnd
        | TokenKind::Rune => Some(4),
        TokenKind::Integer | TokenKind::Float => Some(5),
        TokenKind::LineComment | TokenKind::DocComment | TokenKind::BlockComment => Some(6),
        TokenKind::Identifier => {
            let position = significant.iter().position(|candidate| *candidate == index);
            let previous = position
                .and_then(|position| position.checked_sub(1))
                .and_then(|position| significant.get(position))
                .map(|index| lexed.tokens[*index].kind);
            let next = position
                .and_then(|position| significant.get(position + 1))
                .map(|index| lexed.tokens[*index].kind);
            let text = lexed.source.slice(token.range)?;
            if previous == Some(TokenKind::Keyword(Keyword::Fn)) || next == Some(TokenKind::LParen)
            {
                Some(2)
            } else if text.chars().next().is_some_and(char::is_uppercase) {
                Some(1)
            } else {
                Some(3)
            }
        }
        TokenKind::Plus
        | TokenKind::Minus
        | TokenKind::Star
        | TokenKind::Slash
        | TokenKind::Percent
        | TokenKind::PlusEqual
        | TokenKind::MinusEqual
        | TokenKind::StarEqual
        | TokenKind::SlashEqual
        | TokenKind::PercentEqual
        | TokenKind::Equal
        | TokenKind::EqualEqual
        | TokenKind::Bang
        | TokenKind::BangEqual
        | TokenKind::Less
        | TokenKind::LessEqual
        | TokenKind::Greater
        | TokenKind::GreaterEqual
        | TokenKind::AmpAmp
        | TokenKind::PipePipe
        | TokenKind::Question
        | TokenKind::Arrow
        | TokenKind::FatArrow
        | TokenKind::Dot
        | TokenKind::DotDot
        | TokenKind::ColonColon => Some(7),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn receiver_completion_is_narrowed_to_declared_collection_type() {
        let source = "fn run() { let values: Set<i32> = Set::new(); values. }";
        let offset = source.find("values. }").expect("receiver") + "values.".len();
        let items = super::completion_items(source, offset);
        let labels = items
            .iter()
            .map(|item| item["label"].as_str().expect("label"))
            .collect::<Vec<_>>();
        assert_eq!(labels, ["len", "contains", "insert", "remove", "clear"]);
    }

    #[test]
    fn hover_and_semantic_tokens_use_utf16_columns() {
        let source = "let marker: string = \"😀\"; let values: Set<i32> = Set::new();";
        let start = source.find("Set<i32>").expect("Set type");
        let hover = super::hover(source, start + 1).expect("Set hover");
        assert_eq!(hover["range"]["start"]["character"], 39);
        assert_eq!(hover["range"]["end"]["character"], 42);

        let data = super::semantic_tokens(source);
        assert_eq!(data.len() % 5, 0);
        let mut line = 0_u32;
        let mut column = 0_u32;
        let mut found = false;
        for token in data.chunks_exact(5) {
            line += token[0];
            column = if token[0] == 0 {
                column + token[1]
            } else {
                token[1]
            };
            if line == 0 && column == 39 && token[2] == 3 && token[3] == 1 {
                found = true;
            }
        }
        assert!(found, "Set semantic token must start at UTF-16 column 39");
    }
}
