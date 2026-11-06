//! Autocorrect for the composer, in two layers: a built-in list of common typos fixed the
//! moment a word ends (instant, offline), and Claude's corrections a beat after you pause.
//! Both only fix spelling, capitals and apostrophes — never wording.

use crate::assistant::Fix;

/// How long typing has to pause before Claude takes a look.
pub const PAUSE_MS: u128 = 1200;
/// How long corrected words stay highlighted.
pub const FLASH_MS: u128 = 3000;

/// Common slips, fixed as soon as the word ends. Lowercase; capitals are kept.
const TYPOS: &[(&str, &str)] = &[
    ("teh", "the"), ("hte", "the"), ("adn", "and"), ("nad", "and"), ("taht", "that"), ("thta", "that"),
    ("waht", "what"), ("wiht", "with"), ("whit", "with"), ("jsut", "just"), ("yuo", "you"), ("yoru", "your"),
    ("becuase", "because"), ("becasue", "because"), ("beacuse", "because"), ("recieve", "receive"),
    ("recieved", "received"), ("beleive", "believe"), ("acheive", "achieve"), ("wierd", "weird"),
    ("freind", "friend"), ("thier", "their"), ("alot", "a lot"), ("definately", "definitely"),
    ("seperate", "separate"), ("occured", "occurred"), ("untill", "until"), ("tommorow", "tomorrow"),
    ("tomorow", "tomorrow"), ("tommorrow", "tomorrow"), ("calender", "calendar"), ("goverment", "government"),
    ("wich", "which"), ("whihc", "which"), ("knwo", "know"), ("konw", "know"), ("thnaks", "thanks"),
    ("thansk", "thanks"), ("pleae", "please"), ("plese", "please"), ("shoudl", "should"), ("woudl", "would"),
    ("coudl", "could"), ("dont", "don't"), ("doesnt", "doesn't"), ("didnt", "didn't"), ("cant", "can't"),
    ("wont", "won't"), ("isnt", "isn't"), ("wasnt", "wasn't"), ("arent", "aren't"), ("havent", "haven't"),
    ("hasnt", "hasn't"), ("couldnt", "couldn't"), ("wouldnt", "wouldn't"), ("shouldnt", "shouldn't"),
    ("thats", "that's"), ("theyre", "they're"), ("youre", "you're"), ("im", "I'm"), ("ive", "I've"),
    ("i", "I"), ("i'm", "I'm"), ("i've", "I've"), ("i'll", "I'll"), ("i'd", "I'd"),
    ("monday", "Monday"), ("tuesday", "Tuesday"), ("wednesday", "Wednesday"), ("thursday", "Thursday"),
    ("friday", "Friday"), ("saturday", "Saturday"), ("sunday", "Sunday"), ("wendesday", "Wednesday"),
    ("wednsday", "Wednesday"), ("firday", "Friday"), ("saterday", "Saturday"),
];

pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '’'
}

/// Called right after a word-ending character was typed (space, punctuation, newline).
/// Fixes the word just finished if it's a known typo; returns (typo, fix).
pub fn fix_last_word(body: &mut String) -> Option<(String, String)> {
    let end = body.char_indices().last().map(|(i, _)| i)?; // the word-ending char
    let start = body[..end].char_indices().rev().take_while(|(_, c)| is_word_char(*c)).last().map(|(i, _)| i)?;
    let word = &body[start..end];
    // Leave anything that's part of an address, link or [[gap]].
    let before = body[..start].chars().last();
    if matches!(before, Some('@' | '/' | '.' | '[' | '{' | '_' | '-')) {
        return None;
    }
    let lower = word.to_lowercase();
    let (_, fix) = TYPOS.iter().find(|(t, _)| *t == lower)?;
    let fixed = if word.chars().next().is_some_and(char::is_uppercase) { capitalize(fix) } else { fix.to_string() };
    if fixed == word {
        return None;
    }
    let typo = word.to_string();
    body.replace_range(start..end, &fixed);
    Some((typo, fixed))
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The part of the body worth checking: everything above forwarded or quoted history.
pub fn checkable(body: &str) -> &str {
    let mut pos = 0;
    for line in body.split_inclusive('\n') {
        let t = line.trim_end();
        if t.starts_with("---------- Forwarded message") || t.starts_with('>') || (t.starts_with("On ") && t.ends_with("wrote:")) {
            return &body[..pos];
        }
        pos += line.len();
    }
    body
}

/// Apply Claude's `fixes` (made against `checked`) to the current `body`. Only the part the
/// user hasn't changed since is touched, and never a word that's still being typed.
/// Returns the new body and the replacements actually made.
pub fn apply(checked: &str, body: &str, fixes: &[Fix], rejected: &[String]) -> (String, Vec<(String, String)>) {
    let mut l = checked.char_indices().zip(body.char_indices()).take_while(|((_, a), (_, b))| a == b).last().map(|((i, c), _)| i + c.len_utf8()).unwrap_or(0);
    // If a word runs across the edge of the unchanged part, keep out of it.
    let next = body[l..].chars().next();
    let prev = body[..l].chars().last();
    if next.is_some_and(is_word_char) && prev.is_some_and(is_word_char) {
        l = body[..l].char_indices().rev().find(|(_, c)| !is_word_char(*c)).map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
    }
    let (mut region, rest) = (body[..l].to_string(), &body[l..]);
    let mut made = vec![];
    for f in fixes {
        let (from, to) = (f.from.as_str(), f.to.as_str());
        if from.trim().is_empty() || from == to || from.contains("[[") || from.contains("{{") || rejected.iter().any(|r| r == from) {
            continue;
        }
        if let Some(new) = replace_words(&region, from, to) {
            region = new;
            made.push((from.to_string(), to.to_string()));
        }
    }
    (region + rest, made)
}

/// Replace whole-word occurrences of `from`; None when there are none.
fn replace_words(text: &str, from: &str, to: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut hit = false;
    while let Some(pos) = text[i..].find(from) {
        let s = i + pos;
        let e = s + from.len();
        let left_ok = !text[..s].chars().last().is_some_and(is_word_char);
        let right_ok = !text[e..].chars().next().is_some_and(is_word_char);
        out.push_str(&text[i..s]);
        if left_ok && right_ok {
            out.push_str(to);
            hit = true;
        } else {
            out.push_str(from);
        }
        i = e;
    }
    out.push_str(&text[i..]);
    hit.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(from: &str, to: &str) -> Fix {
        Fix { from: from.into(), to: to.into() }
    }

    #[test]
    fn instant_typos_keep_case_and_skip_addresses() {
        let mut b = "Teh ".to_string();
        assert_eq!(fix_last_word(&mut b), Some(("Teh".into(), "The".into())));
        assert_eq!(b, "The ");
        let mut b = "so i ".to_string();
        fix_last_word(&mut b);
        assert_eq!(b, "so I ");
        let mut b = "mail bob@teh ".to_string();
        assert_eq!(fix_last_word(&mut b), None);
        let mut b = "fine ".to_string();
        assert_eq!(fix_last_word(&mut b), None);
    }

    #[test]
    fn claude_fixes_only_touch_unchanged_complete_words() {
        let checked = "the sesion on firday";
        // User kept typing "firdayy" → the half-typed word is left alone.
        let (b, made) = apply(checked, "the sesion on firdayy", &[fix("sesion", "session"), fix("firday", "Friday")], &[]);
        assert_eq!(b, "the session on firdayy");
        assert_eq!(made.len(), 1);
        // User typed past it with a space → the finished word is fixed, new text kept.
        let (b, _) = apply(checked, "the sesion on firday at 3", &[fix("firday", "Friday")], &[]);
        assert_eq!(b, "the sesion on Friday at 3");
        // Whole words only, and undone fixes stay undone.
        let (b, _) = apply("an ant", "an ant", &[fix("an", "a")], &["an".into()]);
        assert_eq!(b, "an ant");
        let (b, _) = apply("an ant", "an ant", &[fix("an", "a")], &[]);
        assert_eq!(b, "a ant");
    }

    #[test]
    fn quoted_history_is_not_checked() {
        let body = "Sounds good\n\n---------- Forwarded message ---------\nFrom: x";
        assert_eq!(checkable(body), "Sounds good\n\n");
        assert_eq!(checkable("Yes!\nOn Tue, Sam wrote:\n> hi"), "Yes!\n");
        assert_eq!(checkable("On it now\nthanks"), "On it now\nthanks");
    }
}
