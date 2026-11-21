/// Removes Markdown used by agent replies while preserving link labels and plain punctuation.
pub(crate) fn spoken(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim_start();
        let line = line.trim_start_matches(['#', '>']).trim_start();
        let line = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line);
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '[' | '*' | '`' => {}
                '_' if chars.peek() == Some(&'_') => {
                    chars.next();
                }
                ']' if chars.peek() == Some(&'(') => {
                    for ch in chars.by_ref() {
                        if ch == ')' {
                            break;
                        }
                    }
                }
                ']' => {}
                ch => out.push(ch),
            }
        }
        out.push(' ');
    }
    out.trim().to_owned()
}

#[derive(Default)]
pub(crate) struct Chunker {
    text: String,
    fenced: bool,
    ticks: u8,
    emitted_first: bool,
    link: bool,
}

impl Chunker {
    pub fn append(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '`' {
                self.ticks += 1;
                if self.ticks == 3 {
                    if !self.fenced {
                        self.text.push(' ');
                    }
                    self.fenced = !self.fenced;
                    self.ticks = 0;
                }
                continue;
            }
            self.ticks = 0;
            if !self.fenced {
                self.text.push(ch);
            }
        }
    }

    pub fn take(&mut self, flush: bool) -> Option<String> {
        let limit = if self.emitted_first { 240 } else { 100 };
        let mut cut = None;
        let mut whitespace = None;
        let mut in_link = self.link;
        let mut previous = '\0';
        for (at, ch) in self.text.char_indices() {
            if previous == ']' && ch == '(' {
                in_link = true;
            }
            if in_link && ch == ')' {
                in_link = false;
            }
            if !in_link && ch.is_whitespace() {
                whitespace = Some(at);
                if matches!(previous, '.' | '!' | '?' | '\n')
                    || (matches!(previous, ',' | ':' | ';') && at >= 24 && !self.emitted_first)
                    || (previous == ',' && at >= 16 && !self.emitted_first)
                    || at >= limit
                {
                    cut = Some(at);
                    break;
                }
            }
            if at >= limit * 2 {
                cut = whitespace.or(Some(at));
                break;
            }
            previous = ch;
        }
        let ends_sentence =
            !in_link && matches!(previous, '.' | '!' | '?') && self.text.len() >= 20;
        let end = cut.or_else(|| (flush || ends_sentence).then_some(self.text.len()))?;
        if end == 0 {
            return None;
        }
        let chunk = spoken(&self.text[..end]);
        self.link = in_link;
        self.text.drain(..end);
        self.emitted_first |= !chunk.is_empty();
        Some(chunk)
    }

    pub fn buffered(&self) -> usize {
        self.text.len()
    }
}

/// Remove the same markup for one-shot speech as for streamed text.
pub fn speech_text(text: &str) -> String {
    let mut chunker = Chunker::default();
    chunker.append(text);
    spoken(&chunker.text)
}
