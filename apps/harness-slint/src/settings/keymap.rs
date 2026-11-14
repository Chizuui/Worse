//! Keyboard shortcuts (spec phase-9-parity.md §6.2 KEY-001..005): `ZCode`'s command table and default bindings
//! (`packages/shared/src/shortcutCommands.ts`), its binding format (`CmdOrCtrl+Shift+l`), reserved combinations
//! (`packages/ui/src/shortcuts/conflicts.ts`) and the recorder rules. Overrides live in `settings.json` `shortcuts`.

use std::collections::BTreeMap;

use harness_settings::shortcuts::normalize;

/// Command scope: global bindings need a modifier; composer bindings only apply in the message box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    Composer,
}

/// (command id, scope, default bindings), in the order the settings page lists them.
pub const COMMANDS: &[(&str, Scope, &[&str])] = &[
    ("newTask", Scope::Global, &["CmdOrCtrl+n"]),
    ("openWorkspace", Scope::Global, &["CmdOrCtrl+o"]),
    ("closeActiveContext", Scope::Global, &["CmdOrCtrl+w"]),
    ("composerSend", Scope::Composer, &["Enter"]),
    ("composerInsertNewline", Scope::Composer, &["Shift+Enter"]),
    (
        "openCommandCenter",
        Scope::Global,
        &["CmdOrCtrl+k", "CmdOrCtrl+Shift+p"],
    ),
    ("openSettings", Scope::Global, &["CmdOrCtrl+,"]),
    ("findInTask", Scope::Global, &["CmdOrCtrl+f"]),
    ("toggleSidebar", Scope::Global, &["CmdOrCtrl+b"]),
    ("switchTheme", Scope::Global, &["CmdOrCtrl+Shift+l"]),
    ("toggleTerminal", Scope::Global, &["CmdOrCtrl+j"]),
    ("toggleSidePane", Scope::Global, &["CmdOrCtrl+Alt+b"]),
    (
        "toggleSidePaneFullView",
        Scope::Global,
        &["CmdOrCtrl+Shift+f"],
    ),
    (
        "previousConversation",
        Scope::Global,
        &["CmdOrCtrl+Shift+["],
    ),
    ("nextConversation", Scope::Global, &["CmdOrCtrl+Shift+]"]),
    ("navigateBack", Scope::Global, &["CmdOrCtrl+["]),
    ("navigateForward", Scope::Global, &["CmdOrCtrl+]"]),
    ("openModelMenu", Scope::Global, &["Ctrl+m"]),
    ("cycleSessionMode", Scope::Global, &["Ctrl+Shift+m"]),
    ("cycleThoughtLevel", Scope::Global, &["Ctrl+t"]),
    ("zoomIn", Scope::Global, &["CmdOrCtrl+="]),
    ("zoomOut", Scope::Global, &["CmdOrCtrl+-"]),
    ("resetZoom", Scope::Global, &["CmdOrCtrl+0"]),
    ("openOnboarding", Scope::Global, &["CmdOrCtrl+Shift+o"]),
];

const MAC: bool = cfg!(target_os = "macos");

/// Slint key codes (private-use characters, see `slint::platform::Key`).
const KEY_BACKSPACE: char = '\u{8}';
const KEY_ESCAPE: char = '\u{1b}';
const KEY_DELETE: char = '\u{7f}';
const NAMED: &[(char, &str)] = &[
    ('\n', "Enter"),
    ('\r', "Enter"),
    ('\u{f700}', "ArrowUp"),
    ('\u{f701}', "ArrowDown"),
    ('\u{f702}', "ArrowLeft"),
    ('\u{f703}', "ArrowRight"),
    ('\u{f727}', "Insert"),
    ('\u{f729}', "Home"),
    ('\u{f72b}', "End"),
    ('\u{f72c}', "PageUp"),
    ('\u{f72d}', "PageDown"),
    (KEY_DELETE, "Delete"),
];
/// Modifier keys pressed alone (Shift, Control, Alt, `AltGr`, Meta…): ignored by the recorder.
const MODIFIER_KEYS: &[char] = &[
    '\u{10}', '\u{11}', '\u{12}', '\u{13}', '\u{14}', '\u{15}', '\u{16}', '\u{17}', '\u{18}',
];
/// Shifted US punctuation back to the unshifted key (`ZCode` stores the physical key).
const SHIFTED: &[(char, char)] = &[
    ('{', '['),
    ('}', ']'),
    ('+', '='),
    ('_', '-'),
    ('<', ','),
    ('>', '.'),
    ('?', '/'),
    (':', ';'),
    ('"', '\''),
    ('|', '\\'),
    ('~', '`'),
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
];

/// What a key press means to the recorder.
#[derive(Debug, PartialEq, Eq)]
pub enum Press {
    /// Esc without modifiers.
    Escape,
    /// Backspace without modifiers.
    Backspace,
    /// A modifier on its own (wait for the real key).
    ModifierOnly,
    /// Not a key `ZCode` can bind.
    Invalid,
    /// Binding string in `ZCode` format.
    Binding(String),
}

fn key_name(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if let Some((_, name)) = NAMED.iter().find(|(k, _)| *k == c) {
        return Some((*name).to_string());
    }
    if ('\u{f704}'..='\u{f70f}').contains(&c) {
        return Some(format!("F{}", u32::from(c) - 0xf704 + 1));
    }
    let c = SHIFTED.iter().find(|(s, _)| *s == c).map_or(c, |(_, k)| *k);
    // Ctrl+letter can arrive as a control character on some platforms.
    let c = if ('\u{1}'..='\u{1a}').contains(&c) {
        char::from(b'a' + u8::try_from(u32::from(c) - 1).unwrap_or(0))
    } else {
        c.to_ascii_lowercase()
    };
    (c.is_ascii_lowercase() || c.is_ascii_digit() || "[]=-,./;'\\`".contains(c))
        .then(|| c.to_string())
}

/// Turns a Slint key event into a binding (`CmdOrCtrl`, `Ctrl`, `Alt`, `Shift` order, like `ZCode`).
#[allow(clippy::fn_params_excessive_bools)] // Slint KeyEvent modifiers, one flag each.
pub fn press(text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Press {
    let Some(first) = text.chars().next() else {
        return Press::Invalid;
    };
    if MODIFIER_KEYS.contains(&first) {
        return Press::ModifierOnly;
    }
    let plain = !ctrl && !alt && !shift && !meta;
    if plain && first == KEY_ESCAPE {
        return Press::Escape;
    }
    if plain && first == KEY_BACKSPACE {
        return Press::Backspace;
    }
    let Some(key) = key_name(text) else {
        return Press::Invalid;
    };
    let mut parts: Vec<String> = Vec::new();
    // CmdOrCtrl = Ctrl on Windows/Linux, Cmd on macOS; on macOS a physical Ctrl stays `Ctrl`.
    let (cmd_or_ctrl, plain_ctrl) = if MAC { (meta, ctrl) } else { (ctrl, false) };
    if cmd_or_ctrl {
        parts.push("CmdOrCtrl".into());
    }
    if plain_ctrl {
        parts.push("Ctrl".into());
    }
    if alt {
        parts.push("Alt".into());
    }
    if shift {
        parts.push("Shift".into());
    }
    parts.push(key);
    Press::Binding(parts.join("+"))
}

/// Comparison key: on Windows/Linux `Ctrl+m` and `CmdOrCtrl+m` are the same keys.
pub fn same(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

fn has_modifier(binding: &str) -> bool {
    binding.contains('+')
}

/// `ZCode` RESERVED: clipboard/undo/select/save/print/address bar, reload, devtools, function keys, bare arrows.
pub fn reserved(binding: &str, scope: Scope) -> bool {
    let mut list: Vec<String> = ["c", "v", "x", "z", "a", "y", "s", "p", "l", "r"]
        .iter()
        .map(|k| format!("CmdOrCtrl+{k}"))
        .collect();
    for extra in [
        "CmdOrCtrl+Shift+z",
        "CmdOrCtrl+Shift+r",
        "CmdOrCtrl+Shift+i",
        "CmdOrCtrl+Shift+j",
        "CmdOrCtrl+Shift+c",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
    ] {
        list.push(extra.into());
    }
    for n in 1..=12 {
        list.push(format!("F{n}"));
        list.push(format!("CmdOrCtrl+F{n}"));
        list.push(format!("CmdOrCtrl+Shift+F{n}"));
    }
    if MAC {
        list.push("CmdOrCtrl+m".into());
    }
    if scope == Scope::Global {
        list.push("Enter".into());
    }
    list.iter().any(|r| same(r, binding))
}

/// Display form: `CmdOrCtrl+Shift+l` → `Ctrl+Shift+L` (`⌘⇧L` on macOS).
pub fn display(binding: &str) -> String {
    let parts: Vec<String> = binding
        .split('+')
        .filter(|p| !p.is_empty())
        .map(|p| match p {
            "CmdOrCtrl" => if MAC { "⌘" } else { "Ctrl" }.to_string(),
            "Ctrl" => if MAC { "⌃" } else { "Ctrl" }.to_string(),
            "Alt" => if MAC { "⌥" } else { "Alt" }.to_string(),
            "Shift" => if MAC { "⇧" } else { "Shift" }.to_string(),
            "ArrowUp" => "↑".into(),
            "ArrowDown" => "↓".into(),
            "ArrowLeft" => "←".into(),
            "ArrowRight" => "→".into(),
            key if key.chars().count() == 1 => key.to_uppercase(),
            key => key.to_string(),
        })
        .collect();
    // 尾键为 "+" 时 split 会产生空段：ZCode 用 "=" 存放加号键，这里不会出现。
    parts.join(if MAC { "" } else { "+" })
}

/// Effective keymap: defaults overlaid by `overrides` (an override replaces all bindings of its command).
pub struct Keymap {
    pub rows: Vec<(&'static str, Scope, Vec<String>, bool)>,
}

impl Keymap {
    pub fn new(overrides: &BTreeMap<String, Vec<String>>) -> Self {
        let rows = COMMANDS
            .iter()
            .map(|(id, scope, defaults)| match overrides.get(*id) {
                Some(custom) => (*id, *scope, custom.clone(), true),
                None => (
                    *id,
                    *scope,
                    defaults.iter().map(|b| (*b).to_string()).collect(),
                    false,
                ),
            })
            .collect();
        Self { rows }
    }

    pub fn scope(&self, id: &str) -> Scope {
        self.rows
            .iter()
            .find(|r| r.0 == id)
            .map_or(Scope::Global, |r| r.1)
    }

    pub fn bindings(&self, id: &str) -> Vec<String> {
        self.rows
            .iter()
            .find(|r| r.0 == id)
            .map(|r| r.2.clone())
            .unwrap_or_default()
    }

    /// Command of the same scope that already uses `binding` (other than `except`).
    pub fn owner(&self, binding: &str, scope: Scope, except: &str) -> Option<&'static str> {
        self.rows
            .iter()
            .find(|r| r.0 != except && r.1 == scope && r.2.iter().any(|b| same(b, binding)))
            .map(|r| r.0)
    }

    /// Global command bound to `binding`.
    pub fn command_for(&self, binding: &str) -> Option<&'static str> {
        self.owner(binding, Scope::Global, "")
    }
}

/// Recorder verdict for `binding` on command `id`.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// Error code shown by the page (`noModifier`, `reserved`, `duplicate`).
    Error(&'static str),
    /// Used by another command: offer to steal it.
    Occupied(&'static str),
}

pub fn check(keymap: &Keymap, id: &str, binding: &str) -> Verdict {
    let scope = keymap.scope(id);
    if scope == Scope::Global && !has_modifier(binding) {
        return Verdict::Error("noModifier");
    }
    if reserved(binding, scope) {
        return Verdict::Error("reserved");
    }
    if keymap.bindings(id).iter().any(|b| same(b, binding)) {
        return Verdict::Error("duplicate");
    }
    match keymap.owner(binding, scope, id) {
        Some(owner) => Verdict::Occupied(owner),
        None => Verdict::Ok,
    }
}
