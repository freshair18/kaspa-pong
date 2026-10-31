//! Immediate menu choices; player keys and game codes use explicit text entry.
use std::{io::{self, BufRead, IsTerminal, Write}, path::{Path, PathBuf}};
use super::{engine::GameSpeed, terminal::PromptKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameSettings {
    pub speed: GameSpeed,
    pub ball_count: u8,
    /// Zero selects the default winning score for the ball count.
    pub goal_limit: u8,
    pub stake_kas: u8,
}
impl Default for GameSettings {
    fn default() -> Self { Self { speed: GameSpeed::Medium, ball_count: 1, goal_limit: 0, stake_kas: 5 } }
}
impl GameSettings {
    pub fn validate(self) -> super::engine::Result<()> {
        if !(1..=15).contains(&self.speed.tick_daa()) {
            return Err("game speed requires 1–15 DAA per tick".into());
        }
        if !(1..=8).contains(&self.ball_count) || self.goal_limit > 15 {
            return Err("game settings require 1–8 balls and winning goals from 0 (Auto) to 15".into());
        }
        if !(5..=254).contains(&self.stake_kas) { return Err("wager must be 5–254 whole KAS per player".into()); }
        Ok(())
    }
    pub fn win_score(self) -> u8 {
        if self.goal_limit != 0 { return self.goal_limit; }
        super::engine::default_win_score(i64::from(self.ball_count)) as u8
    }
    pub fn terms(self) -> crate::node::invitations::GameTerms {
        crate::node::invitations::GameTerms {
            speed: self.speed, ball_count: self.ball_count, win_score: self.win_score(), stake_kas: self.stake_kas,
        }
    }
    fn goals_label(self) -> String {
        if self.goal_limit == 0 { format!("First to {} goals · Auto", self.win_score()) }
        else { format!("First to {} goals", self.goal_limit) }
    }
    fn summary(self) -> String {
        format!("{}, {} ball{}, {}", self.speed.label(), self.ball_count,
            if self.ball_count == 1 { "" } else { "s" }, self.goals_label())
    }
}

pub enum Balance { Loading, Unavailable, Coins(Vec<super::engine::Funding>) }
pub struct MenuIdentity {
    pub wallet_name: String,
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
pub enum Action { Host, Join, Configure, Send, ConfigureFee, ConfigurePlayback, ConfigureBroadcast, ConfigureAutoAccept }
#[derive(Debug, PartialEq, Eq)]
pub enum Prompt<T> { Selected(T), Back, Exit }

fn line(input: &mut impl BufRead, out: &mut impl Write, label: &str) -> io::Result<Option<String>> {
    write!(out, "{label}")?; out.flush()?;
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 { return Ok(None); }
    let value = value.trim().to_string();
    Ok(if value.eq_ignore_ascii_case("q") || value == "\u{1b}" { None } else { Some(value) })
}
pub fn action(input: &mut impl BufRead, out: &mut impl Write, settings: GameSettings, broadcast_nodes: usize) -> io::Result<Option<Action>> {
    let mut page = MenuPage::Main;
    loop {
        writeln!(out, "{}", navigation_frame(page, None, settings, broadcast_nodes))?;
        let Some(value) = line(input, out, "Choose: ")? else {
            if page == MenuPage::Main { return Ok(None); }
            page = page.parent();
            if input.fill_buf()?.is_empty() { return Ok(None); }
            continue;
        };
        let key = match value.to_ascii_lowercase().as_str() {
            "host" => '1', "join" => '2', "configure" => '3',
            value if value.chars().count() == 1 => value.chars().next().unwrap(),
            _ => { writeln!(out, "Choose one of the listed options.")?; continue; }
        };
        let previous = page;
        match navigation_key(&mut page, PromptKey::Character(key)) {
            Some(Prompt::Selected(action)) => return Ok(Some(action)),
            Some(Prompt::Back | Prompt::Exit) => return Ok(None),
            None if page == previous => writeln!(out, "Choose one of the listed options.")?,
            None => {}
        }
    }
}

pub fn speed(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Option<GameSpeed>> {
    let mut settings = GameSettings::default();
    let mut page = ConfigurePage::Speed;
    loop {
        writeln!(out, "{}", configure_frame(settings, page))?;
        let Some(value) = line(input, out, "Choose: ")? else {
            if page == ConfigurePage::ManualSpeed { page = ConfigurePage::Speed; continue; }
            return Ok(None);
        };
        let mut chars = value.chars();
        if let Some(ch) = chars.next() {
            if chars.next().is_none() {
                configure_key(&mut settings, &mut page, PromptKey::Character(ch));
                if page == ConfigurePage::Overview { return Ok(Some(settings.speed)); }
            }
        }
    }
}
fn speed_frame() -> &'static str {
    "Ball speed\nChoose a preset; the selected speed appears in Game settings.\n\n  1  Super Fast\n  2  Fast\n  3  Medium\n  4  Slow\n  0  Manual configure\n  Esc/q  Back · Number: choose (no Enter)"
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigurePage { Overview, Speed, ManualSpeed, Balls, Goals, Wager }
fn configure_frame(settings: GameSettings, page: ConfigurePage) -> String {
    match page {
        ConfigurePage::Overview => format!("Game settings\nUsed when hosting; joining uses the host's settings.\nChanges apply immediately for this session.\n\n  1  Speed ({})\n  2  Ball count ({})\n  3  First to win ({})\n  4  Wager ({} KAS per player)\n  Esc/q  Back · Number: choose (no Enter)", settings.speed.label(), settings.ball_count, settings.goals_label(), settings.stake_kas),
        ConfigurePage::Speed => format!("{}\nSelected: {}", speed_frame(), settings.speed.label()),
        ConfigurePage::ManualSpeed => "Manual speed\nLower values move the balls faster.\n\n  1–9  DAA ticks per ball step\n  Esc/q  Back · Number: choose (no Enter)".into(),
        ConfigurePage::Balls => format!("Ball count (selected {})\n\n  1  One ball\n  2  Two balls\n  3  Three balls\n  4  Four balls\n  5  Five balls\n  6  Six balls\n  Esc/q  Back · Number: choose (no Enter)", settings.ball_count),
        ConfigurePage::Wager => format!("Wager per player (selected {} KAS)\n\nEnter 6–254 whole KAS, or 5 for the default.\nBoth players stake the same amount; the winner receives the pot.\nNetwork fees are additional.\n\nEnter: apply · Esc/q: back", settings.stake_kas),
        ConfigurePage::Goals => {
            let auto = format!("Auto · First to {} goals", GameSettings { goal_limit: 0, ..settings }.win_score());
            format!("First to win ({})\n\n  0  {auto}\n  1–9  Goals to win\n  Esc/q  Back · Number: choose (no Enter)", settings.goals_label())
        }
    }
}
fn configure_key(settings: &mut GameSettings, page: &mut ConfigurePage, key: PromptKey) -> Option<Prompt<()>> {
    match key {
        PromptKey::Exit => return Some(Prompt::Exit),
        PromptKey::Back | PromptKey::Character('q' | 'Q' | '\u{1b}') => {
            if *page == ConfigurePage::Overview { return Some(Prompt::Back); }
            *page = if *page == ConfigurePage::ManualSpeed { ConfigurePage::Speed } else { ConfigurePage::Overview };
        }
        PromptKey::Character(ch) => match *page {
            ConfigurePage::Overview => match ch {
                '1' => *page = ConfigurePage::Speed,
                '2' => *page = ConfigurePage::Balls,
                '3' => *page = ConfigurePage::Goals,
                '4' => *page = ConfigurePage::Wager,
                _ => {}
            },
            ConfigurePage::Speed if ch == '0' => *page = ConfigurePage::ManualSpeed,
            ConfigurePage::Speed => {
                let speed = match ch { '1' => GameSpeed::SuperFast, '2' => GameSpeed::Fast,
                    '3' => GameSpeed::Medium, '4' => GameSpeed::Slow, _ => return None };
                settings.speed = speed;
                *page = ConfigurePage::Overview;
            }
            ConfigurePage::ManualSpeed if ch.to_digit(16).is_some_and(|n| n > 0) => {
                settings.speed = GameSpeed::from_tick_daa(i64::from(ch.to_digit(16).unwrap())).unwrap();
                *page = ConfigurePage::Overview;
            }
            ConfigurePage::Balls if ('1'..='8').contains(&ch) => {
                settings.ball_count = ch as u8 - b'0';
                *page = ConfigurePage::Overview;
            }
            ConfigurePage::Goals if ch.is_ascii_hexdigit() => {
                settings.goal_limit = ch.to_digit(16).unwrap() as u8;
                *page = ConfigurePage::Overview;
            }
            _ => {}
        },
        _ => {}
    }
    None
}
pub fn configure(input: &mut impl BufRead, out: &mut impl Write, settings: &mut GameSettings) -> io::Result<Prompt<()>> {
    settings.validate().map_err(|error| io::Error::other(error.to_string()))?;
    let mut page = ConfigurePage::Overview;
    loop {
        writeln!(out, "{}\n", configure_frame(*settings, page))?;
        write!(out, "Choose: ")?; out.flush()?;
        let mut value = String::new();
        if input.read_line(&mut value)? == 0 { return Ok(Prompt::Exit); }
        if page == ConfigurePage::Wager {
            if let Some(stake) = parse_wager(value.trim()) {
                settings.stake_kas = stake; page = ConfigurePage::Overview; continue;
            }
            if !matches!(value.trim(), "q" | "Q" | "\u{1b}") {
                writeln!(out, "Enter 6–254 whole KAS, or 5 for the default.")?; continue;
            }
        }
        let mut chars = value.trim().chars();
        if let Some(ch) = chars.next() {
            if chars.next().is_none() {
                if let Some(result) = configure_key(settings, &mut page, PromptKey::Character(ch)) { return Ok(result); }
            }
        }
    }
}
fn configure_prompt_key(settings: &mut GameSettings, page: &mut ConfigurePage, key: PromptKey,
    begin_screen: impl FnOnce() -> io::Result<()>) -> io::Result<Option<Prompt<()>>> {
    let previous = *page;
    let result = configure_key(settings, page, key);
    if *page != previous { begin_screen()?; }
    Ok(result)
}
fn parse_wager(value: &str) -> Option<u8> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) { return None; }
    value.parse::<u8>().ok().filter(|stake| (5..=254).contains(stake))
}
pub fn interactive_configure(settings: &mut GameSettings) -> io::Result<Prompt<()>> {
    if !interactive_tty() { return configure(&mut io::stdin().lock(), &mut io::stdout().lock(), settings); }
    settings.validate().map_err(|error| io::Error::other(error.to_string()))?;
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let mut page = ConfigurePage::Overview;
    let mut draft = String::new();
    let mut invalid = false;
    loop {
        let mut frame = configure_frame(*settings, page);
        if page == ConfigurePage::Wager { frame.push_str(&format!("\n\nKAS: {draft}▏")); }
        terminal.draw_with_panels(&frame, None, if invalid { Some("Enter one of the shown values.") } else { None })?;
        let key = if page == ConfigurePage::Wager { terminal.prompt_text_key(std::time::Duration::from_millis(100))? }
            else { terminal.prompt_key(std::time::Duration::from_millis(100))? };
        if let Some(key) = key {
            if page == ConfigurePage::Wager {
                match key {
                    PromptKey::Enter => {
                        if let Some(stake) = parse_wager(&draft) {
                            settings.stake_kas = stake; page = ConfigurePage::Overview;
                            draft.clear(); invalid = false; terminal.begin_prompt()?;
                        } else { invalid = true; }
                        continue;
                    }
                    PromptKey::Backspace => { draft.pop(); invalid = false; continue; }
                    PromptKey::Character(ch) if !matches!(ch, 'q' | 'Q') => { draft.push(ch); invalid = false; continue; }
                    _ => {}
                }
            }
            let previous = page;
            if let Some(result) = configure_prompt_key(settings, &mut page, key, || terminal.begin_prompt())? { return Ok(result); }
            invalid = page == previous;
            if page != previous { draft.clear(); }
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
const SEND_FEE_MENU: &str = "Send transaction fee\n\n  0  Manually choose total fee\n  1  Priority\n  2  Normal\n  3  Economy\n  q/Esc  Cancel";
fn send_fee_choice(ch: char) -> Option<super::node::wallet::SendFee> {
    use super::node::{fee_policy::FeePolicy, wallet::SendFee};
    match ch { '1' => Some(SendFee::Estimated(FeePolicy::Priority)), '2' => Some(SendFee::Estimated(FeePolicy::Normal)),
        '3' => Some(SendFee::Estimated(FeePolicy::Economy)), _ => None }
}
pub fn send_fee(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Prompt<super::node::wallet::SendFee>> {
    writeln!(out, "{SEND_FEE_MENU}")?;
    loop {
        let Some(value) = line(input, out, "Choose fee: ")? else { return Ok(Prompt::Back) };
        if value == "0" {
            loop {
                let Some(value) = line(input, out, "Total transaction fee in KAS (q cancels): ")? else { return Ok(Prompt::Back) };
                if let Some(Some(fee)) = parse_send_amount(&value) { return Ok(Prompt::Selected(super::node::wallet::SendFee::Manual(fee))); }
                writeln!(out, "Enter a positive fee with up to 8 decimal places.")?;
            }
        }
        if value.len() == 1 {
            if let Some(fee) = value.chars().next().and_then(send_fee_choice) { return Ok(Prompt::Selected(fee)); }
        }
    }
}
pub fn interactive_send_fee() -> io::Result<Prompt<super::node::wallet::SendFee>> {
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    loop {
        terminal.draw_with_panels(SEND_FEE_MENU, None, None)?;
        match terminal.prompt_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Character('0')) => {
                drop(terminal);
                let mut label = "Total transaction fee in KAS (q/Esc: cancel):";
                loop {
                    match interactive_line(label)? {
                        Prompt::Selected(value) => {
                            if let Some(Some(fee)) = parse_send_amount(&value) { return Ok(Prompt::Selected(super::node::wallet::SendFee::Manual(fee))); }
                            label = "Enter a positive fee in KAS with up to 8 decimals (q/Esc: cancel):";
                        }
                        Prompt::Back => return Ok(Prompt::Back), Prompt::Exit => return Ok(Prompt::Exit),
                    }
                }
            }
            Some(PromptKey::Character('q' | 'Q') | PromptKey::Back | PromptKey::Exit) => return Ok(Prompt::Back),
            Some(PromptKey::Character(ch)) => if let Some(fee) = send_fee_choice(ch) { return Ok(Prompt::Selected(fee)); },
            _ => {}
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
        terminal.draw_with_panels(&format!("Send {} KAS\n\nTo:\n{address}\n\nTransaction fee: {} KAS\nTotal deducted: {} KAS\n\nSend may use reserved coins and disrupt a pending game or refund.\n\nEnter: send KAS    Esc/q: cancel", format_kas(amount), format_kas(fee), format_kas(amount.checked_add(fee).ok_or_else(|| io::Error::other("send total exceeds supported amount"))?)), None, None)?;
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
        PromptKey::Character('3') => Some(Prompt::Selected(Action::Configure)),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuPage { Main, Wallet, Settings, Advanced }
impl MenuPage {
    fn parent(self) -> Self { if self == Self::Advanced { Self::Settings } else { Self::Main } }
}
fn navigation_key(page: &mut MenuPage, key: PromptKey) -> Option<Prompt<Action>> {
    if matches!(key, PromptKey::Exit) { return Some(Prompt::Exit); }
    if matches!(key, PromptKey::Back | PromptKey::Character('q' | 'Q')) {
        if *page == MenuPage::Main { return Some(Prompt::Back); }
        *page = page.parent();
        return None;
    }
    let action = match (*page, key) {
        (MenuPage::Main, PromptKey::Character('4')) => { *page = MenuPage::Wallet; return None; }
        (MenuPage::Main, PromptKey::Character('5')) => { *page = MenuPage::Settings; return None; }
        (MenuPage::Main, key) => return action_key(key),
        (MenuPage::Wallet, PromptKey::Character('1')) => Action::Send,
        (MenuPage::Settings, PromptKey::Character('1')) => Action::ConfigureFee,
        (MenuPage::Settings, PromptKey::Character('2')) => Action::ConfigureAutoAccept,
        (MenuPage::Settings, PromptKey::Character('3')) => { *page = MenuPage::Advanced; return None; }
        (MenuPage::Advanced, PromptKey::Character('1')) => Action::ConfigurePlayback,
        (MenuPage::Advanced, PromptKey::Character('2')) => Action::ConfigureBroadcast,
        _ => return None,
    };
    Some(Prompt::Selected(action))
}
fn identity_frame(player: Option<&MenuIdentity>, details: bool) -> String {
    let Some(player) = player else { return String::new(); };
    let (total, available) = match player.balance.lock() {
        Ok(state) => {
            let available = super::node::reservations::reserved().map(|reserved| balance_text(&state, &reserved))
                .unwrap_or_else(|_| "Unavailable".into());
            (balance_text(&state, &[]), available)
        }
        Err(_) => ("Unavailable".into(), "Unavailable".into()),
    };
    let header = format!("Wallet: {} · {}\nAvailable: {available}\n", player.wallet_name, player.network);
    if details {
        format!("{header}Total balance: {total}\nAvailable excludes coins reserved for pending transactions.\n\nWallet address:\n{}\nPlayer public key:\n{}\n", player.address, player.public_key)
    } else { header }
}
pub fn menu_frame(player: Option<&MenuIdentity>, settings: GameSettings, _broadcast_nodes: usize) -> String {
    format!("PONG\n{}\n  1  Host game (RIGHT)\n  2  Join game (LEFT)\n  3  Game settings\n  4  Wallet\n  5  Settings\n  q  Quit\n\nHosting: {}\nStake: {} KAS per player + network fees\n\nNumber: choose · No Enter needed", identity_frame(player, false), settings.summary(), settings.stake_kas)
}
fn navigation_frame(page: MenuPage, player: Option<&MenuIdentity>, settings: GameSettings, broadcast_nodes: usize) -> String {
    match page {
        MenuPage::Main => menu_frame(player, settings, broadcast_nodes),
        MenuPage::Wallet => format!("Wallet\n\n{}\n  1  Send KAS\n\nNumber: choose (no Enter) · Esc/q: back", identity_frame(player, true)),
        MenuPage::Settings => format!("Settings\nChanges apply immediately for this session.\n\n  1  Game transaction fees ({})\n  2  Invitation auto-accept ({})\n  3  Advanced\n\nNumber: choose (no Enter) · Esc/q: back", super::node::fee_policy::policy().label(), auto_accept_label()),
        MenuPage::Advanced => format!("Settings > Advanced\nChanges apply immediately for this session.\n\n  1  Playback buffering ({} DAA base)\n  2  Transaction broadcast nodes ({broadcast_nodes})\n\nNumber: choose (no Enter) · Esc/q: back", super::network_ui::playback_base()),
    }
}
fn auto_accept_label() -> String {
    match super::network_ui::auto_accept_seconds() { 0 => "Off".into(), seconds => format!("{seconds}s") }
}
fn parse_auto_accept_seconds(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())).then(|| value.parse().ok()).flatten()
}
pub fn auto_accept(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Prompt<u64>> {
    loop {
        let Some(value) = line(input, out, "Invitation auto-accept delay in whole seconds (0: never; q/Esc: back): ")? else { return Ok(Prompt::Back) };
        if let Some(seconds) = parse_auto_accept_seconds(&value) { return Ok(Prompt::Selected(seconds)); }
        writeln!(out, "Enter a non-negative whole number of seconds.")?;
    }
}
pub fn interactive_auto_accept() -> io::Result<Prompt<u64>> {
    if !interactive_tty() { return auto_accept(&mut io::stdin().lock(), &mut io::stdout().lock()); }
    let mut label = "Invitation auto-accept\n\nAutomatically approves joining and locks the shown stake.\nCountdown starts after submitting a game code.\nDefault: 5 seconds. 0: always require manual approval.\nEsc/q: back.\n\nEnter whole seconds:";
    loop {
        match interactive_line(label)? {
            Prompt::Selected(value) => if let Some(seconds) = parse_auto_accept_seconds(&value) { return Ok(Prompt::Selected(seconds)); }
                else { label = "Enter a non-negative whole number of seconds (0: never; q/Esc: back):"; },
            Prompt::Back => return Ok(Prompt::Back), Prompt::Exit => return Ok(Prompt::Exit),
        }
    }
}
fn broadcast_nodes_key(key: PromptKey) -> Option<Prompt<usize>> {
    match key {
        PromptKey::Character(ch @ '1'..='8') => Some(Prompt::Selected((ch as u8 - b'0') as usize)),
        PromptKey::Back | PromptKey::Character('q' | 'Q') => Some(Prompt::Back),
        PromptKey::Exit => Some(Prompt::Exit),
        _ => None,
    }
}
const BROADCAST_PROMPT: &str = "Transaction broadcast nodes\n\nMore nodes may improve transaction responsiveness, but use more\nCPU, memory, bandwidth and connections. Improvement is not guaranteed.\nFewer nodes use fewer resources. This adds no extra chain fees.\n\nChoose 1–8 total nodes, including the primary.\n1 uses only the primary; default: 5.\nFewer nodes may be available.\n\nq/Esc: back";
pub fn broadcast_nodes(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Prompt<usize>> {
    writeln!(out, "{BROADCAST_PROMPT}")?;
    loop {
        let Some(value) = line(input, out, "Nodes: ")? else { return Ok(Prompt::Back) };
        if value.len() == 1 {
            if let Some(result) = broadcast_nodes_key(PromptKey::Character(value.chars().next().unwrap())) { return Ok(result); }
        }
        writeln!(out, "Choose a whole number from 1 to 8.")?;
    }
}
pub fn interactive_broadcast_nodes() -> io::Result<Prompt<usize>> {
    if !interactive_tty() { return broadcast_nodes(&mut io::stdin().lock(), &mut io::stdout().lock()); }
    let terminal = super::terminal::Terminal::new()?;
    terminal.begin_prompt()?;
    loop {
        terminal.draw_with_panels(BROADCAST_PROMPT, None, None)?;
        if let Some(result) = terminal.prompt_key(std::time::Duration::from_millis(100))?.and_then(broadcast_nodes_key) { return Ok(result); }
    }
}
fn parse_playback_base(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|value| *value <= super::network_ui::PLAYBACK_MAX_BUFFER_DAA)
}
pub fn playback_base(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Prompt<u64>> {
    loop {
        let Some(value) = line(input, out, "Display delay base (0-15 DAA; q/Esc: back): ")? else { return Ok(Prompt::Back) };
        if let Some(value) = parse_playback_base(&value) { return Ok(Prompt::Selected(value)); }
        writeln!(out, "Enter a whole number from 0 to 15.")?;
    }
}
pub fn interactive_playback_base() -> io::Result<Prompt<u64>> {
    let mut label = "Playback buffering\nSmooths animation when chain updates arrive unevenly.\nLower values reduce the base delay; higher values add buffering.\nBase: 0–15 DAA (about 0–1.5 seconds on a 10-BPS network).\nAdaptive maximum: 15 DAA. Default base: 2 DAA.\n\nEnter a number, then Enter · Esc/q: back:";
    loop {
        match interactive_line(label)? {
            Prompt::Selected(value) => if let Some(value) = parse_playback_base(&value) { return Ok(Prompt::Selected(value)); }
                else { label = "Enter a whole number from 0 to 15 (q/Esc: back):"; },
            Prompt::Back => return Ok(Prompt::Back), Prompt::Exit => return Ok(Prompt::Exit),
        }
    }
}
fn fee_policy_frame() -> String {
    format!("Game transaction fees\nHigher priority may cost more; faster acceptance is not guaranteed.\nSelected: {}\n\n  1  Priority\n  2  Normal\n  3  Economy\n\nNumber: choose (no Enter) · Esc/q: back", super::node::fee_policy::policy().label())
}
pub fn fee_policy(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Prompt<super::node::fee_policy::FeePolicy>> {
    use super::node::fee_policy::FeePolicy;
    writeln!(out, "{}", fee_policy_frame())?;
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
        terminal.draw_with_panels(&fee_policy_frame(), None, None)?;
        match terminal.prompt_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Character('1')) => return Ok(Prompt::Selected(FeePolicy::Priority)),
            Some(PromptKey::Character('2')) => return Ok(Prompt::Selected(FeePolicy::Normal)),
            Some(PromptKey::Character('3')) => return Ok(Prompt::Selected(FeePolicy::Economy)),
            Some(PromptKey::Character('q' | 'Q') | PromptKey::Back) => return Ok(Prompt::Back),
            Some(PromptKey::Exit) => return Ok(Prompt::Exit), _ => {}
        }
    }
}
pub fn interactive_action(error: Option<&str>, player: Option<&MenuIdentity>, settings: GameSettings, broadcast_nodes: usize) -> io::Result<Option<Action>> {
    if !interactive_tty() { return action(&mut io::stdin().lock(), &mut io::stdout().lock(), settings, broadcast_nodes); }
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let notice = error.map(setup_notice);
    let mut page = MenuPage::Main;
    let mut invalid = false;
    loop {
        let frame = format!("{}\n\n{}", navigation_frame(page, player, settings, broadcast_nodes), super::node::hosted::status());
        terminal.draw_with_panels(&frame,None,if invalid { Some("Choose one of the listed options.") } else { notice.as_deref() })?;
        if let Some(key) = terminal.prompt_key(std::time::Duration::from_millis(100))? {
            let previous = page;
            if let Some(choice) = navigation_key(&mut page, key) {
                return Ok(match choice { Prompt::Selected(action) => Some(action), Prompt::Back | Prompt::Exit => None });
            }
            invalid = page == previous;
            if page != previous { terminal.begin_prompt()?; }
        }
    }
}
pub fn interactive_speed() -> io::Result<Prompt<GameSpeed>> {
    if !interactive_tty() {
        return Ok(speed(&mut io::stdin().lock(), &mut io::stdout().lock())?.map(Prompt::Selected).unwrap_or(Prompt::Back));
    }
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let mut settings = GameSettings::default();
    let mut page = ConfigurePage::Speed;
    loop {
        terminal.draw_with_panels(&configure_frame(settings, page), None, None)?;
        if let Some(key) = terminal.prompt_key(std::time::Duration::from_millis(100))? {
            let cancelled = matches!(key, PromptKey::Back | PromptKey::Character('q' | 'Q' | '\u{1b}'));
            let previous = page;
            if let Some(Prompt::Exit) = configure_prompt_key(&mut settings, &mut page, key, || terminal.begin_prompt())? {
                return Ok(Prompt::Exit);
            }
            if page == ConfigurePage::Overview {
                return Ok(if cancelled && previous == ConfigurePage::Speed { Prompt::Back }
                    else { Prompt::Selected(settings.speed) });
            }
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
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let mut draft = String::new();
    let mut error = String::new();
    loop {
        terminal.draw_with_panels(&format!("Join game\nPaste the host's complete game code.\n\nCODE: {draft}▏\n\nEnter: review game · Esc: back"), None,
            if error.is_empty() { None } else { Some(error.as_str()) })?;
        match terminal.prompt_text_key(std::time::Duration::from_millis(100))? {
            Some(PromptKey::Back) => return Ok(Prompt::Back),
            Some(PromptKey::Exit) => return Ok(Prompt::Exit),
            Some(PromptKey::Enter) => {
                if draft.trim().eq_ignore_ascii_case("q") { return Ok(Prompt::Back); }
                match super::node::invitations::parse_rendezvous_code(draft.trim()) {
                    Ok(_) => return Ok(Prompt::Selected(draft.trim().to_string())),
                    Err(reason) => error = format!("Invalid game code: {reason}. Check that you pasted the entire code."),
                }
            }
            Some(PromptKey::Backspace) => { draft.pop(); error.clear(); }
            Some(PromptKey::Character(ch)) => { draft.push(ch); error.clear(); }
            None => {}
        }
    }
}
fn interactive_line(label: &str) -> io::Result<Prompt<String>> {
    let terminal = super::terminal::Terminal::new()?; terminal.begin_prompt()?;
    let mut draft = String::new();
    let prefix = if label.starts_with("GAME CODE") { "CODE: " } else { "" };
    loop {
        terminal.draw_with_panels(&format!("{label}\n{prefix}{draft}▏\n\nEnter: continue · Esc: back"),None,None)?;
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
    use super::{GameSettings, GameSpeed};

    #[test]
    fn wager_menu_accepts_whole_kas_boundaries_and_cancel_keeps_previous_stake() {
        use super::*;
        for stake in 5..=254 {
            let mut settings = GameSettings::default();
            let input = format!("4\n{stake}\nq\n");
            assert_eq!(configure(&mut io::Cursor::new(input), &mut Vec::new(), &mut settings).unwrap(), Prompt::Back);
            assert_eq!(settings.stake_kas, stake);
            assert_eq!(settings.terms().stake_kas, stake);
            settings.validate().unwrap();
        }
        for value in ["", "0", "4", "255", "256", "6.5", "-6", "+6"] { assert_eq!(parse_wager(value), None); }
        let mut settings = GameSettings { stake_kas: 127, ..Default::default() };
        configure(&mut io::Cursor::new("4\nq\nq\n"), &mut Vec::new(), &mut settings).unwrap();
        assert_eq!(settings.stake_kas, 127);
        configure(&mut io::Cursor::new("4\n255\n6.5\n254\nq\n"), &mut Vec::new(), &mut settings).unwrap();
        assert_eq!(settings.stake_kas, 254);
        assert!(menu_frame(None, settings, 8).contains("Stake: 254 KAS per player"));
        for stake_kas in [0, 4, 255] { assert!(GameSettings { stake_kas, ..settings }.validate().is_err()); }
    }

    #[test]
    fn grouped_navigation_returns_to_parent_and_preserves_actions() {
        use super::*;
        for (input, expected) in [("4\n1\n", Action::Send), ("5\n1\n", Action::ConfigureFee),
            ("5\n2\n", Action::ConfigureAutoAccept), ("5\n3\n1\n", Action::ConfigurePlayback),
            ("5\n3\n2\n", Action::ConfigureBroadcast), ("5\n3\nq\nq\n1\n", Action::Host)] {
            assert_eq!(action(&mut io::Cursor::new(input), &mut Vec::new(), GameSettings::default(), 8).unwrap(), Some(expected));
        }
        let mut output = Vec::new();
        assert_eq!(action(&mut io::Cursor::new("9\n4\nq\nq\n"), &mut output, GameSettings::default(), 8).unwrap(), None);
        assert!(String::from_utf8(output).unwrap().contains("Choose one of the listed options."));
        let mut page = MenuPage::Main;
        navigation_key(&mut page, PromptKey::Character('5'));
        navigation_key(&mut page, PromptKey::Character('3'));
        navigation_key(&mut page, PromptKey::Back);
        assert_eq!(page, MenuPage::Settings);
        navigation_key(&mut page, PromptKey::Back);
        assert_eq!(page, MenuPage::Main);
    }

    #[test]
    fn settings_default_auto_scores_and_explicit_terms() {
        let defaults = GameSettings::default();
        assert_eq!(defaults, GameSettings { speed: GameSpeed::Medium, ball_count: 1, goal_limit: 0, stake_kas: 5 });
        for (balls, goals) in [(1, 3), (2, 5), (3, 6), (4, 7), (5, 8), (6, 9), (7, 10), (8, 11)] {
            let mut settings = GameSettings { ball_count: balls, ..defaults };
            assert!(settings.validate().is_ok());
            assert_eq!(settings.win_score(), goals);
            assert_eq!(settings.terms(), crate::node::invitations::GameTerms {
                speed: GameSpeed::Medium, ball_count: balls, win_score: goals, stake_kas: 5,
            });
            for explicit in 1..=15 {
                settings.goal_limit = explicit;
                assert!(settings.validate().is_ok());
                assert_eq!(settings.win_score(), explicit);
                assert_eq!(settings.terms().win_score, explicit);
            }
        }
        for balls in [0, 9, u8::MAX] {
            assert!(GameSettings { ball_count: balls, ..defaults }.validate().is_err());
        }
        for goals in [16, u8::MAX] {
            assert!(GameSettings { goal_limit: goals, ..defaults }.validate().is_err());
        }
    }

    #[test]
    fn redirected_configure_updates_settings_and_keeps_explicit_goals() {
        use super::*;
        let mut settings = GameSettings::default();
        let mut output = Vec::new();
        // Super Fast, 3 balls, explicit 9 goals, then 2 balls and back.
        let mut input = std::io::Cursor::new(b"1\n1\n2\n3\n3\n9\n2\n2\nq\n");
        assert_eq!(configure(&mut input, &mut output, &mut settings).unwrap(), Prompt::Back);
        assert_eq!(settings, GameSettings { speed: GameSpeed::SuperFast, ball_count: 2, goal_limit: 9, stake_kas: 5 });
        assert_eq!(settings.win_score(), 9);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("First to 6 goals · Auto"));
        assert!(output.contains("First to win (First to 9 goals)"));
        assert!(menu_frame(None, settings, 8).contains("Hosting: Super Fast, 2 balls, First to 9 goals"));
        let mut input = std::io::Cursor::new(b"3\n0\nq\n");
        assert_eq!(configure(&mut input, &mut Vec::new(), &mut settings).unwrap(), Prompt::Back);
        assert_eq!(settings.win_score(), 5);
        assert_eq!(settings.goal_limit, 0);
    }

    #[test]
    fn configure_navigation_uses_immediate_keys_and_preserves_on_cancel() {
        use super::*;
        let mut settings = GameSettings::default();
        let mut page = ConfigurePage::Overview;
        for key in ['1', 'q', '2', '\u{1b}', '3', 'Q'] {
            assert_eq!(configure_key(&mut settings, &mut page, PromptKey::Character(key)), None);
        }
        assert_eq!(page, ConfigurePage::Overview);
        assert_eq!(settings, GameSettings::default());
        assert_eq!(configure_key(&mut settings, &mut page, PromptKey::Back), Some(Prompt::Back));
        for selected in [ConfigurePage::Overview, ConfigurePage::Speed, ConfigurePage::ManualSpeed, ConfigurePage::Balls, ConfigurePage::Goals] {
            page = selected;
            assert_eq!(configure_key(&mut settings, &mut page, PromptKey::Exit), Some(Prompt::Exit));
        }
    }

    #[test]
    fn configure_page_changes_block_held_keys_without_release_events() {
        use super::*;
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        use crate::terminal::PromptKeyFilter;
        use std::time::{Duration, Instant};
        for (choice, expected) in [('1', ConfigurePage::Speed), ('2', ConfigurePage::Balls), ('3', ConfigurePage::Goals)] {
            let mut filter = PromptKeyFilter::new(false); filter.begin_screen();
            let now = Instant::now();
            let event = KeyEvent::new_with_kind(KeyCode::Char(choice), KeyModifiers::NONE, KeyEventKind::Press);
            let mut settings = GameSettings::default(); let mut page = ConfigurePage::Overview;
            let key = filter.filter(event, now, false).unwrap();
            configure_prompt_key(&mut settings, &mut page, key, || { filter.begin_screen(); Ok(()) }).unwrap();
            assert_eq!(page, expected);
            assert_eq!(filter.filter(event, now + Duration::from_millis(50), false), None);
            assert_eq!(settings, GameSettings::default(), "a held menu key must not select a value");
            let key = filter.filter(event, now + Duration::from_millis(850), false).unwrap();
            configure_prompt_key(&mut settings, &mut page, key, || { filter.begin_screen(); Ok(()) }).unwrap();
            assert_eq!(page, ConfigurePage::Overview);
            assert_eq!(filter.filter(event, now + Duration::from_millis(900), false), None);
        }
    }
    #[test]
    fn manual_speed_and_goals_accept_single_hex_keys_and_eight_balls() {
        use super::*;
        for ticks in 1..=15 {
            for ch in [char::from_digit(ticks, 16).unwrap(), char::from_digit(ticks, 16).unwrap().to_ascii_uppercase()] {
                let mut settings = GameSettings::default();
                let mut page = ConfigurePage::Overview;
                for key in ['1', '0', ch] { configure_key(&mut settings, &mut page, PromptKey::Character(key)); }
                assert_eq!(page, ConfigurePage::Overview);
                assert_eq!(settings.speed.tick_daa(), i64::from(ticks));
                for balls in 1..=8 {
                    settings.ball_count = balls;
                    for goals in 1..=15 {
                        settings.goal_limit = goals;
                        assert!(settings.validate().is_ok());
                        assert!(settings.terms().validate().is_ok());
                    }
                }
                page = ConfigurePage::Goals;
                configure_key(&mut settings, &mut page, PromptKey::Character(ch));
                assert_eq!(settings.goal_limit, ticks as u8);
                assert_eq!(page, ConfigurePage::Overview);
                let mut input = std::io::Cursor::new(format!("0\n{ch}\n"));
                assert_eq!(speed(&mut input, &mut Vec::new()).unwrap().unwrap().tick_daa(), i64::from(ticks));
            }
        }
        for balls in 1..=8 {
            let mut settings = GameSettings::default();
            let mut page = ConfigurePage::Balls;
            configure_key(&mut settings, &mut page, PromptKey::Character(char::from(b'0' + balls)));
            assert_eq!(settings.ball_count, balls);
            assert_eq!(page, ConfigurePage::Overview);
        }
        for ticks in [0, 16, u8::MAX] {
            assert!(GameSettings { speed: GameSpeed::Manual(ticks), ..Default::default() }.validate().is_err());
        }
    }

    #[test]
    fn configure_hints_show_decimal_choices_and_keep_extra_ball_keys_hidden() {
        use super::*;
        let settings = GameSettings::default();
        for page in [ConfigurePage::ManualSpeed, ConfigurePage::Goals] {
            let frame = configure_frame(settings, page);
            assert!(frame.contains("  1–9  "));
            for hidden in ["1–F", "A–F", "1–15", "hex"] {
                assert!(!frame.contains(hidden), "{frame}");
            }
        }
        assert!(configure_frame(settings, ConfigurePage::Goals).contains("  0  Auto"));
        assert!(configure_frame(settings, ConfigurePage::Speed).contains("  0  Manual configure"));
        for balls in 1..=8 {
            let frame = configure_frame(GameSettings { ball_count: balls, ..settings }, ConfigurePage::Balls);
            for listed in 1..=6 { assert!(frame.contains(&format!("\n  {listed}  "))); }
            for hidden in [7, 8] { assert!(!frame.contains(&format!("\n  {hidden}  "))); }
        }
        for balls in [7, 8] {
            let mut selected = settings;
            let mut input = std::io::Cursor::new(format!("2\n{balls}\nq\n"));
            assert_eq!(configure(&mut input, &mut Vec::new(), &mut selected).unwrap(), Prompt::Back);
            assert_eq!(selected.ball_count, balls);
            assert!(selected.validate().is_ok());
        }
    }

    #[test]
    fn manual_speed_rejects_invalid_input_and_cancel_preserves_settings() {
        use super::*;
        for cancel in [PromptKey::Back, PromptKey::Character('q'), PromptKey::Character('Q'), PromptKey::Character('\u{1b}')] {
            let mut settings = GameSettings::default();
            let mut page = ConfigurePage::ManualSpeed;
            for key in [PromptKey::Character('0'), PromptKey::Character('g'), PromptKey::Character('G'), PromptKey::Enter, PromptKey::Backspace] {
                assert_eq!(configure_key(&mut settings, &mut page, key), None);
                assert_eq!(page, ConfigurePage::ManualSpeed);
                assert_eq!(settings, GameSettings::default());
            }
            assert_eq!(configure_key(&mut settings, &mut page, cancel), None);
            assert_eq!(page, ConfigurePage::Speed);
            assert_eq!(settings, GameSettings::default());
        }
        let mut settings = GameSettings::default();
        let mut input = std::io::Cursor::new("1\n0\n0\ng\n10\n-1\nf\n3\nF\n2\n6\nq\n");
        assert_eq!(configure(&mut input, &mut Vec::new(), &mut settings).unwrap(), Prompt::Back);
        assert_eq!(settings.speed.tick_daa(), 15);
        assert_eq!((settings.ball_count, settings.goal_limit), (6, 15));
        for answer in ["0\nq\nq\n", "0\n\u{1b}\nq\n", "0\n", "0\ng\n10\n"] {
            assert_eq!(speed(&mut std::io::Cursor::new(answer), &mut Vec::new()).unwrap(), None);
        }
        assert_eq!(speed(&mut std::io::Cursor::new("0\nq\n2\n"), &mut Vec::new()).unwrap(), Some(GameSpeed::Fast));
        let mut settings = GameSettings { goal_limit: 15, ..Default::default() };
        let mut page = ConfigurePage::Goals;
        for key in ['g', 'G', '-', ' '] {
            configure_key(&mut settings, &mut page, PromptKey::Character(key));
            assert_eq!(page, ConfigurePage::Goals);
            assert_eq!(settings.goal_limit, 15);
        }
        configure_key(&mut settings, &mut page, PromptKey::Character('0'));
        assert_eq!(settings.goal_limit, 0);
        assert_eq!(settings.win_score(), 3);
        let mut input = std::io::Cursor::new("1\n0\nq\nq\nq\n");
        let mut settings = GameSettings::default();
        assert_eq!(configure(&mut input, &mut Vec::new(), &mut settings).unwrap(), Prompt::Back);
        assert_eq!(settings, GameSettings::default());
    }

    #[test]
    fn auto_accept_menu_accepts_whole_seconds_and_cancellation() {
        use super::*;
        let mut page = MenuPage::Settings;
        assert_eq!(navigation_key(&mut page, PromptKey::Character('2')), Some(Prompt::Selected(Action::ConfigureAutoAccept)));
        for value in ["0", "1", "30", "18446744073709551615"] { assert_eq!(parse_auto_accept_seconds(value), value.parse().ok()); }
        for value in ["", "-1", "1.5", "+1", "18446744073709551616"] { assert_eq!(parse_auto_accept_seconds(value), None); }
        assert_eq!(auto_accept(&mut io::Cursor::new("1.5\n-1\n3\n"), &mut Vec::new()).unwrap(), Prompt::Selected(3));
        assert_eq!(auto_accept(&mut io::Cursor::new("0\n"), &mut Vec::new()).unwrap(), Prompt::Selected(0));
        for value in ["q\n", "\x1b\n", ""] { assert_eq!(auto_accept(&mut io::Cursor::new(value), &mut Vec::new()).unwrap(), Prompt::Back); }
        assert_eq!(action(&mut io::Cursor::new("5\n2\n"), &mut Vec::new(), GameSettings::default(), 8).unwrap(), Some(Action::ConfigureAutoAccept));
    }

    #[test]
    fn broadcast_menu_selects_limits_and_cancel_does_not_select() {
        use super::*;
        let mut page = MenuPage::Advanced;
        assert_eq!(navigation_key(&mut page, PromptKey::Character('2')), Some(Prompt::Selected(Action::ConfigureBroadcast)));
        assert!(navigation_frame(MenuPage::Advanced, None, GameSettings::default(), 3).contains("Transaction broadcast nodes (3)"));
        for n in 1..=8 {
            assert_eq!(broadcast_nodes_key(PromptKey::Character(char::from(b'0' + n))), Some(Prompt::Selected(n as usize)));
            assert_eq!(broadcast_nodes(&mut io::Cursor::new(format!("{n}\n")), &mut Vec::new()).unwrap(), Prompt::Selected(n as usize));
        }
        assert_eq!(broadcast_nodes_key(PromptKey::Character('0')), None);
        assert_eq!(broadcast_nodes_key(PromptKey::Character('9')), None);
        for value in ["q\n", "\x1b\n", ""] {
            assert_eq!(broadcast_nodes(&mut io::Cursor::new(value), &mut Vec::new()).unwrap(), Prompt::Back);
        }
        assert_eq!(broadcast_nodes_key(PromptKey::Exit), Some(Prompt::Exit));
        assert_eq!(broadcast_nodes(&mut io::Cursor::new("0\n9\n1.5\n2\n"), &mut Vec::new()).unwrap(), Prompt::Selected(2));
        assert_eq!(action(&mut io::Cursor::new("5\n3\n2\n"), &mut Vec::new(), GameSettings::default(), 8).unwrap(), Some(Action::ConfigureBroadcast));
    }

    #[test]
    fn playback_delay_accepts_only_zero_through_fifteen() {
        use super::*;
        assert_eq!(parse_playback_base("0"), Some(0));
        assert_eq!(parse_playback_base("15"), Some(15));
        assert_eq!(parse_playback_base("16"), None);
        assert_eq!(parse_playback_base("-1"), None);
        assert_eq!(playback_base(&mut std::io::Cursor::new(b"16\n3\n"), &mut Vec::new()).unwrap(), Prompt::Selected(3));
        assert_eq!(playback_base(&mut std::io::Cursor::new(b"q\n"), &mut Vec::new()).unwrap(), Prompt::Back);
    }

    #[test]
    fn send_menu_validates_whole_kas_and_cancellation() {
        use super::*;
        let mut page = MenuPage::Wallet;
        assert_eq!(navigation_key(&mut page, PromptKey::Character('1')), Some(Prompt::Selected(Action::Send)));
        assert!(navigation_frame(MenuPage::Wallet, None, GameSettings::default(), 8).contains("1  Send KAS"));
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
    fn wallet_details_show_complete_address_and_main_menu_stays_compact() {
        let address = "kaspa:qp57jvcv65ee3m5kujq4k2kuf5wduy2ksnd4q7w3l7a25vzu03wxge7h5slp0";
        let player = super::MenuIdentity { wallet_name: "alice".into(), public_key: "public-key".into(), address: address.into(), network: "mainnet".into(), balance: std::sync::Arc::new(std::sync::Mutex::new(super::Balance::Loading)) };
        let main = super::menu_frame(Some(&player), GameSettings::default(), 8);
        assert!(main.contains("Wallet: alice · mainnet"));
        assert!(!main.contains(address) && !main.contains("public-key"));
        let frame = super::navigation_frame(super::MenuPage::Wallet, Some(&player), GameSettings::default(), 8);
        assert!(frame.contains(&format!("Wallet address:\n{address}")));
        assert!(frame.contains("Player public key:\npublic-key"));
        assert!(frame.contains("Total balance: Loading..."));
        assert!(frame.contains("Available: Loading..."));
        assert!(main.contains("1  Host") && main.contains("2  Join"));
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
    fn fee_menu_selects_policy_and_allows_cancellation() {
        use super::*;
        use crate::node::fee_policy::FeePolicy;
        let mut page = MenuPage::Settings;
        assert_eq!(navigation_key(&mut page, PromptKey::Character('1')), Some(Prompt::Selected(Action::ConfigureFee)));
        assert_eq!(fee_policy(&mut std::io::Cursor::new(b"1\n"), &mut Vec::new()).unwrap(), Prompt::Selected(FeePolicy::Priority));
        assert_eq!(fee_policy(&mut std::io::Cursor::new(b"3\n"), &mut Vec::new()).unwrap(), Prompt::Selected(FeePolicy::Economy));
        assert_eq!(fee_policy(&mut std::io::Cursor::new(b"\x1b\n"), &mut Vec::new()).unwrap(), Prompt::Back);
    }
    #[test]
    fn send_fee_menu_accepts_exact_kas_and_reprompts_invalid_values() {
        use super::*;
        use crate::node::{wallet::SendFee, fee_policy::FeePolicy};
        let mut output = Vec::new();
        assert_eq!(send_fee(&mut std::io::Cursor::new(b"0\nall\n-1\n0\n0.000000001\n0.00123456\n"), &mut output).unwrap(), Prompt::Selected(SendFee::Manual(123_456)));
        assert!(String::from_utf8(output).unwrap().contains("0  Manually choose total fee"));
        assert_eq!(send_fee(&mut std::io::Cursor::new(b"1\n"), &mut Vec::new()).unwrap(), Prompt::Selected(SendFee::Estimated(FeePolicy::Priority)));
        assert_eq!(send_fee(&mut std::io::Cursor::new(b"0\nq\n"), &mut Vec::new()).unwrap(), Prompt::Back);
    }
}
