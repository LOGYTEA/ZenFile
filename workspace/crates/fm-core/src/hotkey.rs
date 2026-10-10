//! 键位解析（技术方案 §8）。
//!
//! 两个硬性要求：
//!   - 字母必须用 VK_* 虚拟键码，绝不能用 ASCII（CapsLock / 不同键盘布局会失效）
//!   - 注册时必须带 MOD_NOREPEAT，退出必须 UnregisterHotKey（否则残留导致下次启动 1409）

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    VK_A, VK_D, VK_F, VK_L, VK_N, VK_T, VK_V, VK_W,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

const VK_MAP: [(char, u32); 8] = [
    ('a', VK_A.0 as u32),
    ('d', VK_D.0 as u32),
    ('f', VK_F.0 as u32),
    ('l', VK_L.0 as u32),
    ('n', VK_N.0 as u32),
    ('t', VK_T.0 as u32),
    ('v', VK_V.0 as u32),
    ('w', VK_W.0 as u32),
];

const VIRTUAL_KEYS: [(&str, u32); 11] = [
    ("space", 0x20),
    ("enter", 0x0D),
    ("esc", 0x1B),
    ("delete", 0x2E),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pagedown", 0x22),
    ("left", 0x25),
    ("up", 0x26),
    ("right", 0x27),
];

/// 解析 "ctrl+alt+f" / "ctrl+shift+n" / "delete" 等书写形式。
pub fn parse(input: &str) -> Option<Hotkey> {
    let parts: Vec<String> = input.split('+').map(|s| s.trim().to_lowercase()).collect();
    let mut modifiers = HOT_KEY_MODIFIERS(0);
    let mut vk: Option<u32> = None;
    for p in &parts {
        match p.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" => modifiers |= windows::Win32::UI::Input::KeyboardAndMouse::MOD_WIN,
            other => {
                if let Some((_, code)) = VIRTUAL_KEYS.iter().find(|(k, _)| *k == other) {
                    vk = Some(*code);
                } else if other.chars().count() == 1 {
                    let c = other.chars().next().unwrap();
                    vk = VK_MAP.iter().find(|(k, _)| *k == c).map(|(_, v)| *v);
                    if vk.is_none() {
                        vk = Some((c.to_ascii_uppercase()) as u32);
                    }
                }
            }
        }
    }
    vk.map(|vk| Hotkey { modifiers: modifiers | MOD_NOREPEAT, vk })
}

impl Hotkey {
    /// 是否包含 Win 键 —— Win 组合可能被系统静默吞掉，需在设置页做冲突自检提示（§8.1）。
    pub fn involves_win(&self) -> bool {
        self.modifiers & windows::Win32::UI::Input::KeyboardAndMouse::MOD_WIN
            == windows::Win32::UI::Input::KeyboardAndMouse::MOD_WIN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let hk = parse("ctrl+alt+f").unwrap();
        assert_eq!(hk.vk, VK_F.0 as u32);
        assert!(hk.modifiers & MOD_CONTROL == MOD_CONTROL);
        assert!(hk.modifiers & MOD_ALT == MOD_ALT);
        assert!(hk.modifiers & MOD_NOREPEAT == MOD_NOREPEAT);
    }

    #[test]
    fn parse_virtual_keys() {
        assert_eq!(parse("delete").unwrap().vk, 0x2E);
        assert_eq!(parse("ctrl+shift+n").unwrap().vk, VK_N.0 as u32);
    }
}
