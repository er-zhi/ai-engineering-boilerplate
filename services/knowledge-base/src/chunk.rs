// Splits document text into passages that fit the embedder's window, breaking on lines, then sentences, then words.

pub const MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW: usize = 300;

struct Piece<'a> {
    text: &'a str,
    starts_line: bool,
}

pub fn split(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;

    for piece in pieces(text) {
        let piece_chars = len(piece.text);
        if current_chars > 0 && current_chars + 1 + piece_chars > MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW
        {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if current_chars > 0 {
            current.push(if piece.starts_line { '\n' } else { ' ' });
            current_chars += 1;
        }
        current.push_str(piece.text);
        current_chars += piece_chars;
    }
    if current_chars > 0 {
        chunks.push(current);
    }
    chunks
}

fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        for (index, piece) in fit(line).into_iter().enumerate() {
            pieces.push(Piece {
                text: piece,
                starts_line: index == 0,
            });
        }
    }
    pieces
}

fn fit(line: &str) -> Vec<&str> {
    if len(line) <= MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW {
        return vec![line];
    }
    sentences(line)
        .into_iter()
        .flat_map(|sentence| {
            if len(sentence) <= MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW {
                vec![sentence]
            } else {
                sentence.split_whitespace().flat_map(hard_split).collect()
            }
        })
        .collect()
}

fn sentences(line: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        let ends_sentence = matches!(character, '.' | '!' | '?')
            && chars.peek().is_some_and(|(_, next)| next.is_whitespace());
        if ends_sentence {
            let end = index + character.len_utf8();
            sentences.push(line[start..end].trim());
            start = end;
        }
    }
    sentences.push(line[start..].trim());
    sentences.retain(|sentence| !sentence.is_empty());
    sentences
}

fn hard_split(word: &str) -> Vec<&str> {
    let boundaries: Vec<usize> = word
        .char_indices()
        .map(|(index, _)| index)
        .step_by(MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW)
        .chain(std::iter::once(word.len()))
        .collect();
    boundaries
        .windows(2)
        .map(|bounds| &word[bounds[0]..bounds[1]])
        .collect()
}

fn len(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<&str> {
        text.split_whitespace().collect()
    }

    fn numbered_words(count: usize) -> Vec<String> {
        (0..count).map(|n| format!("w{n}")).collect()
    }

    #[test]
    fn blank_text_has_no_chunks() {
        assert!(split("  \n\n \t ").is_empty());
    }

    #[test]
    fn short_text_is_one_chunk_with_its_lines_kept() {
        assert_eq!(
            split("  First line.  \n\nSecond line."),
            ["First line.\nSecond line."]
        );
    }

    #[test]
    fn lines_are_packed_into_chunks_that_fit_keeping_every_word_in_order() {
        let lines: Vec<String> = numbered_words(900)
            .chunks(15)
            .map(|line| format!("{}.", line.join(" ")))
            .collect();
        let text = lines.join("\n");

        let chunks = split(&text);

        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| len(chunk) <= MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW)
        );
        assert_eq!(
            chunks
                .iter()
                .flat_map(|chunk| words(chunk))
                .collect::<Vec<_>>(),
            words(&text)
        );
    }

    #[test]
    fn a_line_longer_than_a_chunk_breaks_on_sentences() {
        let sentence = format!("{}.", "word ".repeat(50).trim_end());
        let line = format!("{sentence} ").repeat(10);

        let chunks = split(&line);

        assert!(
            chunks
                .iter()
                .all(|chunk| len(chunk) <= MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW)
        );
        assert!(chunks.iter().all(|chunk| chunk.ends_with('.')));
    }

    #[test]
    fn a_sentence_longer_than_a_chunk_breaks_on_words_losing_none() {
        let sentence = numbered_words(600).join(" ");

        let chunks = split(&sentence);

        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| len(chunk) <= MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW)
        );
        assert_eq!(
            chunks
                .iter()
                .flat_map(|chunk| words(chunk))
                .collect::<Vec<_>>(),
            words(&sentence)
        );
    }

    #[test]
    fn an_unbroken_run_of_text_is_cut_at_character_boundaries() {
        let run = "ё".repeat(MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW * 2 + 10);

        let chunks = split(&run);

        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| len(chunk) <= MAX_CHUNK_CHARS_IN_EMBEDDER_WINDOW)
        );
        assert_eq!(chunks.concat(), run);
    }
}
