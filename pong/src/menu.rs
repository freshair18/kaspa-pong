//! Immediate menu choices; player keys and game codes use explicit text entry.
use std::{io::{self, BufRead, IsTerminal, Write}, path::{Path, PathBuf}};
use super::{engine::GameSpeed, terminal::PromptKey};

pub enum Balance { Loading, Unavailable, Coins(Vec<super::engine::Funding>) }
pub struct MenuIdentity {
    pub public_key: String,
    pub address: String,
    pub network: String,
    pub balance: std::sync::Arc<std::sync::Mutex<Balance>>,
}
pub fn format_kas(amount: u64) -> String {
    let fraction = format!("{:08}", amount % 100_000_000);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() { format!("{}", amount / 100_000_000) }
    else { format!("{}.{fraction}", amount / 100_000_000) }
}
fn balance_text(balance: &Balance, reserved: &[kaspa_consensus_core::tx::TransactionOutpoint]) -> String {
    match balance {
        Balance::Loading => "Loading...".into(),
        Balance::Unavailable => "Unavailable".into(),
        Balance::Coins(coins) => {
            let amount = coins.iter().filter(|coin| !reserved.contains(&coin.outpoint))
                .try_fold(0u64, |sum, coin| sum.checked_add(coin.utxo.amount));
            match amount {
                Some(amount) => {
                    format!("{} KAS", format_kas(amount))
                }
                None => "Unavailable".into(),
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action { Host, Join, ConfigureSpeed, Send, ConfigureFee }
#[derive(Debug, PartialEq, Eq)]
pub enum Prompt<T> { Selected(T), Back, Exit }

fn line(input: &mut impl BufRead, out: &mut impl Write, label: &str) -> io::Result<Option<String>> {
    write!(out, "{label}")?; out.flush()?;
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 { return Ok(None); }
    let value = value.trim().to_string();
    Ok(if value.eq_ignore_ascii_case("q") || value == "\u{1b}" { None } else { Some(value) })
}
pub fn action(input: &mut impl BufRead, out: &mut impl Write, speed: GameSpeed) -> io::Result<Option<Action>> {
    writeln!(out, "PONG\n\n  1  Host a new game (RIGHT)\n  2  Join a game (LEFT)\n  3  Configure speed ({})\n  4  Send to address\n  5  Configure fee policy ({})\n  q  Quit\n", speed.label(), super::node::fee_policy::policy().label())?;
    loop {
        let Some(value) = line(input,out,"Choose: ")? else { return Ok(None) };
        match value.to_ascii_lowercase().as_str() {
            "1" | "host" => return Ok(Some(Action::Host)),
            "2" | "join" => return Ok(Some(Action::Join)),
            "3" | "speed" => return Ok(Some(Action::ConfigureSpeed)),
            "4" | "send" => return Ok(Some(Action::Send)),
            "5" | "fee" => return Ok(Some(Action::ConfigureFee)),
            _ => writeln!(out,"Choose 1, 2, 3, 4, 5, or q.")?,
        }
    }
}
pub fn speed(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Option<GameSpeed>> {
    writeln!(out, "Ball speed\n\n  1  Fast\n  2  Medium\n  3  Slow\n  q  Back\n")?;
    loop {
        let Some(value) = line(input, out, "Choose: ")? else { return Ok(None) };
        match value.as_str() {
            "1" => return Ok(Some(GameSpeed::Fast)),
            "2" => return Ok(Some(GameSpeed::Medium)),
            "3" => return Ok(Some(GameSpeed::Slow)),
            _ => writeln!(out, "Choose 1, 2, 3, or q.")?,
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
fn parse_send_amount(value: &str) -> Option<Option<u64>> {
    if value.eq_ignore_ascii_case("all") { return Some(None); }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if (whole.is_empty() && fraction.is_empty()) || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit()) || fraction.len() > 8 { return None; }
    let kas = if whole.is_empty() { 0 } else { whole.parse::<u64>().ok()? };
    let fraction = if fraction.is_empty() { 0 } else { fraction.parse::<u64>().ok()? * 10u64.pow((8 - fraction.len()) as u32) };
    let amount = kas.checked_mul(100_000_000)?.checked_add(fraction)?;
    (amount > 0).then_some(Some(amount))
}
fn valid_address(value: &str) -> bool { kaspa_addresses::Address::try_from(value).is_ok() }
pub fn send_amount(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Prompt<Option<u64>>> {
    writeln!(out, "Send can spend reserved coins and interfere with a pending game or refund.")?;
    loop {
        let Some(value) = line(input, out, "Amount in KAS or all (q/Esc cancels): ")? else { return Ok(Prompt::Back) };
        if let Some(amount) = parse_send_amount(&value) { return Ok(Prompt::Selected(amount)); }
        writeln!(out, "Enter a positive amount with up to 8 decimal places, or all.")?;
    }
}
pub fn send_address(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Option<String>> {
    loop {
        let Some(value) = line(input, out, "Destination address (q/Esc cancels): ")? else { return Ok(None) };
        if valid_address(&value) { return Ok(Some(value)); }
        writeln!(out, "Enter a valid Kaspa address.")?;
    }
}
pub fn interactive_send_amount() -> io::Result<Prompt<Option<u64>>> {
    let mut label = "Send can spend reserved coins and interfere with a pending game or refund.\n\nAmount in KAS or all (q/Esc: cancel):";
    loop {
        match interactive_line(label)? {
            Prompt::Selected(value) => if let Some(amount) = parse_send_amount(&value) { return Ok(Prompt::Selected(amount)); } else { label = "Send can spend reserved coins and interfere with a pending game or refund.\n\nEnter a positive amount with up to 8 decimals, or all (q/Esc: cancel):"; },
            Prompt::Back => return Ok(Prompt::Back), Prompt::Exit => return Ok(Prompt::Exit),
        }
    }
}
pub fn interactive_send_address() -> io::Result<Prompt<String>> {
    let mut label = "Destination address (q/Esc: cancel):";
    loop {
        match interactive_line(label)? {
            Prompt::Selected(value) if valid_address(&value) => return Ok(Prompt::Selected(value)),
            Prompt::Selected(_) => { label = "Enter a valid Kaspa address (q/Esc: cancel):"; }, other => return Ok(other),
        }
    }
}
pub fn interactive_send_result(txid: &str) -> io::Result<()> {
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    loop {
        terminal.draw_with_panels(&format!("Transaction submitted.\n\nTransaction ID:\n{txid}\n\nPress Enter or Esc to return to the menu."), None, None)?;
        match terminal.prompt_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Enter | PromptKey::Back | PromptKey::Exit) => return Ok(()), _ => {}
        }
    }
}
pub fn interactive_send_preview(address: &str, amount: u64, fee: u64) -> io::Result<bool> {
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    loop {
        terminal.draw_with_panels(&format!("Send {} KAS\n\nTo:\n{address}\n\nTransaction fee: {} KAS\n\nEnter: send    q/Esc: cancel", format_kas(amount), format_kas(fee)), None, None)?;
        match terminal.prompt_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Enter) => return Ok(true),
            Some(PromptKey::Back | PromptKey::Exit | PromptKey::Character('q' | 'Q')) => return Ok(false),
            _ => {}
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
        PromptKey::Character('3') => Some(Prompt::Selected(Action::ConfigureSpeed)),
        PromptKey::Character('4') => Some(Prompt::Selected(Action::Send)),
        PromptKey::Character('5') => Some(Prompt::Selected(Action::ConfigureFee)),
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
pub fn menu_frame(player: Option<&MenuIdentity>, speed: GameSpeed) -> String {
    let identity = player.map(|player| {
        let (total, balance) = match player.balance.lock() {
            Ok(state) => {
                let total = balance_text(&state, &[]);
                let free = match super::node::reservations::reserved() {
                    Ok(reserved) => balance_text(&state, &reserved),
                    Err(_) => "Unavailable".into(),
                };
                (total, free)
            }
            Err(_) => ("Unavailable".into(), "Unavailable".into()),
        };
        format!("Player public key:\n{}\nWallet address:\n{}\nTotal balance: {total}\nFree balance (excluding reserved): {balance}\n\n", player.public_key, player.address)
    }).unwrap_or_default();
    let source = player.map(|p| super::node::fee_policy::source(&p.network)).unwrap_or("configured fallback");
    format!("PONG\n\n{identity}  1  Host a new game (RIGHT)\n  2  Join a game (LEFT)\n  3  Configure speed ({})\n  4  Send to address\n  5  Configure fee policy ({}; {source})\n  q  Quit\n\nAny funded match you leave remains active and at risk of timeout.\nLeaving does not cancel it or automatically refund stakes.\n\nRelease the previous screen's key before choosing.\nPress 1, 2, 3, 4, 5, or q (no Enter).", speed.label(), super::node::fee_policy::policy().label())
}
pub fn fee_policy(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Prompt<super::node::fee_policy::FeePolicy>> {
    use super::node::fee_policy::FeePolicy;
    writeln!(out, "Fee policy\n\n  1  Priority\n  2  Normal\n  3  Economy\n  q  Back")?;
    loop {
        let Some(value) = line(input, out, "Choose: ")? else { return Ok(Prompt::Back) };
        match value.as_str() {
            "1" => return Ok(Prompt::Selected(FeePolicy::Priority)),
            "2" => return Ok(Prompt::Selected(FeePolicy::Normal)),
            "3" => return Ok(Prompt::Selected(FeePolicy::Economy)),
            _ => writeln!(out, "Choose 1, 2, 3, or q.")?,
        }
    }
}
pub fn interactive_fee_policy() -> io::Result<Prompt<super::node::fee_policy::FeePolicy>> {
    use super::node::fee_policy::FeePolicy;
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    loop {
        terminal.draw_with_panels("Fee policy\n\n  1  Priority\n  2  Normal\n  3  Economy\n  q/Esc  Back", None, None)?;
        match terminal.prompt_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Character('1')) => return Ok(Prompt::Selected(FeePolicy::Priority)),
            Some(PromptKey::Character('2')) => return Ok(Prompt::Selected(FeePolicy::Normal)),
            Some(PromptKey::Character('3')) => return Ok(Prompt::Selected(FeePolicy::Economy)),
            Some(PromptKey::Character('q' | 'Q') | PromptKey::Back) => return Ok(Prompt::Back),
            Some(PromptKey::Exit) => return Ok(Prompt::Exit), _ => {}
        }
    }
}
pub fn interactive_action(error: Option<&str>, player: Option<&MenuIdentity>, speed: GameSpeed) -> io::Result<Option<Action>> {
    if !interactive_tty() { return action(&mut io::stdin().lock(), &mut io::stdout().lock(), speed); }
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let notice = error.map(setup_notice);
    loop {
        let frame = format!("{}\n\n{}", menu_frame(player, speed), super::node::hosted::status());
        terminal.draw_with_panels(&frame,None,notice.as_deref())?;
        if let Some(choice) = terminal.prompt_key(std::time::Duration::from_millis(100))?.and_then(action_key) {
            return Ok(match choice { Prompt::Selected(action) => Some(action), Prompt::Back | Prompt::Exit => None });
        }
    }
}
pub fn interactive_speed() -> io::Result<Prompt<GameSpeed>> {
    if !interactive_tty() {
        return Ok(speed(&mut io::stdin().lock(), &mut io::stdout().lock())?.map(Prompt::Selected).unwrap_or(Prompt::Back));
    }
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    loop {
        terminal.draw_with_panels("Ball speed\n\n  1  Fast\n  2  Medium\n  3  Slow\n  q/Esc  Back", None, None)?;
        match terminal.prompt_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Character('1')) => return Ok(Prompt::Selected(GameSpeed::Fast)),
            Some(PromptKey::Character('2')) => return Ok(Prompt::Selected(GameSpeed::Medium)),
            Some(PromptKey::Character('3')) => return Ok(Prompt::Selected(GameSpeed::Slow)),
            Some(PromptKey::Character('q' | 'Q') | PromptKey::Back) => return Ok(Prompt::Back),
            Some(PromptKey::Exit) => return Ok(Prompt::Exit),
            _ => {}
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
    use super::GameSpeed;

    #[test]
    fn send_menu_validates_whole_kas_and_cancellation() {
        use super::*;
        assert_eq!(action_key(PromptKey::Character('4')), Some(Prompt::Selected(Action::Send)));
        assert!(menu_frame(None, GameSpeed::Medium).contains("4  Send to address"));
        let mut output = Vec::new();
        assert_eq!(send_amount(&mut std::io::Cursor::new(b"0\n0.000000001\n184467440738\n1.5\n"), &mut output).unwrap(), Prompt::Selected(Some(150_000_000)));
        assert_eq!(send_amount(&mut std::io::Cursor::new(b"all\n"), &mut output).unwrap(), Prompt::Selected(None));
        assert_eq!(send_amount(&mut std::io::Cursor::new(b"\x1b\n"), &mut output).unwrap(), Prompt::Back);
        assert_eq!(parse_send_amount(".00000001"), Some(Some(1)));
        assert_eq!(parse_send_amount("1."), Some(Some(100_000_000)));
        assert_eq!(parse_send_amount("1.2.3"), None);
        assert_eq!(send_address(&mut std::io::Cursor::new(b"\x1b\n"), &mut output).unwrap(), None);
        let address = "kaspa:qp57jvcv65ee3m5kujq4k2kuf5wduy2ksnd4q7w3l7a25vzu03wxge7h5slp0";
        assert_eq!(send_address(&mut std::io::Cursor::new(format!("bad\n{address}\n")), &mut output).unwrap(), Some(address.to_string()));
    }

    #[test]
    fn main_menu_shows_the_complete_wallet_address() {
        let address = "kaspa:qp57jvcv65ee3m5kujq4k2kuf5wduy2ksnd4q7w3l7a25vzu03wxge7h5slp0";
        let player = super::MenuIdentity { public_key: "public-key".into(), address: address.into(), network: "mainnet".into(), balance: std::sync::Arc::new(std::sync::Mutex::new(super::Balance::Loading)) };
        let frame = super::menu_frame(Some(&player), GameSpeed::Medium);
        assert!(frame.contains(&format!("Wallet address:\n{address}")));
        assert!(frame.contains("Player public key:\npublic-key"));
        assert!(frame.contains("Total balance: Loading..."));
        assert!(frame.contains("Free balance (excluding reserved): Loading..."));
        assert!(frame.contains("1  Host") && frame.contains("2  Join"));
    }
    #[test]
    fn balance_subtracts_only_reserved_coins_and_keeps_fractional_kas() {
        use kaspa_consensus_core::{Hash, tx::{TransactionOutpoint, UtxoEntry}};
        let coin = |n, amount| super::super::engine::Funding {
            outpoint: TransactionOutpoint::new(Hash::from_bytes([n; 32]), 0),
            utxo: UtxoEntry::new(amount, super::super::engine::p2pk(&[2; 32]), 1, false, None),
        };
        let first = coin(1, 200_000_000);
        let second = coin(2, 125_000_001);
        let coins = super::Balance::Coins(vec![first.clone(), second]);
        assert_eq!(super::balance_text(&coins, &[]), "3.25000001 KAS");
        assert_eq!(super::balance_text(&coins, &[first.outpoint]), "1.25000001 KAS");
        assert_eq!(super::balance_text(&super::Balance::Unavailable, &[]), "Unavailable");
    }
    #[test]
    fn speed_menu_has_three_presets_and_main_menu_exposes_it() {
        let mut output = Vec::new();
        let selected = super::action(&mut std::io::Cursor::new(b"3\n"), &mut output, GameSpeed::Medium).unwrap();
        assert_eq!(selected, Some(super::Action::ConfigureSpeed));
        assert!(String::from_utf8(output).unwrap().contains("Configure speed (Medium)"));
        let mut output = Vec::new();
        assert_eq!(super::speed(&mut std::io::Cursor::new(b"1\n"), &mut output).unwrap(), Some(GameSpeed::Fast));
        assert!(String::from_utf8(output).unwrap().contains("3  Slow"));
    }
    #[test]
    fn fee_menu_selects_policy_and_allows_cancellation() {
        use super::*;
        use crate::node::fee_policy::FeePolicy;
        assert_eq!(action_key(PromptKey::Character('5')), Some(Prompt::Selected(Action::ConfigureFee)));
        assert_eq!(fee_policy(&mut std::io::Cursor::new(b"1\n"), &mut Vec::new()).unwrap(), Prompt::Selected(FeePolicy::Priority));
        assert_eq!(fee_policy(&mut std::io::Cursor::new(b"3\n"), &mut Vec::new()).unwrap(), Prompt::Selected(FeePolicy::Economy));
        assert_eq!(fee_policy(&mut std::io::Cursor::new(b"\x1b\n"), &mut Vec::new()).unwrap(), Prompt::Back);
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
