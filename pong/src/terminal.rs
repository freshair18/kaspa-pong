//! Immediate keyboard input and scoped terminal setup. Gameplay remains in the
//! existing transaction engine; this module only handles the terminal.
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    cell::RefCell,
    io::{self, BufWriter, IsTerminal, Read, Write},
    sync::{Mutex, mpsc::{self, Receiver}},
    time::{Duration, Instant},
};

pub struct Terminal {
    raw: bool,
    screen: bool,
    piped: Option<Receiver<char>>,
    previous_frame: RefCell<Option<ScreenFrame>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PromptKey {
    Character(char),
    Enter,
    Backspace,
    Back,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameplayCommand {
    Up,
    Down,
    DismissNotice,
    ToggleSound,
    Exit,
}

/// Gameplay exits only on Escape; menu/setup commands use their own decoder.
pub fn gameplay_key(key: KeyEvent) -> Option<GameplayCommand> {
    if key.kind == KeyEventKind::Release { return None; }
    match key.code {
        KeyCode::Esc if key.kind == KeyEventKind::Press => Some(GameplayCommand::Exit),
        KeyCode::Up => Some(GameplayCommand::Up),
        KeyCode::Down => Some(GameplayCommand::Down),
        KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            gameplay_line(&ch.to_string())
        }
        _ => None,
    }
}

pub fn gameplay_line(line: &str) -> Option<GameplayCommand> {
    match line.trim() {
        "w" | "W" | "up" => Some(GameplayCommand::Up),
        "s" | "S" | "down" => Some(GameplayCommand::Down),
        "r" | "R" => Some(GameplayCommand::DismissNotice),
        "m" | "M" => Some(GameplayCommand::ToggleSound),
        "\u{1b}" => Some(GameplayCommand::Exit),
        _ => None,
    }
}

fn prompt_key(key: KeyEvent) -> Option<PromptKey> {
    Some(match key.code {
        KeyCode::Esc => PromptKey::Back,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => PromptKey::Back,
        KeyCode::Char('d' | 'z') if key.modifiers.contains(KeyModifiers::CONTROL) => PromptKey::Exit,
        KeyCode::Enter => PromptKey::Enter,
        KeyCode::Backspace => PromptKey::Backspace,
        KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => PromptKey::Character(ch),
        _ => return None,
    })
}

/// Windows crossterm 0.29 emits repeated key-downs as Press, not Repeat.
/// Keep held keys across prompt lifetimes, including their release events.
pub struct PromptKeyFilter {
    releases: bool,
    held: Vec<KeyCode>,
    blocked: Vec<KeyCode>,
    last_input: Option<Instant>,
}
impl PromptKeyFilter {
    pub const fn new(releases: bool) -> Self {
        Self { releases, held: Vec::new(), blocked: Vec::new(), last_input: None }
    }
    pub fn begin_screen(&mut self) {
        self.blocked.clone_from(&self.held);
    }
    fn observe(&mut self, key: KeyEvent, now: Instant) -> bool {
        let code = match key.code {
            KeyCode::Char(ch) => KeyCode::Char(ch.to_ascii_lowercase()),
            code => code,
        };
        if key.kind == KeyEventKind::Release {
            self.held.retain(|held| *held != code);
            self.blocked.retain(|held| *held != code);
            return false;
        }
        // Legacy Unix terminals cannot distinguish a repeated Press from a new
        // press. Require silence before reusing the incoming screen's held key.
        if !self.releases && self.last_input.is_some_and(|last| now.saturating_duration_since(last) >= Duration::from_millis(750)) {
            self.held.clear();
            self.blocked.clear();
        }
        let fresh = !self.held.contains(&code);
        if !self.releases && fresh {
            self.held.clear();
        }
        if fresh { self.held.push(code); }
        self.last_input = Some(now);
        fresh && !self.blocked.contains(&code)
    }
    pub fn filter(&mut self, key: KeyEvent, now: Instant, text: bool) -> Option<PromptKey> {
        let fresh = self.observe(key, now);
        if key.kind == KeyEventKind::Release { return None; }
        let code = match key.code {
            KeyCode::Char(ch) => KeyCode::Char(ch.to_ascii_lowercase()),
            code => code,
        };
        if self.blocked.contains(&code) { return None; }
        if !text && (key.kind == KeyEventKind::Repeat || (self.releases && !fresh)) { return None; }
        prompt_key(key)
    }
    pub fn gameplay(&mut self, key: KeyEvent, now: Instant) -> Option<GameplayCommand> {
        let fresh = self.observe(key, now);
        let code = match key.code {
            KeyCode::Char(ch) => KeyCode::Char(ch.to_ascii_lowercase()),
            code => code,
        };
        if self.blocked.contains(&code) { return None; }
        let command = gameplay_key(key)?;
        if command == GameplayCommand::Exit && self.releases && !fresh { return None; }
        if command == GameplayCommand::ToggleSound && (key.kind == KeyEventKind::Repeat || (self.releases && !fresh)) { return None; }
        Some(command)
    }
}
static PROMPT_KEYS: Mutex<PromptKeyFilter> = Mutex::new(PromptKeyFilter::new(cfg!(windows)));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStyle {
    Plain,
    Activity,
    Alert,
}
#[derive(Debug, PartialEq, Eq)]
pub struct ScreenRow {
    pub text: String,
    pub style: RowStyle,
}
pub struct ScreenFrame {
    size: (u16, u16),
    pub rows: Vec<ScreenRow>,
}
impl ScreenFrame {
    pub fn compose(frame: &str, activity: Option<&str>, notice: Option<&str>, size: (u16, u16)) -> Self {
        let height = size.1 as usize;
        // Leave the last column unused so writing the bottom row cannot wrap
        // and scroll the entire court out of place.
        let width = size.0.saturating_sub(1) as usize;
        let banner: Vec<_> = notice.unwrap_or("").lines().take(height).collect();
        let activity: Vec<_> = activity.unwrap_or("").lines().take(height.saturating_sub(banner.len())).collect();
        let body_height = height.saturating_sub(banner.len() + activity.len());
        let body = panel_body(frame, body_height);
        let mut rows = Vec::with_capacity(height);
        let row = |text: &str, style| {
            let mut text: String = text.chars().take(width).collect();
            text.extend(std::iter::repeat_n(' ', width.saturating_sub(text.chars().count())));
            ScreenRow { text, style }
        };
        for i in 0..body_height {
            rows.push(row(body.get(i).copied().unwrap_or(""), RowStyle::Plain));
        }
        rows.extend(activity.into_iter().map(|text| row(text, RowStyle::Activity)));
        rows.extend(banner.into_iter().map(|text| row(text, RowStyle::Alert)));
        Self { size, rows }
    }
    pub fn changed_rows(&self, previous: Option<&Self>) -> Vec<usize> {
        let previous = previous.filter(|p| p.size == self.size);
        self.rows.iter().enumerate().filter_map(|(i, row)| (previous.and_then(|p| p.rows.get(i)) != Some(row)).then_some(i)).collect()
    }
}
/// Fit diagnostic panels without hiding any court row or the actionable status.
pub fn panel_body(frame: &str, height: usize) -> Vec<&str> {
    let court = frame.lines().nth(3).is_some_and(|line| {
        line.starts_with('+') && line.ends_with('+') && line.len() >= 34 && line[1..line.len() - 1].bytes().all(|b| b == b'-')
    }) && frame.lines().count() >= 24;
    frame
        .lines()
        .enumerate()
        .filter(|(i, _)| {
            !court
                || !(
                    (height < 24 && *i == 21) || // redundant key help
        (height < 23 && *i == 0) ||  // title
        (height < 22 && *i == 2) ||  // updater statistics
        (height < 21 && *i == 22)
                    // metrics footer
                )
        })
        .map(|(_, line)| line)
        .take(height)
        .collect()
}
impl Terminal {
    /// Drain input queued before this prompt. Preserve held/released state so
    /// holding Join cannot choose an invitation on the following screen.
    pub fn begin_prompt(&self) -> io::Result<()> {
        if !self.raw || !self.screen {
            return Err(io::Error::other("interactive prompts require terminal stdin and stdout"));
        }
        let mut keys = PROMPT_KEYS.lock().unwrap_or_else(|error| error.into_inner());
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? { keys.observe(key, Instant::now()); }
        }
        keys.begin_screen();
        Ok(())
    }
    pub fn prompt_key(&self, timeout: Duration) -> io::Result<Option<PromptKey>> {
        self.read_prompt_key(timeout, false)
    }
    pub fn prompt_text_key(&self, timeout: Duration) -> io::Result<Option<PromptKey>> {
        self.read_prompt_key(timeout, true)
    }
    fn read_prompt_key(&self, timeout: Duration, text: bool) -> io::Result<Option<PromptKey>> {
        if !self.raw {
            return Err(io::Error::other("interactive prompts require terminal stdin"));
        }
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                return Ok(PROMPT_KEYS.lock().unwrap_or_else(|error| error.into_inner()).filter(key, Instant::now(), text));
            }
        }
        Ok(None)
    }
    pub fn new() -> io::Result<Self> {
        let mut result = Self { raw: false, screen: false, piped: None, previous_frame: RefCell::new(None) };
        if io::stdin().is_terminal() {
            terminal::enable_raw_mode()?;
            result.raw = true;
        } else {
            // Pipes also deliver immediately, without waiting for a newline.
            let (send, receive) = mpsc::channel();
            std::thread::spawn(move || {
                let mut input = io::stdin().lock();
                let mut buffer = [0; 64];
                while let Ok(count) = input.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    for &byte in &buffer[..count] {
                        if send.send(char::from(byte)).is_err() {
                            return;
                        }
                    }
                }
            });
            result.piped = Some(receive);
        }
        if io::stdout().is_terminal() {
            result.screen = true;
            execute!(io::stdout(), EnterAlternateScreen, Hide, Clear(ClearType::All))?;
        }
        Ok(result)
    }
    pub fn commands(&self) -> io::Result<Vec<String>> {
        let mut commands = Vec::new();
        if let Some(receive) = &self.piped {
            commands.extend(receive.try_iter().filter_map(control));
        } else {
            while event::poll(Duration::ZERO)? {
                if let Event::Key(key) = event::read()? {
                    PROMPT_KEYS.lock().unwrap_or_else(|error| error.into_inner()).observe(key, Instant::now());
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let ch = match key.code {
                        KeyCode::Esc => 'q',
                        KeyCode::Up => 'w',
                        KeyCode::Down => 's',
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => 'q',
                        KeyCode::Char(ch) => ch,
                        _ => continue,
                    };
                    if let Some(command) = control(ch) {
                        commands.push(command);
                    }
                }
            }
        }
        Ok(commands)
    }
    pub fn gameplay_commands(&self) -> io::Result<Vec<GameplayCommand>> {
        let mut commands = Vec::new();
        if let Some(receive) = &self.piped {
            commands.extend(receive.try_iter().filter_map(|ch| gameplay_line(&ch.to_string())));
        } else {
            while event::poll(Duration::ZERO)? {
                if let Event::Key(key) = event::read()? {
                    if let Some(command) = PROMPT_KEYS.lock().unwrap_or_else(|error| error.into_inner()).gameplay(key, Instant::now()) {
                        commands.push(command);
                    }
                }
            }
        }
        Ok(commands)
    }
    pub fn draw_with_panels(&self, frame: &str, activity: Option<&str>, notice: Option<&str>) -> io::Result<()> {
        if self.screen {
            let next = ScreenFrame::compose(frame, activity, notice, terminal::size().unwrap_or((80, 25)));
            let mut previous = self.previous_frame.borrow_mut();
            let changed = next.changed_rows(previous.as_ref());
            if changed.is_empty() {
                return Ok(());
            }
            let mut output = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
            // Clear only on an actual resize. Ordinary frames never erase the
            // screen, and unchanged borders/paddles are left in place.
            if previous.as_ref().is_some_and(|p| p.size != next.size) {
                queue!(output, Clear(ClearType::All))?;
            }
            for i in changed {
                let row = &next.rows[i];
                queue!(output, MoveTo(0, i as u16), ResetColor, SetAttribute(Attribute::Reset))?;
                match row.style {
                    RowStyle::Plain => {}
                    RowStyle::Activity => {
                        queue!(output, SetForegroundColor(Color::Cyan))?;
                    }
                    RowStyle::Alert => {
                        queue!(
                            output,
                            SetBackgroundColor(Color::DarkRed),
                            SetForegroundColor(Color::White),
                            SetAttribute(Attribute::Bold)
                        )?;
                    }
                }
                if row.style == RowStyle::Plain && row.text.starts_with(['|', '║', '╏', '┊']) {
                    let mut color = Color::White;
                    queue!(output, SetForegroundColor(color))?;
                    for ch in row.text.chars() {
                        let (glyph, next_color) = court_cell(ch);
                        if next_color != color {
                            queue!(output, SetForegroundColor(next_color))?;
                            color = next_color;
                        }
                        write!(output, "{glyph}")?;
                    }
                } else {
                    output.write_all(row.text.as_bytes())?;
                }
            }
            queue!(output, ResetColor, SetAttribute(Attribute::Reset))?;
            output.flush()?;
            *previous = Some(next);
        }
        Ok(())
    }
    pub fn is_screen(&self) -> bool {
        self.screen
    }
}
fn control(ch: char) -> Option<String> {
    let ch = ch.to_ascii_lowercase();
    if matches!(ch, 'w' | 's' | '1' | '2' | 'q' | 'r') {
        Some(ch.to_string())
    } else if ch == '\u{3}' {
        Some("q".into())
    } else {
        None
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if self.screen {
            let _ = execute!(io::stdout(), ResetColor, SetAttribute(Attribute::Reset), Show, LeaveAlternateScreen);
        }
        if self.raw {
            let _ = terminal::disable_raw_mode();
        }
    }
}

/// Single-column presentation tokens keep layout/clipping free of ANSI escapes.
pub fn court_cell(ch: char) -> (char, Color) {
    const ORANGE: Color = Color::Rgb { r: 249, g: 115, b: 22 };
    const EXTRA: [Color; 5] = [
        Color::Rgb { r: 139, g: 92, b: 246 },
        Color::Rgb { r: 132, g: 204, b: 22 },
        Color::Rgb { r: 77, g: 124, b: 255 },
        Color::Rgb { r: 255, g: 95, b: 162 },
        Color::Rgb { r: 95, g: 255, b: 179 },
    ];
    match ch {
        '\u{f0001}'..='\u{f13ff}' if crate::ascii::ball_dots(ch).is_some() => {
            let token = ch as u32 - 0xf0000;
            let Color::Rgb { r, g, b } = EXTRA[(token / 0x400) as usize] else { unreachable!() };
            let gain = [130, 195, 255, 255][(token % 0x400 / 0x100) as usize];
            let shade = |value| (u32::from(value) * gain / 255) as u8;
            (char::from_u32(0x2800 + token % 0x100).unwrap(), Color::Rgb { r: shade(r), g: shade(g), b: shade(b) })
        }
        '\u{f1400}'..='\u{f1427}' => {
            let token = ch as u32 - 0xf1400;
            let glyph = ['←', '→', '↖', '↗', '↙', '↘'].get((token % 8) as usize).copied().unwrap_or(' ');
            (glyph, EXTRA[(token / 8) as usize])
        }
        '\u{f1500}'..='\u{f1509}' => {
            let token = ch as u32 - 0xf1500;
            (if token % 2 == 0 { '✧' } else { '·' }, EXTRA[(token / 2) as usize])
        }
        '\u{e701}'..='\u{eeff}' => {
            let token = ch as u32 - 0xe700;
            let colors = if token / 0x400 == 0 {
                [Color::AnsiValue(90), Color::AnsiValue(164), Color::AnsiValue(201), Color::Magenta]
            } else {
                [Color::Rgb { r: 130, g: 60, b: 12 }, Color::Rgb { r: 195, g: 90, b: 17 }, ORANGE, ORANGE]
            };
            (char::from_u32(0x2800 + token % 0x100).unwrap(), colors[(token % 0x400 / 0x100) as usize])
        }
        '\u{ef00}'..='\u{ef17}' => {
            let token = ch as u32 - 0xef00;
            let glyph = ['←', '→', '↖', '↗', '↙', '↘'].get((token % 8) as usize).copied().unwrap_or(' ');
            (glyph, [Color::Yellow, Color::Magenta, ORANGE][(token / 8) as usize])
        }
        '\u{ef20}'..='\u{ef25}' => {
            let token = ch as u32 - 0xef20;
            let color = [Color::Yellow, Color::Magenta, ORANGE][(token / 2) as usize];
            (if token % 2 == 0 { '✧' } else { '·' }, color)
        }
        '\u{e400}'..='\u{e6ff}' => {
            let token = ch as u32 - 0xe400;
            let color = match token / 0x100 { 0 => Color::Green, 1 => Color::Cyan, _ => Color::Yellow };
            (char::from_u32(token % 0x100).unwrap(), color)
        }
        '\u{e101}'..='\u{e3ff}' => {
            let token = ch as u32 - 0xe100;
            let color = match token / 0x100 { 0 => Color::AnsiValue(178), 1 => Color::AnsiValue(220), _ => Color::Yellow };
            (char::from_u32(0x2800 + token % 0x100).unwrap(), color)
        }
        '←' | '→' | '↖' | '↗' | '↙' | '↘' => (ch, Color::Yellow),
        '✶' => ('*', Color::Cyan),
        '✧' => (ch, Color::Cyan),
        '·' => (ch, Color::DarkCyan),
        '▉' => ('█', Color::Cyan),
        '▔' => ('▀', Color::Cyan),
        '▁' => ('▄', Color::Cyan),
        '║' => ('|', Color::Green),
        '╏' => ('|', Color::Green),
        '┊' => ('|', Color::DarkGreen),
        'G' | 'O' | 'A' | 'L' | '!' | '<' | '>' => (ch, Color::Green),
        '░' => ('█', Color::DarkGrey),
        '╵' => ('▀', Color::DarkGrey),
        '╷' => ('▄', Color::DarkGrey),
        '▓' => ('█', Color::Red),
        '▴' => ('▀', Color::Red),
        '▾' => ('▄', Color::Red),
        ch if ('\u{2801}'..='\u{28ff}').contains(&ch) => (ch, Color::Yellow),
        ch => (ch, Color::White),
    }
}
