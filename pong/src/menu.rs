//! Immediate menu choices; player keys and game codes use explicit text entry.
use std::{io::{self, BufRead, IsTerminal, Write}, path::{Path, PathBuf}};
use super::terminal::PromptKey;

#[derive(Debug, PartialEq, Eq)]
pub enum Action { Host, Join }
#[derive(Debug, PartialEq, Eq)]
pub enum Prompt<T> { Selected(T), Back, Exit }

fn line(input: &mut impl BufRead, out: &mut impl Write, label: &str) -> io::Result<Option<String>> {
    write!(out, "{label}")?; out.flush()?;
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 { return Ok(None); }
    let value = value.trim().to_string();
    Ok(if value.eq_ignore_ascii_case("q") || value == "\u{1b}" { None } else { Some(value) })
}
pub fn action(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Option<Action>> {
    writeln!(out, "PONG\n\n  1  Host a new game (RIGHT)\n  2  Join a game (LEFT)\n  q  Quit\n")?;
    loop {
        let Some(value) = line(input,out,"Choose: ")? else { return Ok(None) };
        match value.to_ascii_lowercase().as_str() {
            "1" | "host" => return Ok(Some(Action::Host)),
            "2" | "join" => return Ok(Some(Action::Join)),
            _ => writeln!(out,"Choose 1, 2, or q.")?,
        }
    }
}
pub fn opponent(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Option<String>> {
    loop {
        let Some(value) = line(input,out,"Paste opponent's public key (q cancels): ")? else { return Ok(None) };
        if value.parse::<secp256k1::XOnlyPublicKey>().is_ok() { return Ok(Some(value)); }
        writeln!(out,"Expected the 64-character public player key.")?;
    }
}
pub fn code(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Option<String>> {
    loop {
        let Some(value) = line(input,out,"Game code (q cancels): ")? else { return Ok(None) };
        match super::node::invitations::parse_rendezvous_code(&value) {
            Ok(_) => return Ok(Some(value)),
            Err(error) => writeln!(out,"{error}")?,
        }
    }
}
/// Resolve only an explicitly named wallet; never inspect its private contents.
pub fn named_wallet(root: &Path, name: &str) -> io::Result<PathBuf> {
    if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return Err(io::Error::other("wallet names may contain letters, numbers, - and _; use --wallet for a file path"));
    }
    let personal = root.join("wallets").join(name).join("wallet.json");
    let existing = root.join(format!("{name}.wallet.json"));
    match (personal.is_file(),existing.is_file()) {
        (true,false) => Ok(personal), (false,true) => Ok(existing),
        (true,true) => Err(io::Error::other("wallet name is ambiguous; select its exact path with --wallet FILE")),
        _ => Err(io::Error::other(format!("wallet '{name}' not found; use --wallet FILE or wallets/{name}/wallet.json"))),
    }
}
pub fn action_key(key: PromptKey) -> Option<Prompt<Action>> {
    match key {
        PromptKey::Character('1') => Some(Prompt::Selected(Action::Host)),
        PromptKey::Character('2') => Some(Prompt::Selected(Action::Join)),
        PromptKey::Character('q' | 'Q') | PromptKey::Back => Some(Prompt::Back),
        PromptKey::Exit => Some(Prompt::Exit), _ => None,
    }
}
fn setup_notice(error: &str) -> String {
    // Keep long setup errors readable instead of truncating them to one row.
    let mut notice = String::from("Last attempt failed:\n");
    let mut column = 0;
    for ch in error.chars().take(360) {
        if ch == '\n' || column == 60 {
            notice.push('\n');
            column = 0;
            if ch == '\n' { continue; }
        }
        notice.push(ch);
        column += 1;
    }
    let shortened = notice.lines().take(7).collect::<Vec<_>>().join("\n");
    if shortened != notice || error.chars().count() > 360 {
        format!("{shortened}\n... Exit menu to view the full error.")
    } else { notice }
}
pub fn menu_frame(player: Option<&str>) -> String {
    let identity = player.map(|pk| format!("Player public key:\n{pk}\n\n")).unwrap_or_default();
    format!("PONG\n\n{identity}  1  Host a new game (RIGHT)\n  2  Join a game (LEFT)\n  q  Quit\n\nAny funded match you leave remains active and at risk of timeout.\nLeaving does not cancel it or automatically refund stakes.\n\nRelease the previous screen's key before choosing.\nPress 1, 2, or q (no Enter).")
}
pub fn interactive_action(error: Option<&str>, player: Option<&str>) -> io::Result<Option<Action>> {
    if !interactive_tty() { return action(&mut io::stdin().lock(), &mut io::stdout().lock()); }
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let notice = error.map(setup_notice);
    loop {
        let frame = format!("{}\n\n{}", menu_frame(player), super::node::hosted::status());
        terminal.draw_with_panels(&frame,None,notice.as_deref())?;
        if let Some(choice) = terminal.prompt_key(std::time::Duration::from_millis(100))?.and_then(action_key) {
            return Ok(match choice { Prompt::Selected(action) => Some(action), Prompt::Back | Prompt::Exit => None });
        }
    }
}
pub fn interactive_opponent() -> io::Result<Prompt<String>> {
    if !interactive_tty() {
        return Ok(opponent(&mut io::stdin().lock(), &mut io::stdout().lock())?.map(Prompt::Selected).unwrap_or(Prompt::Exit));
    }
    loop {
        match interactive_line("Paste opponent's 64-character public key (q/Esc: main menu):")? {
            Prompt::Selected(value) if value.parse::<secp256k1::XOnlyPublicKey>().is_ok() => return Ok(Prompt::Selected(value)),
            Prompt::Selected(_) => continue, other => return Ok(other),
        }
    }
}
pub fn interactive_code() -> io::Result<Prompt<String>> {
    if !interactive_tty() {
        return Ok(code(&mut io::stdin().lock(), &mut io::stdout().lock())?.map(Prompt::Selected).unwrap_or(Prompt::Exit));
    }
    loop {
        match interactive_line("Enter the host's game code (q/Esc: main menu):")? {
            Prompt::Selected(value) if super::node::invitations::parse_rendezvous_code(&value).is_ok() => return Ok(Prompt::Selected(value)),
            Prompt::Selected(_) => continue, other => return Ok(other),
        }
    }
}
fn interactive_line(label: &str) -> io::Result<Prompt<String>> {
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let mut draft = String::new();
    loop {
        terminal.draw_with_panels(&format!("{label}\n{draft}▏"),None,None)?;
        match terminal.prompt_text_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Back) => return Ok(Prompt::Back),
            Some(PromptKey::Exit) => return Ok(Prompt::Exit),
            Some(PromptKey::Enter) => return Ok(if draft.trim().eq_ignore_ascii_case("q") { Prompt::Back }
                else { Prompt::Selected(draft.trim().to_string()) }),
            Some(PromptKey::Backspace) => { draft.pop(); },
            Some(PromptKey::Character(ch)) => draft.push(ch), None => {},
        }
    }
}
fn interactive_tty() -> bool { io::stdin().is_terminal() && io::stdout().is_terminal() }

#[cfg(test)]
mod tests {
    #[test]
    fn main_menu_shows_the_complete_selected_player_key() {
        let pk = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let frame = super::menu_frame(Some(pk));
        assert!(frame.contains(&format!("Player public key:\n{pk}")));
        assert!(frame.contains("1  Host") && frame.contains("2  Join"));
    }
    #[test]
    fn setup_errors_remain_in_menu_banner() {
        let error = "configure invitation_index as http://HOST:PORT and run pong-node index before creating invitations; see docs/ONCHAIN_INVITATIONS.md";
        let notice = super::setup_notice(error);
        assert!(notice.starts_with("Last attempt failed:\n"));
        assert_eq!(notice.lines().skip(1).collect::<String>(), error);
        assert!(notice.lines().all(|line| line.chars().count() <= 60));
        let frame = crate::terminal::ScreenFrame::compose("PONG", None, Some(&notice), (80, 25));
        assert!(frame.rows.iter().any(|row| row.style == crate::terminal::RowStyle::Alert && row.text.contains("invitation_index")));
        assert!(super::setup_notice(&"x".repeat(4000)).lines().count() <= 8);
    }
}
