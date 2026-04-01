use winit::event::ElementState;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

pub enum AppAction {
    ToggleSwitcher,
    ToggleHelp,
    NewTerminal,
    CloseSession,
    CycleSessionNext,
    CycleSessionPrev,
    CyclePaneNext,
    CyclePanePrev,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    CloseOverlay,
}

pub fn check_app_keybinding(
    key: &Key,
    physical: &PhysicalKey,
    mods: &ModifiersState,
    state: ElementState,
) -> Option<AppAction> {
    if state != ElementState::Pressed {
        return None;
    }
    let ctrl = mods.control_key();
    let shift = mods.shift_key();
    let super_key = mods.super_key();
    let cmd = ctrl || super_key;

    if cmd && !shift && matches_physical(physical, KeyCode::KeyK) {
        return Some(AppAction::ToggleSwitcher);
    }
    if ctrl && shift && key_char(key) == Some('?') {
        return Some(AppAction::ToggleHelp);
    }
    if cmd && shift && matches_named(key, NamedKey::Enter) {
        return Some(AppAction::NewTerminal);
    }
    if cmd && shift && matches_physical(physical, KeyCode::KeyW) {
        return Some(AppAction::CloseSession);
    }
    if cmd && shift && key_char(key) == Some('}') {
        return Some(AppAction::CycleSessionNext);
    }
    if cmd && shift && key_char(key) == Some('{') {
        return Some(AppAction::CycleSessionPrev);
    }
    if ctrl && !shift && matches_physical(physical, KeyCode::BracketRight) {
        return Some(AppAction::CyclePaneNext);
    }
    if ctrl && !shift && matches_physical(physical, KeyCode::BracketLeft) {
        return Some(AppAction::CyclePanePrev);
    }
    if shift && !ctrl && !super_key && matches_named(key, NamedKey::PageUp) {
        return Some(AppAction::ScrollPageUp);
    }
    if shift && !ctrl && !super_key && matches_named(key, NamedKey::PageDown) {
        return Some(AppAction::ScrollPageDown);
    }
    if shift && !ctrl && !super_key && matches_named(key, NamedKey::Home) {
        return Some(AppAction::ScrollToTop);
    }
    if shift && !ctrl && !super_key && matches_named(key, NamedKey::End) {
        return Some(AppAction::ScrollToBottom);
    }
    if matches_named(key, NamedKey::Escape) && !ctrl && !shift && !super_key {
        return Some(AppAction::CloseOverlay);
    }
    None
}

pub fn key_to_bytes(
    key: &Key,
    physical: &PhysicalKey,
    mods: &ModifiersState,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();
    let super_key = mods.super_key();

    if ctrl && !alt && !super_key {
        if let Some(ch) = physical_key_letter(physical) {
            return Some(vec![ch - b'A' + 1]);
        }
        if let PhysicalKey::Code(code) = physical {
            match code {
                KeyCode::BracketLeft => return Some(vec![0x1b]),
                KeyCode::Backslash => return Some(vec![0x1c]),
                KeyCode::BracketRight => return Some(vec![0x1d]),
                _ => {}
            }
        }
        if ctrl && shift {
            if key_char(key) == Some('?') {
                return Some(vec![0x7f]);
            }
            if key_char(key) == Some(' ') || key_char(key) == Some('@') {
                return Some(vec![0x00]);
            }
        }
    }

    let modbits = (if shift { 1u8 } else { 0 })
        + (if alt { 2 } else { 0 })
        + (if ctrl { 4 } else { 0 })
        + (if super_key { 8 } else { 0 });

    if let Some(arrow) = match key {
        Key::Named(NamedKey::ArrowUp) => Some(b'A'),
        Key::Named(NamedKey::ArrowDown) => Some(b'B'),
        Key::Named(NamedKey::ArrowRight) => Some(b'C'),
        Key::Named(NamedKey::ArrowLeft) => Some(b'D'),
        _ => None,
    } {
        if modbits != 0 {
            return Some(format!("\x1b[1;{}{}", modbits + 1, arrow as char).into_bytes());
        }
        let prefix = if app_cursor { "\x1bO" } else { "\x1b[" };
        return Some(format!("{prefix}{}", arrow as char).into_bytes());
    }

    let tilde: Option<&str> = match key {
        Key::Named(NamedKey::PageUp) => Some("5"),
        Key::Named(NamedKey::PageDown) => Some("6"),
        Key::Named(NamedKey::Delete) => Some("3"),
        Key::Named(NamedKey::Insert) => Some("2"),
        _ => None,
    };
    if let Some(code) = tilde {
        if modbits != 0 {
            return Some(format!("\x1b[{code};{}~", modbits + 1).into_bytes());
        }
        return Some(format!("\x1b[{code}~").into_bytes());
    }

    let he: Option<char> = match key {
        Key::Named(NamedKey::Home) => Some('H'),
        Key::Named(NamedKey::End) => Some('F'),
        _ => None,
    };
    if let Some(ch) = he {
        if modbits != 0 {
            return Some(format!("\x1b[1;{}{ch}", modbits + 1).into_bytes());
        }
        return Some(format!("\x1b[{ch}").into_bytes());
    }

    let f14: Option<char> = match key {
        Key::Named(NamedKey::F1) => Some('P'),
        Key::Named(NamedKey::F2) => Some('Q'),
        Key::Named(NamedKey::F3) => Some('R'),
        Key::Named(NamedKey::F4) => Some('S'),
        _ => None,
    };
    if let Some(ch) = f14 {
        if modbits != 0 {
            return Some(format!("\x1b[1;{}{ch}", modbits + 1).into_bytes());
        }
        return Some(format!("\x1bO{ch}").into_bytes());
    }

    let fkeys: Option<&str> = match key {
        Key::Named(NamedKey::F5) => Some("15"),
        Key::Named(NamedKey::F6) => Some("17"),
        Key::Named(NamedKey::F7) => Some("18"),
        Key::Named(NamedKey::F8) => Some("19"),
        Key::Named(NamedKey::F9) => Some("20"),
        Key::Named(NamedKey::F10) => Some("21"),
        Key::Named(NamedKey::F11) => Some("23"),
        Key::Named(NamedKey::F12) => Some("24"),
        _ => None,
    };
    if let Some(code) = fkeys {
        if modbits != 0 {
            return Some(format!("\x1b[{code};{}~", modbits + 1).into_bytes());
        }
        return Some(format!("\x1b[{code}~").into_bytes());
    }

    match key {
        Key::Named(NamedKey::Enter) => return Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => return Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) => return Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => return Some(vec![0x1b]),
        _ => {}
    }

    if alt && !ctrl && !super_key {
        if let Some(ch) = key_char(key) {
            if (0x20..=0x7e).contains(&(ch as u32)) {
                return Some(format!("\x1b{ch}").into_bytes());
            }
        }
    }

    if !ctrl && !super_key && !alt {
        if let Key::Character(s) = key {
            return Some(s.as_str().as_bytes().to_vec());
        }
    }

    None
}

fn matches_named(key: &Key, named: NamedKey) -> bool {
    matches!(key, Key::Named(n) if *n == named)
}

fn matches_physical(physical: &PhysicalKey, code: KeyCode) -> bool {
    matches!(physical, PhysicalKey::Code(pc) if *pc == code)
}

fn key_char(key: &Key) -> Option<char> {
    if let Key::Character(s) = key {
        s.chars().next()
    } else {
        None
    }
}

fn physical_key_letter(physical: &PhysicalKey) -> Option<u8> {
    if let PhysicalKey::Code(code) = physical {
        let b = match code {
            KeyCode::KeyA => b'A', KeyCode::KeyB => b'B', KeyCode::KeyC => b'C',
            KeyCode::KeyD => b'D', KeyCode::KeyE => b'E', KeyCode::KeyF => b'F',
            KeyCode::KeyG => b'G', KeyCode::KeyH => b'H', KeyCode::KeyI => b'I',
            KeyCode::KeyJ => b'J', KeyCode::KeyK => b'K', KeyCode::KeyL => b'L',
            KeyCode::KeyM => b'M', KeyCode::KeyN => b'N', KeyCode::KeyO => b'O',
            KeyCode::KeyP => b'P', KeyCode::KeyQ => b'Q', KeyCode::KeyR => b'R',
            KeyCode::KeyS => b'S', KeyCode::KeyT => b'T', KeyCode::KeyU => b'U',
            KeyCode::KeyV => b'V', KeyCode::KeyW => b'W', KeyCode::KeyX => b'X',
            KeyCode::KeyY => b'Y', KeyCode::KeyZ => b'Z',
            _ => return None,
        };
        Some(b)
    } else {
        None
    }
}
