//! POST /computer — the desktop as a tool a MODEL can drive.
//!
//! `/hid` is the right shape for a person's browser: it forwards what a mouse
//! and a keyboard already produced, as normalized pointer positions and Linux
//! keycodes. A vision model has neither. It looks at a screenshot, names a
//! point in that picture's pixels, and wants to say "type hello" or
//! "ctrl+l", and it needs to SEE what its action did before it decides the
//! next one. Each of those is a translation a model cannot do reliably for
//! itself (a fraction of the screen, an evdev code, when to look again), so
//! this endpoint does them here, where the screen size, the keymap and the
//! guest's own repaint are all known.
//!
//! One call is one action and its outcome: the input is injected through the
//! same path /hid uses, the request is held while the guest runs (it is never
//! answered from inside the event loop's turn, so the machine keeps running
//! and the screen can change), and it is answered with a JPEG of the screen
//! once the picture has stopped changing, or at the caller's deadline,
//! whichever comes first. That turns "act, wait, look" into one round trip,
//! which matters when every step of the caller's loop is a model generation.
//!
//! Body: `{"action": "...", ...}`. Coordinates are PIXELS of the screenshot
//! (x from the left, y from the top) unless the body names a `grid`, in which
//! case they are on a 0..grid scale across each axis — the convention some
//! vision models are trained on (Qwen's 0..1000). Actions:
//!
//!   screenshot                         look only
//!   move        x, y                   pointer to a point
//!   click       x?, y?, button?, count?   (button left|right|middle, count 1..3)
//!   double_click / right_click / middle_click   x?, y?
//!   mouse_down / mouse_up   x?, y?, button?
//!   drag        x, y, to_x, to_y, button?
//!   scroll      x?, y?, direction (up|down|left|right), amount? (notches, 1..30)
//!   type        text                   US keyboard, printable ASCII + \n \t
//!   key         keys                   "Return", "ctrl+l", "ctrl+shift+t", or
//!                                      several separated by spaces: "ctrl+a Delete"
//!   wait        ms                     let time pass, then look
//!
//! Optional on every action: `wait_ms` (the longest to wait for the screen and
//! the guest to settle, default 8000, max 20000; a screenshot waits only when
//! given one), `quality` (JPEG, 30..95, default 70).
//! `coordinate: [x, y]` and `to_coordinate: [x, y]` are accepted for x/y and
//! to_x/to_y, the spelling computer-use tool schemas tend to use.
//!
//! Authority: none beyond /hid's (the same api_key gate, the same virtual
//! input device). It is a translator, not a new door.

use std::time::Duration;

/// One input event, as the virtio-input device takes it. lib.rs injects a
/// list of these (each followed by EV_SYN) exactly as /hid does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ev {
    /// absolute pointer position, in the device's raw axis units
    Abs(u32, u32),
    /// a mouse button (BTN_LEFT/RIGHT/MIDDLE) or a key (evdev code)
    Key(u16, bool),
    /// wheel notches: vertical (positive = up), horizontal (positive = right)
    Wheel(i32, i32),
}

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

const KEY_LEFTSHIFT: u16 = 42;

/// The longest text one `type` call takes. Each character is two to four
/// events, and the device queue holds 4096; past this a call is better split
/// so each part is checked on screen anyway.
pub const MAX_TYPE_CHARS: usize = 800;
/// Default and ceiling for how long a call waits for the screen to settle.
/// Generous on purpose: the caller is a model whose every step is a
/// generation of a minute or more, so a second spent waiting for an app to
/// finish drawing is cheap next to a step wasted on a half-drawn screen.
pub const DEFAULT_WAIT_MS: u64 = 8000;
pub const MAX_WAIT_MS: u64 = 20_000;
/// The guest gets at least this long to react before the screen is judged.
/// An emulated core takes a few hundred ms to turn a click into pixels; a
/// picture taken sooner would show the screen BEFORE the action and the
/// caller would conclude the action did nothing.
pub const MIN_REACT_MS: u64 = 600;
/// Settled = neither the screen nor the guest's CPU has been busy for this
/// long (see COMPUTER_BUSY_IPS in lib.rs).
pub const SETTLE_MS: u64 = 500;
/// How often a held call samples the screen while it waits.
pub const SAMPLE_MS: u64 = 100;

/// What one call does, decided entirely before anything is injected: a bad
/// argument fails the call with nothing half-done on the machine.
#[derive(Debug)]
pub struct Plan {
    pub action: String,
    pub events: Vec<Ev>,
    /// what was done, in words, for the caller to read beside the picture
    pub did: String,
    /// earliest and latest moments (from now) to answer
    pub min: Duration,
    pub max: Duration,
    /// answer at `max` whatever the screen does (the `wait` action)
    pub fixed: bool,
    pub quality: u8,
    pub grid: Option<f64>,
}

/// The screen a plan is made against: its size in pixels and the device's
/// axis maximum (positions are sent as 0..abs_max on each axis).
#[derive(Clone, Copy)]
pub struct Screen {
    pub w: usize,
    pub h: usize,
    pub abs_max: u32,
}

impl Screen {
    /// The raw axis value for a pixel. The X server maps 0..abs_max onto
    /// 0..(size-1), so this is the inverse of that, rounded.
    fn axis(&self, px: f64, size: usize) -> u32 {
        let span = (size.max(2) - 1) as f64;
        let px = px.clamp(0.0, span);
        ((px * self.abs_max as f64) / span).round() as u32
    }

    fn abs(&self, p: (f64, f64)) -> Ev {
        Ev::Abs(self.axis(p.0, self.w), self.axis(p.1, self.h))
    }
}

/// Parse a request body into a plan against `screen`.
pub fn plan(body: &[u8], screen: Screen) -> Result<Plan, String> {
    let v: serde_json::Value = if body.iter().all(|b| b.is_ascii_whitespace()) {
        serde_json::json!({ "action": "screenshot" })
    } else {
        serde_json::from_slice(body).map_err(|e| format!("bad JSON: {e}"))?
    };
    let action = v
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("screenshot")
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_");
    let grid = match num(&v, "grid") {
        Some(g) if g >= 1.0 => Some(g),
        Some(_) => return Err("grid must be at least 1".into()),
        None => None,
    };
    let quality = num(&v, "quality").map(|q| q.clamp(30.0, 95.0) as u8).unwrap_or(70);
    let wait = num(&v, "wait_ms")
        .map(|w| w.clamp(0.0, MAX_WAIT_MS as f64) as u64)
        .unwrap_or(DEFAULT_WAIT_MS);
    // a point in the caller's space -> screen pixels
    let to_px = |p: (f64, f64)| -> (f64, f64) {
        match grid {
            Some(g) => (p.0 / g * screen.w as f64, p.1 / g * screen.h as f64),
            None => p,
        }
    };
    let at = point(&v, "x", "y", "coordinate")?;
    let fmt = |p: (f64, f64)| format!("({}, {})", trim_num(p.0), trim_num(p.1));
    let mut ev = Vec::new();
    let mut fixed = false;
    let mut min_ms = MIN_REACT_MS;
    let did = match action.as_str() {
        "screenshot" | "look" | "observe" => {
            // a look waits only when asked to: the screen is what it is
            min_ms = 0;
            if num(&v, "wait_ms").is_none() {
                return Ok(Plan {
                    action,
                    events: ev,
                    did: "took a screenshot".into(),
                    min: Duration::ZERO,
                    max: Duration::ZERO,
                    fixed: false,
                    quality,
                    grid,
                });
            }
            "took a screenshot".to_string()
        }
        "move" | "mouse_move" | "hover" => {
            let p = at.ok_or("move needs x and y")?;
            ev.push(screen.abs(to_px(p)));
            format!("moved the pointer to {}", fmt(p))
        }
        "click" | "left_click" | "double_click" | "triple_click" | "right_click"
        | "middle_click" => {
            let (button, count) = match action.as_str() {
                "double_click" => (button_of(&v, BTN_LEFT)?, 2),
                "triple_click" => (button_of(&v, BTN_LEFT)?, 3),
                "right_click" => (BTN_RIGHT, 1),
                "middle_click" => (BTN_MIDDLE, 1),
                _ => (button_of(&v, BTN_LEFT)?, num(&v, "count").unwrap_or(1.0).clamp(1.0, 3.0) as usize),
            };
            if let Some(p) = at {
                ev.push(screen.abs(to_px(p)));
            }
            for _ in 0..count {
                ev.push(Ev::Key(button, true));
                ev.push(Ev::Key(button, false));
            }
            let what = match count {
                1 => "clicked",
                2 => "double-clicked",
                _ => "triple-clicked",
            };
            format!(
                "{what} the {} button{}",
                button_name(button),
                at.map(|p| format!(" at {}", fmt(p))).unwrap_or_else(|| " where the pointer was".into())
            )
        }
        "mouse_down" | "left_mouse_down" | "mouse_up" | "left_mouse_up" => {
            let down = action.ends_with("down");
            let button = button_of(&v, BTN_LEFT)?;
            if let Some(p) = at {
                ev.push(screen.abs(to_px(p)));
            }
            ev.push(Ev::Key(button, down));
            format!(
                "{} the {} button{}",
                if down { "pressed" } else { "released" },
                button_name(button),
                at.map(|p| format!(" at {}", fmt(p))).unwrap_or_default()
            )
        }
        "drag" | "left_click_drag" => {
            let from = at.ok_or("drag needs x and y (where it starts)")?;
            let to = point(&v, "to_x", "to_y", "to_coordinate")?
                .ok_or("drag needs to_x and to_y (where it ends)")?;
            let button = button_of(&v, BTN_LEFT)?;
            let (a, b) = (to_px(from), to_px(to));
            ev.push(screen.abs(a));
            ev.push(Ev::Key(button, true));
            // intermediate motion, so toolkits that only start a drag after
            // the pointer has travelled a few pixels while held see it travel
            const STEPS: usize = 8;
            for i in 1..=STEPS {
                let t = i as f64 / STEPS as f64;
                ev.push(screen.abs((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)));
            }
            ev.push(Ev::Key(button, false));
            format!("dragged with the {} button from {} to {}", button_name(button), fmt(from), fmt(to))
        }
        "scroll" => {
            let dir = v
                .get("direction")
                .and_then(|d| d.as_str())
                .unwrap_or("down")
                .trim()
                .to_ascii_lowercase();
            let n = num(&v, "amount").unwrap_or(3.0).clamp(1.0, 30.0) as i32;
            let (dy, dx) = match dir.as_str() {
                "up" => (n, 0),
                "down" => (-n, 0),
                "left" => (0, -n),
                "right" => (0, n),
                other => return Err(format!("scroll direction must be up, down, left or right (got \"{other}\")")),
            };
            if let Some(p) = at {
                ev.push(screen.abs(to_px(p)));
            }
            // one notch per report: the guest's stack ignores a single report
            // carrying several (measured: -5 in one event moved nothing; five
            // events of -1 scrolled the page)
            for _ in 0..n {
                ev.push(Ev::Wheel(dy.signum(), dx.signum()));
            }
            format!(
                "scrolled {dir} {n} notch{}{}",
                if n == 1 { "" } else { "es" },
                at.map(|p| format!(" at {}", fmt(p))).unwrap_or_default()
            )
        }
        "type" | "type_text" | "write" => {
            let text = v.get("text").and_then(|t| t.as_str()).ok_or("type needs text")?;
            if text.is_empty() {
                return Err("type needs non-empty text".into());
            }
            let n = text.chars().count();
            if n > MAX_TYPE_CHARS {
                return Err(format!(
                    "text is {n} characters; type at most {MAX_TYPE_CHARS} per call and check the screen between parts"
                ));
            }
            ev.extend(type_events(text)?);
            format!("typed {n} character{}", if n == 1 { "" } else { "s" })
        }
        "key" | "keys" | "press" | "hotkey" | "key_press" => {
            let keys = v
                .get("keys")
                .or_else(|| v.get("key"))
                .or_else(|| v.get("text"))
                .and_then(|k| k.as_str())
                .ok_or("key needs keys, e.g. \"Return\" or \"ctrl+l\"")?;
            ev.extend(key_events(keys)?);
            format!("pressed {}", keys.trim())
        }
        "wait" | "sleep" => {
            // the wait's length: `ms`, `duration` (seconds), or - since a tool
            // schema offering one timing field will use it for this too -
            // `wait_ms`
            let ms = num(&v, "ms")
                .or_else(|| num(&v, "duration").map(|s| s * 1000.0))
                .or_else(|| num(&v, "wait_ms"))
                .unwrap_or(1000.0)
                .clamp(0.0, MAX_WAIT_MS as f64) as u64;
            fixed = true;
            return Ok(Plan {
                action,
                events: ev,
                did: format!("waited {ms} ms"),
                min: Duration::from_millis(ms),
                max: Duration::from_millis(ms),
                fixed,
                quality,
                grid,
            });
        }
        other => {
            return Err(format!(
                "unknown action \"{other}\"; use screenshot, move, click, double_click, right_click, \
                 middle_click, mouse_down, mouse_up, drag, scroll, type, key or wait"
            ))
        }
    };
    let max_ms = wait.max(min_ms);
    Ok(Plan {
        action,
        events: ev,
        did,
        min: Duration::from_millis(min_ms.min(max_ms)),
        max: Duration::from_millis(max_ms),
        fixed,
        quality,
        grid,
    })
}

/// A number field, tolerating a numeric string (models write "412").
fn num(v: &serde_json::Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// x/y, or the [x, y] array spelling. Half a point is an error, not a guess.
fn point(v: &serde_json::Value, kx: &str, ky: &str, karr: &str) -> Result<Option<(f64, f64)>, String> {
    if let Some(a) = v.get(karr).and_then(|a| a.as_array()) {
        let n = |i: usize| match a.get(i) {
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        };
        return match (n(0), n(1)) {
            (Some(x), Some(y)) if a.len() == 2 => Ok(Some((x, y))),
            _ => Err(format!("{karr} must be [x, y]")),
        };
    }
    match (num(v, kx), num(v, ky)) {
        (Some(x), Some(y)) => Ok(Some((x, y))),
        (None, None) => Ok(None),
        _ => Err(format!("give both {kx} and {ky}")),
    }
}

fn button_of(v: &serde_json::Value, default: u16) -> Result<u16, String> {
    match v.get("button").and_then(|b| b.as_str()).map(|b| b.trim().to_ascii_lowercase()) {
        None => Ok(default),
        Some(b) => match b.as_str() {
            "left" | "1" => Ok(BTN_LEFT),
            "right" | "3" => Ok(BTN_RIGHT),
            "middle" | "2" => Ok(BTN_MIDDLE),
            other => Err(format!("button must be left, right or middle (got \"{other}\")")),
        },
    }
}

fn button_name(b: u16) -> &'static str {
    match b {
        BTN_RIGHT => "right",
        BTN_MIDDLE => "middle",
        _ => "left",
    }
}

fn trim_num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{}", x as i64)
    } else {
        format!("{x:.1}")
    }
}

/// The evdev code for a printable ASCII character on a US layout, and
/// whether it needs shift.
pub fn char_key(c: char) -> Option<(u16, bool)> {
    const ROW_DIGITS: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10]; // 0..9
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    let letter = |c: char| -> u16 {
        match c {
            'a' => 30, 'b' => 48, 'c' => 46, 'd' => 32, 'e' => 18, 'f' => 33, 'g' => 34,
            'h' => 35, 'i' => 23, 'j' => 36, 'k' => 37, 'l' => 38, 'm' => 50, 'n' => 49,
            'o' => 24, 'p' => 25, 'q' => 16, 'r' => 19, 's' => 31, 't' => 20, 'u' => 22,
            'v' => 47, 'w' => 17, 'x' => 45, 'y' => 21, _ => 44, // 'z'
        }
    };
    Some(match c {
        'a'..='z' => (letter(c), false),
        'A'..='Z' => (letter(c.to_ascii_lowercase()), true),
        '0'..='9' => (ROW_DIGITS[c as usize - '0' as usize], false),
        _ if SHIFTED_DIGITS.contains(c) => {
            let i = SHIFTED_DIGITS.find(c).unwrap();
            (ROW_DIGITS[i], true)
        }
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        _ => return None,
    })
}

/// Key events that type `text`. Every character is checked BEFORE any event
/// exists, so an untypeable character fails the call with nothing typed.
pub fn type_events(text: &str) -> Result<Vec<Ev>, String> {
    let text = text.replace("\r\n", "\n");
    let bad: Vec<String> = text
        .chars()
        .filter(|&c| char_key(c).is_none())
        .map(|c| format!("{c:?}"))
        .collect();
    if !bad.is_empty() {
        let mut uniq = bad.clone();
        uniq.dedup();
        return Err(format!(
            "cannot type {} - only printable ASCII, newline and tab exist on this keyboard; \
             nothing was typed",
            uniq.join(", ")
        ));
    }
    let mut ev = Vec::new();
    for c in text.chars() {
        let (code, shift) = char_key(c).expect("checked above");
        if shift {
            ev.push(Ev::Key(KEY_LEFTSHIFT, true));
        }
        ev.push(Ev::Key(code, true));
        ev.push(Ev::Key(code, false));
        if shift {
            ev.push(Ev::Key(KEY_LEFTSHIFT, false));
        }
    }
    Ok(ev)
}

/// The evdev code for one key NAME (case-insensitive; xdotool's names and
/// the obvious aliases), or a single character.
pub fn named_key(name: &str) -> Option<(u16, bool)> {
    let n = name.trim();
    if n.chars().count() == 1 {
        let c = n.chars().next().unwrap();
        // a lone letter names the KEY, not the capital: "ctrl+T" is ctrl+t
        return char_key(c.to_ascii_lowercase()).map(|(k, s)| (k, s && !c.is_ascii_alphabetic()));
    }
    let l = n.to_ascii_lowercase().replace(['-', ' '], "_");
    let code = match l.as_str() {
        "ctrl" | "control" | "ctl" | "control_l" | "ctrl_l" | "lctrl" => 29,
        "control_r" | "ctrl_r" | "rctrl" => 97,
        "shift" | "shift_l" | "lshift" => 42,
        "shift_r" | "rshift" => 54,
        "alt" | "alt_l" | "lalt" | "option" | "meta_l" => 56,
        "alt_r" | "altgr" | "ralt" | "iso_level3_shift" => 100,
        "super" | "super_l" | "win" | "windows" | "cmd" | "command" | "meta" | "logo" => 125,
        "super_r" => 126,
        "return" | "enter" | "kp_enter" => 28,
        "escape" | "esc" => 1,
        "tab" => 15,
        "backspace" | "back_space" => 14,
        "delete" | "del" => 111,
        "insert" | "ins" => 110,
        "home" => 102,
        "end" => 107,
        "page_up" | "pageup" | "prior" | "pgup" => 104,
        "page_down" | "pagedown" | "next" | "pgdn" => 109,
        "up" | "arrowup" | "arrow_up" => 103,
        "down" | "arrowdown" | "arrow_down" => 108,
        "left" | "arrowleft" | "arrow_left" => 105,
        "right" | "arrowright" | "arrow_right" => 106,
        "space" | "spacebar" => 57,
        "menu" | "context_menu" | "apps" => 127,
        "print" | "printscreen" | "print_screen" | "sysrq" => 99,
        "pause" | "break" => 119,
        "caps_lock" | "capslock" => 58,
        "num_lock" | "numlock" => 69,
        "scroll_lock" | "scrolllock" => 70,
        "minus" => 12,
        "equal" | "equals" => 13,
        "plus" => return Some((13, true)),
        "bracketleft" => 26,
        "bracketright" => 27,
        "semicolon" => 39,
        "apostrophe" | "quote" => 40,
        "grave" | "backtick" => 41,
        "backslash" => 43,
        "comma" => 51,
        "period" | "dot" => 52,
        "slash" => 53,
        f if f.starts_with('f') && f.len() <= 3 => match f[1..].parse::<u16>().ok()? {
            n @ 1..=10 => 58 + n,
            11 => 87,
            12 => 88,
            _ => return None,
        },
        _ => return None,
    };
    Some((code, false))
}

/// Key events for a key spec: chords joined by '+', several separated by
/// whitespace ("ctrl+a Delete"). Modifiers are pressed in order and released
/// in reverse, around the chord's last key.
pub fn key_events(spec: &str) -> Result<Vec<Ev>, String> {
    let mut ev = Vec::new();
    let chords: Vec<&str> = spec.split_whitespace().collect();
    if chords.is_empty() {
        return Err("keys is empty; name a key like \"Return\" or a chord like \"ctrl+l\"".into());
    }
    if chords.len() > 32 {
        return Err("at most 32 key presses per call; use type for text".into());
    }
    for chord in chords {
        // "+" alone, or a chord ending in "++" ("ctrl++"), means the plus key
        let parts: Vec<String> = if chord == "+" {
            vec!["+".into()]
        } else if let Some(head) = chord.strip_suffix("++") {
            head.split('+').map(str::to_string).chain(std::iter::once("+".to_string())).collect()
        } else {
            chord.split('+').map(str::to_string).collect()
        };
        let mut codes = Vec::new();
        for p in &parts {
            if p.is_empty() {
                return Err(format!("\"{chord}\" has an empty key between its '+' signs"));
            }
            let (code, shift) =
                named_key(p).ok_or_else(|| format!("unknown key \"{p}\" in \"{chord}\""))?;
            if shift && !codes.contains(&KEY_LEFTSHIFT) {
                codes.push(KEY_LEFTSHIFT);
            }
            codes.push(code);
        }
        for &c in &codes {
            ev.push(Ev::Key(c, true));
        }
        for &c in codes.iter().rev() {
            ev.push(Ev::Key(c, false));
        }
    }
    Ok(ev)
}

/// The JSON a finished call answers with. `cursor` is in pixels; it is
/// reported in the caller's own space (pixels, or the grid it asked for).
#[allow(clippy::too_many_arguments)]
pub fn answer(
    plan: &Plan,
    w: usize,
    h: usize,
    cursor: Option<(i64, i64)>,
    settled: bool,
    changed: Option<bool>,
    waited_ms: u64,
    jpeg: &[u8],
) -> String {
    let space = |p: (f64, f64)| -> (f64, f64) {
        match plan.grid {
            Some(g) => ((p.0 / w as f64 * g).round(), (p.1 / h as f64 * g).round()),
            None => p,
        }
    };
    let cursor = cursor
        .map(|(x, y)| {
            let (x, y) = space((x as f64, y as f64));
            serde_json::json!({ "x": x as i64, "y": y as i64 })
        })
        .unwrap_or(serde_json::Value::Null);
    let coords = match plan.grid {
        Some(g) => format!("0..{} on each axis, from the top-left", trim_num(g)),
        None => format!("pixels: x 0..{} from the left, y 0..{} from the top", w.saturating_sub(1), h.saturating_sub(1)),
    };
    serde_json::json!({
        "ok": true,
        "action": plan.action,
        "did": plan.did,
        "screen": { "width": w, "height": h },
        "coordinates": coords,
        "cursor": cursor,
        "settled": settled,
        "changed": changed,
        "waited_ms": waited_ms,
        "mime": "image/jpeg",
        "image": crate::b64(jpeg),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Screen = Screen { w: 960, h: 600, abs_max: 32767 };

    fn p(body: &str) -> Result<Plan, String> {
        plan(body.as_bytes(), SCREEN)
    }

    #[test]
    fn a_click_lands_on_the_pixel_it_names() {
        let pl = p(r#"{"action":"click","x":959,"y":0}"#).unwrap();
        assert_eq!(pl.events[0], Ev::Abs(32767, 0));
        assert_eq!(pl.events[1..], [Ev::Key(BTN_LEFT, true), Ev::Key(BTN_LEFT, false)]);
        // the middle pixel maps to the middle of the axis, give or take rounding
        let pl = p(r#"{"action":"click","x":479.5,"y":299.5}"#).unwrap();
        assert_eq!(pl.events[0], Ev::Abs(16384, 16384));
        assert!(pl.min >= Duration::from_millis(MIN_REACT_MS));
    }

    #[test]
    fn a_grid_scales_to_the_screen() {
        let pl = p(r#"{"action":"move","coordinate":[500,500],"grid":1000}"#).unwrap();
        // 500/1000 of 960 = 480 px -> 480/959 of the axis
        assert_eq!(pl.events[0], Ev::Abs((480.0 * 32767.0 / 959.0f64).round() as u32, (300.0 * 32767.0 / 599.0f64).round() as u32));
        let j: serde_json::Value = serde_json::from_str(&answer(&pl, 960, 600, Some((480, 300)), true, Some(true), 700, b"x")).unwrap();
        assert_eq!(j["cursor"], serde_json::json!({"x": 500, "y": 500}));
        assert!(j["coordinates"].as_str().unwrap().starts_with("0..1000"));
    }

    #[test]
    fn numbers_may_arrive_as_strings_and_half_a_point_is_refused() {
        let pl = p(r#"{"action":"move","x":"10","y":"20"}"#).unwrap();
        assert_eq!(pl.events.len(), 1);
        assert!(p(r#"{"action":"move","x":10}"#).unwrap_err().contains("both"));
        assert!(p(r#"{"action":"move"}"#).is_err());
    }

    #[test]
    fn double_and_right_clicks() {
        let pl = p(r#"{"action":"double_click","x":1,"y":1}"#).unwrap();
        assert_eq!(pl.events.iter().filter(|e| **e == Ev::Key(BTN_LEFT, true)).count(), 2);
        let pl = p(r#"{"action":"right_click"}"#).unwrap();
        assert_eq!(pl.events, [Ev::Key(BTN_RIGHT, true), Ev::Key(BTN_RIGHT, false)]);
        assert!(pl.did.contains("where the pointer was"));
        assert!(p(r#"{"action":"click","button":"fourth"}"#).is_err());
    }

    #[test]
    fn typing_uses_shift_for_capitals_and_symbols() {
        let ev = type_events("aA!").unwrap();
        assert_eq!(
            ev,
            [
                Ev::Key(30, true), Ev::Key(30, false),
                Ev::Key(42, true), Ev::Key(30, true), Ev::Key(30, false), Ev::Key(42, false),
                Ev::Key(42, true), Ev::Key(2, true), Ev::Key(2, false), Ev::Key(42, false),
            ]
        );
        // every printable ASCII character is typeable
        for c in (0x20u8..0x7f).map(|b| b as char) {
            assert!(char_key(c).is_some(), "{c:?}");
        }
    }

    #[test]
    fn untypeable_text_types_nothing() {
        let e = p(r#"{"action":"type","text":"price: 5€"}"#).unwrap_err();
        assert!(e.contains("'€'") && e.contains("nothing was typed"), "{e}");
        let long = "a".repeat(MAX_TYPE_CHARS + 1);
        assert!(p(&format!(r#"{{"action":"type","text":"{long}"}}"#)).is_err());
    }

    #[test]
    fn chords_press_in_order_and_release_in_reverse() {
        let ev = key_events("ctrl+shift+t").unwrap();
        assert_eq!(
            ev,
            [
                Ev::Key(29, true), Ev::Key(42, true), Ev::Key(20, true),
                Ev::Key(20, false), Ev::Key(42, false), Ev::Key(29, false),
            ]
        );
        // sequences, names, aliases, case
        assert_eq!(key_events("Return").unwrap(), [Ev::Key(28, true), Ev::Key(28, false)]);
        assert_eq!(key_events("ctrl+a Delete").unwrap().len(), 6);
        assert_eq!(key_events("alt+F4").unwrap()[1], Ev::Key(62, true));
        assert_eq!(key_events("F12").unwrap()[0], Ev::Key(88, true));
        assert_eq!(key_events("ctrl+T").unwrap()[1], Ev::Key(20, true), "a capital letter names the key");
        assert_eq!(key_events("Page_Down").unwrap()[0], Ev::Key(109, true));
        // the plus key, alone and in a chord
        assert_eq!(key_events("+").unwrap(), [Ev::Key(42, true), Ev::Key(13, true), Ev::Key(13, false), Ev::Key(42, false)]);
        assert_eq!(key_events("ctrl++").unwrap()[0], Ev::Key(29, true));
        assert!(key_events("ctrl+hyper").unwrap_err().contains("hyper"));
        assert!(key_events("   ").is_err());
    }

    #[test]
    fn scroll_and_drag() {
        let pl = p(r#"{"action":"scroll","direction":"down","amount":5,"x":10,"y":10}"#).unwrap();
        assert_eq!(pl.events.iter().filter(|e| **e == Ev::Wheel(-1, 0)).count(), 5);
        assert_eq!(pl.events.len(), 6, "the move, then one report per notch");
        assert_eq!(p(r#"{"action":"scroll","direction":"right"}"#).unwrap().events, [Ev::Wheel(0, 1); 3]);
        assert!(p(r#"{"action":"scroll","direction":"sideways"}"#).is_err());
        let pl = p(r#"{"action":"drag","x":0,"y":0,"to_x":959,"to_y":599}"#).unwrap();
        assert_eq!(pl.events.first(), Some(&Ev::Abs(0, 0)));
        assert_eq!(pl.events[1], Ev::Key(BTN_LEFT, true));
        assert_eq!(pl.events[pl.events.len() - 2], Ev::Abs(32767, 32767));
        assert_eq!(pl.events.last(), Some(&Ev::Key(BTN_LEFT, false)));
        assert!(p(r#"{"action":"drag","x":0,"y":0}"#).is_err());
    }

    #[test]
    fn screenshot_and_wait_shape_the_clock() {
        let pl = p("").unwrap();
        assert_eq!(pl.action, "screenshot");
        assert!(pl.events.is_empty() && pl.min == Duration::ZERO && pl.max == Duration::ZERO);
        // ...unless it is asked to wait for the screen to settle
        assert_eq!(p(r#"{"action":"screenshot","wait_ms":1500}"#).unwrap().max, Duration::from_millis(1500));
        let pl = p(r#"{"action":"wait","ms":2500}"#).unwrap();
        assert!(pl.fixed && pl.min == Duration::from_millis(2500) && pl.max == pl.min);
        assert_eq!(p(r#"{"action":"wait","wait_ms":4000}"#).unwrap().max, Duration::from_millis(4000));
        let pl = p(r#"{"action":"key","keys":"Return","wait_ms":99999}"#).unwrap();
        assert_eq!(pl.max, Duration::from_millis(MAX_WAIT_MS));
        // a wait shorter than the reaction floor is the floor
        let pl = p(r#"{"action":"key","keys":"Return","wait_ms":10}"#).unwrap();
        assert_eq!(pl.max, Duration::from_millis(MIN_REACT_MS));
        assert!(p(r#"{"action":"teleport"}"#).unwrap_err().contains("unknown action"));
    }
}
