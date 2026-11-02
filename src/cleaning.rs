//! Strip quoted history and signatures so readers/agents see only what's new.

pub fn clean_message(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut cut = lines.len();

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
        let is_quote_intro = (line.starts_with("On ") && line.ends_with("wrote:"))
            || (line.starts_with("On ") && !line.ends_with("wrote:") && next.ends_with("wrote:"));
        let is_outlook = line.starts_with("From:")
            && lines[i + 1..].iter().take(4).any(|l| {
                let l = l.trim();
                l.starts_with("Sent:") || l.starts_with("Date:") || l.starts_with("Subject:")
            });
        let is_forward = line.starts_with("---------- Forwarded message")
            || line.starts_with("-----Original Message-----")
            || line.starts_with("Begin forwarded message");
        let is_sig = *raw == "-- " || line == "--";
        let is_mobile_sig = line.starts_with("Sent from my ") || line.starts_with("Get Outlook for");
        let is_quoted_block = line.starts_with('>')
            && lines[i..].iter().all(|l| l.trim().is_empty() || l.trim_start().starts_with('>'));

        if is_quote_intro || is_outlook || is_forward || is_sig || is_mobile_sig || is_quoted_block {
            cut = i;
            break;
        }
    }

    let cleaned = lines[..cut]
        .iter()
        .filter(|l| !l.trim_start().starts_with('>'))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        body.trim().to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::clean_message;

    #[test]
    fn strips_gmail_quote() {
        let b = "Sounds good!\n\nOn Mon, Nov 2, 2026 at 9:00 AM Bob <b@x.com> wrote:\n> earlier";
        assert_eq!(clean_message(b), "Sounds good!");
    }

    #[test]
    fn strips_two_line_intro() {
        let b = "Yes.\n\nOn Mon, Nov 2, 2026 at 9:00 AM Bob Smith\n<b@x.com> wrote:\n> old";
        assert_eq!(clean_message(b), "Yes.");
    }

    #[test]
    fn strips_outlook_block() {
        let b = "Thanks\n\nFrom: Bob\nSent: Monday\nTo: me\nSubject: hi\n\nold text";
        assert_eq!(clean_message(b), "Thanks");
    }

    #[test]
    fn strips_signature() {
        assert_eq!(clean_message("Hi there\n-- \nBob\nCEO"), "Hi there");
        assert_eq!(clean_message("Ok\n\nSent from my iPhone"), "Ok");
    }

    #[test]
    fn strips_forward() {
        assert_eq!(clean_message("FYI\n---------- Forwarded message ---------\nFrom: x"), "FYI");
    }

    #[test]
    fn keeps_body_if_everything_would_vanish() {
        assert_eq!(clean_message("> only quoted"), "> only quoted");
    }
}
