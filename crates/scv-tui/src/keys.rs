//! The dashboard's key bindings: [`Action`]s, their defaults, and the
//! user's `[tui.keys]` table, which the server sends with `session.started`.
//!
//! A binding is a key specification such as `alt+.`, `ctrl+t`, `f1`, or
//! `ctrl+pagedown`: modifiers (`ctrl`, `alt`, `shift`) joined to one key by
//! `+`. It needs `ctrl` or `alt` unless it is a function key, and may not be
//! a key a chat or agent view uses ([`RESERVED`]). Configured bindings come
//! first; an action whose configured key the TUI cannot use, or that another
//! action's configured key took, keeps its default, or is unbound when that
//! is taken too. The TUI says which once at startup.

use std::collections::BTreeMap;

/// Keys a chat's composer or an agent view uses, which no dashboard action
/// may take: editing (`ctrl+a`/`b`/`e`/`f`/`k`/`u`/`w`, `alt+b`/`f`, word
/// movement), newlines (`ctrl+j`, `alt+enter`), cancelling and quitting
/// (`ctrl+c`), tool details (`ctrl+o`), the queue (`alt+p`, `alt+up`,
/// `alt+down`, `ctrl+x`), and an agent view's stop and resume (`ctrl+x`,
/// `ctrl+r`).
pub(crate) const RESERVED: &[&str] = &[
    "ctrl+a",
    "ctrl+b",
    "ctrl+c",
    "ctrl+e",
    "ctrl+f",
    "ctrl+j",
    "ctrl+k",
    "ctrl+o",
    "ctrl+r",
    "ctrl+u",
    "ctrl+w",
    "ctrl+x",
    "alt+b",
    "alt+f",
    "alt+p",
    "alt+enter",
    "alt+left",
    "alt+right",
    "alt+up",
    "alt+down",
];

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Something a key does in the dashboard, apart from editing a composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Action {
    /// Move focus to the sidebar, or back to the pane.
    FocusSidebar,
    /// Show or hide the sidebar.
    ToggleSidebar,
    NextTab,
    PrevTab,
    /// Open a new chat tab: the composer asks for its workspace.
    NewTab,
    CloseTab,
    /// Split the main area into two panes, or join them again.
    Split,
    /// Move focus to the other pane.
    FocusOtherPane,
    /// Show the notifications.
    Notifications,
    Help,
    /// Go to tab 1-9.
    Tab(u8),
}

impl Action {
    /// The action's name in `[tui.keys]`.
    pub(crate) fn name(self) -> String {
        match self {
            Self::FocusSidebar => "focus_sidebar".into(),
            Self::ToggleSidebar => "toggle_sidebar".into(),
            Self::NextTab => "next_tab".into(),
            Self::PrevTab => "prev_tab".into(),
            Self::NewTab => "new_tab".into(),
            Self::CloseTab => "close_tab".into(),
            Self::Split => "split".into(),
            Self::FocusOtherPane => "focus_other_pane".into(),
            Self::Notifications => "notifications".into(),
            Self::Help => "help".into(),
            Self::Tab(number) => format!("tab_{number}"),
        }
    }

    /// What the action does, for the help overlay.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::FocusSidebar => "focus the sidebar, or go back to the pane".into(),
            Self::ToggleSidebar => "show or hide the sidebar".into(),
            Self::NextTab => "next tab".into(),
            Self::PrevTab => "previous tab".into(),
            Self::NewTab => "open a chat tab in another workspace".into(),
            Self::CloseTab => "close the tab".into(),
            Self::Split => "split into two panes, or join them".into(),
            Self::FocusOtherPane => "focus the other pane".into(),
            Self::Notifications => "show notifications".into(),
            Self::Help => "keys and commands".into(),
            Self::Tab(number) => format!("go to tab {number}"),
        }
    }

    /// Every action, in help order.
    pub(crate) fn all() -> Vec<Self> {
        let mut actions = vec![
            Self::FocusSidebar,
            Self::ToggleSidebar,
            Self::NextTab,
            Self::PrevTab,
            Self::NewTab,
            Self::CloseTab,
            Self::Split,
            Self::FocusOtherPane,
            Self::Notifications,
            Self::Help,
        ];
        actions.extend((1..=9).map(Self::Tab));
        actions
    }

    fn default_key(self) -> String {
        match self {
            Self::FocusSidebar => "ctrl+g".into(),
            Self::ToggleSidebar => "alt+s".into(),
            Self::NextTab => "alt+.".into(),
            Self::PrevTab => "alt+,".into(),
            Self::NewTab => "ctrl+t".into(),
            Self::CloseTab => "alt+w".into(),
            Self::Split => "alt+v".into(),
            Self::FocusOtherPane => "alt+o".into(),
            Self::Notifications => "alt+n".into(),
            Self::Help => "f1".into(),
            Self::Tab(number) => format!("alt+{number}"),
        }
    }
}

/// One key with its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub(crate) code: KeyCode,
    pub(crate) modifiers: KeyModifiers,
}

impl Key {
    /// A pressed key, normalized: a character's case carries `shift`, and
    /// so does `BackTab`, which is how terminals send `shift+tab`.
    pub(crate) fn of(event: KeyEvent) -> Self {
        let mut modifiers =
            event.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let code = match event.code {
            KeyCode::Char(character) => {
                modifiers.remove(KeyModifiers::SHIFT);
                KeyCode::Char(character.to_ascii_lowercase())
            }
            KeyCode::BackTab => {
                modifiers.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            other => other,
        };
        Self { code, modifiers }
    }

    /// The key a specification such as `ctrl+t` names.
    pub(crate) fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim().to_ascii_lowercase();
        let mut modifiers = KeyModifiers::NONE;
        let mut parts: Vec<&str> = spec.split('+').collect();
        // `alt++` binds the plus key.
        let last = if spec.ends_with("++") {
            parts.truncate(parts.len().saturating_sub(2));
            "+"
        } else {
            parts.pop()?
        };
        for modifier in parts {
            modifiers |= match modifier {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "meta" | "option" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => return None,
            };
        }
        let code = match last {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "space" => KeyCode::Char(' '),
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "insert" => KeyCode::Insert,
            "delete" | "del" => KeyCode::Delete,
            "backspace" => KeyCode::Backspace,
            function if function.starts_with('f') && function.len() > 1 => KeyCode::F(
                function[1..]
                    .parse()
                    .ok()
                    .filter(|number| (1..=12).contains(number))?,
            ),
            single if single.chars().count() == 1 => {
                KeyCode::Char(single.chars().next()?.to_ascii_lowercase())
            }
            _ => return None,
        };
        // Pressed, a character's case and `BackTab` carry `shift`.
        let code = match code {
            KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            other => other,
        };
        if matches!(code, KeyCode::Char(_) | KeyCode::BackTab) {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        // A key without `ctrl` or `alt` is the composer's or the views'
        // own (text, Enter, Esc, arrows, ...); only function keys are free.
        if !matches!(code, KeyCode::F(_))
            && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        Some(Self { code, modifiers })
    }

    /// Whether a chat or agent view uses this key ([`RESERVED`]).
    fn reserved(self) -> bool {
        RESERVED
            .iter()
            .filter_map(|spec| Self::parse(spec))
            .any(|key| key == self)
    }
}

/// The dashboard's bindings.
#[derive(Debug, Clone)]
pub(crate) struct KeyMap {
    bindings: BTreeMap<String, (Action, Key)>,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::with(&BTreeMap::new()).0
    }
}

impl KeyMap {
    /// The defaults with the user's `configured` bindings, and a warning for
    /// each binding the TUI could not use.
    pub(crate) fn with(configured: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let known: Vec<String> = Action::all().into_iter().map(Action::name).collect();
        for name in configured.keys().filter(|name| !known.contains(name)) {
            warnings.push(format!("tui.keys.{name} is not an action the TUI knows"));
        }
        // Who holds each key: configured bindings first, in help order.
        let mut bindings: BTreeMap<String, (Action, Key)> = BTreeMap::new();
        let holder = |bindings: &BTreeMap<String, (Action, Key)>, key: Key| {
            bindings
                .iter()
                .find(|(_, (_, held))| *held == key)
                .map(|(name, _)| name.clone())
        };
        // Actions left to their defaults, with why when one was configured.
        let mut defaulted: Vec<(Action, Option<String>)> = Vec::new();
        for action in Action::all() {
            let name = action.name();
            let Some(spec) = configured.get(&name) else {
                defaulted.push((action, None));
                continue;
            };
            let refusal = match Key::parse(spec) {
                None => Some(
                    "is not a key the TUI can bind (it needs ctrl or alt, or is a function key)"
                        .to_owned(),
                ),
                Some(key) if key.reserved() => {
                    Some("is a key a chat or agent view uses".to_owned())
                }
                Some(key) => {
                    let taken = holder(&bindings, key);
                    if taken.is_none() {
                        bindings.insert(name.clone(), (action, key));
                    }
                    taken.map(|other| format!("is tui.keys.{other}'s already"))
                }
            };
            if let Some(refusal) = refusal {
                defaulted.push((
                    action,
                    Some(format!("tui.keys.{name} = {spec:?} {refusal}")),
                ));
            }
        }
        for (action, refused) in defaulted {
            let name = action.name();
            let Some(key) = Key::parse(&action.default_key()) else {
                continue;
            };
            let taken = holder(&bindings, key);
            let outcome = match &taken {
                Some(other) => format!(
                    "its default {} is tui.keys.{other}'s, so {name} has no key",
                    describe(key)
                ),
                None => format!("it keeps its default {}", describe(key)),
            };
            match refused {
                Some(refused) => warnings.push(format!("{refused}; {outcome}")),
                None if taken.is_some() => warnings.push(format!("{name}: {outcome}")),
                None => {}
            }
            if taken.is_none() {
                bindings.insert(name, (action, key));
            }
        }
        (Self { bindings }, warnings)
    }

    /// The action `event` is bound to.
    pub(crate) fn action(&self, event: KeyEvent) -> Option<Action> {
        let pressed = Key::of(event);
        self.bindings
            .values()
            .find(|(_, key)| *key == pressed)
            .map(|(action, _)| *action)
    }

    /// The key bound to `action`, as text for help.
    pub(crate) fn label(&self, action: Action) -> String {
        self.bindings
            .get(&action.name())
            .map_or_else(|| "unbound".into(), |(_, key)| describe(*key))
    }
}

/// A key as people write it, such as `Alt+.`.
pub(crate) fn describe(key: Key) -> String {
    let mut text = String::new();
    for (modifier, name) in [
        (KeyModifiers::CONTROL, "Ctrl+"),
        (KeyModifiers::ALT, "Alt+"),
        (KeyModifiers::SHIFT, "Shift+"),
    ] {
        if key.modifiers.contains(modifier) {
            text.push_str(name);
        }
    }
    match key.code {
        KeyCode::Char(' ') => text.push_str("Space"),
        KeyCode::Char(character) => text.push(character.to_ascii_uppercase()),
        KeyCode::F(number) => text.push_str(&format!("F{number}")),
        KeyCode::PageUp => text.push_str("PageUp"),
        KeyCode::PageDown => text.push_str("PageDown"),
        other => text.push_str(&format!("{other:?}")),
    }
    text
}

#[cfg(test)]
mod tests;
