//! 快捷键绑定（1.0.7）：与平台无关的"键名 ↔ 虚拟键码 + 修饰键"映射。
//!
//! 为什么放在 core 而不是平台层：设置界面要显示/编辑键名，钩子线程要拿到键码，
//! 两边都需要同一套解析规则；而且解析逻辑是纯函数，可以跨平台单测（本 crate 不依赖
//! `windows`，见 `docs/adr/0003-deps-and-loading-trim.md`）。
//!
//! 键名的写法：`Space`、`PageDown`、`F5`、`Ctrl+Space`、`Ctrl+Shift+S`。
//! 分隔符只有 `+`；键名本身带加号的用别名 `Plus`（`Ctrl+Plus`）。
//!
//! 虚拟键码表是 Windows 的 `VK_*` 数值，这里只存数字，不引用任何 Win32 头，
//! 因此仍然满足"core 不依赖 windows"。

use serde::{Deserialize, Serialize};

/// 可绑定的动作。
///
/// 索引（[`Action::index`]）必须与 [`Action::ALL`] 的下标一致 —— 平台层用这个下标
/// 去索引它的 `AtomicU32` 绑定表，错位就会串键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 打开 / 关闭预览（默认空格）。
    Toggle,
    /// 关闭预览（默认 Esc）。
    Close,
    /// 下一页（默认 PageDown）。
    PageNext,
    /// 上一页（默认 PageUp）。
    PagePrev,
    /// 向下滚动一行（默认 下方向键）。
    ScrollDown,
    /// 向上滚动一行（默认 上方向键）。
    ScrollUp,
    /// 放大一档（默认 =）。
    ZoomIn,
    /// 缩小一档（默认 -）。
    ZoomOut,
}

impl Action {
    /// 全部动作，顺序即索引。平台层的绑定表按这个顺序排。
    pub const ALL: [Action; 8] = [
        Action::Toggle,
        Action::Close,
        Action::PageNext,
        Action::PagePrev,
        Action::ScrollDown,
        Action::ScrollUp,
        Action::ZoomIn,
        Action::ZoomOut,
    ];
    /// 动作种类数量。
    pub const COUNT: usize = 8;

    pub fn index(self) -> usize {
        match self {
            Action::Toggle => 0,
            Action::Close => 1,
            Action::PageNext => 2,
            Action::PagePrev => 3,
            Action::ScrollDown => 4,
            Action::ScrollUp => 5,
            Action::ZoomIn => 6,
            Action::ZoomOut => 7,
        }
    }

    /// 配置文件里的字段名（改这个等于改配置 schema，慎动）。
    pub fn key(self) -> &'static str {
        match self {
            Action::Toggle => "toggle",
            Action::Close => "close",
            Action::PageNext => "page_next",
            Action::PagePrev => "page_prev",
            Action::ScrollDown => "scroll_down",
            Action::ScrollUp => "scroll_up",
            Action::ZoomIn => "zoom_in",
            Action::ZoomOut => "zoom_out",
        }
    }

    /// 设置界面里显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            Action::Toggle => "打开 / 关闭预览",
            Action::Close => "关闭预览",
            Action::PageNext => "下一页",
            Action::PagePrev => "上一页",
            Action::ScrollDown => "向下滚动一行",
            Action::ScrollUp => "向上滚动一行",
            Action::ZoomIn => "放大一档",
            Action::ZoomOut => "缩小一档",
        }
    }

    /// 匹配时是否忽略 Shift（1.8.3）。
    ///
    /// 只有放大/缩小：`=`/`+` 在物理上是同一个键（`VK_OEM_PLUS`），`-`/`_` 同理。
    /// 严格比对之后，用户按 `Shift+=`（打出 `+`）会不再命中——那是回归，所以放宽
    /// 这一维。其它动作保持严格：`Ctrl+Space`、`Alt+Space` 绝不能被吞。
    pub fn ignores_shift(self) -> bool {
        matches!(self, Action::ZoomIn | Action::ZoomOut)
    }

    pub fn category(self) -> Category {
        match self {
            Action::Toggle | Action::Close => Category::Global,
            _ => Category::Browse,
        }
    }

    pub fn default_binding(self) -> &'static str {
        match self {
            Action::Toggle => "Space",
            Action::Close => "Esc",
            Action::PageNext => "PageDown",
            Action::PagePrev => "PageUp",
            Action::ScrollDown => "Down",
            Action::ScrollUp => "Up",
            Action::ZoomIn => "=",
            Action::ZoomOut => "-",
        }
    }

    /// 上报给 UI 的**规范键码**。
    ///
    /// 用户可以把"下一页"改绑到 F5，但钩子仍然按这个规范键码投递消息，
    /// 这样 `app` 里的按键分派（`page_delta_for` 等）不需要跟着配置变形。
    pub fn canonical_vk(self) -> u16 {
        match self {
            Action::Toggle => 0x20,     // VK_SPACE
            Action::Close => 0x1B,      // VK_ESCAPE
            Action::PageNext => 0x22,   // VK_NEXT
            Action::PagePrev => 0x21,   // VK_PRIOR
            Action::ScrollDown => 0x28, // VK_DOWN
            Action::ScrollUp => 0x26,   // VK_UP
            Action::ZoomIn => 0xBB,     // VK_OEM_PLUS
            Action::ZoomOut => 0xBD,    // VK_OEM_MINUS
        }
    }
}

/// 设置界面的分组。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// 任何时候都生效（打开/关闭）。
    Global,
    /// 只在预览窗打开时生效（翻页、滚动、缩放）。
    Browse,
}

impl Category {
    pub const ALL: [Category; 2] = [Category::Global, Category::Browse];

    pub fn label(self) -> &'static str {
        match self {
            Category::Global => "全局",
            Category::Browse => "浏览（预览打开时）",
        }
    }

    /// 该分组下的动作（按 [`Action::ALL`] 的顺序）。
    pub fn actions(self) -> Vec<Action> {
        Action::ALL
            .iter()
            .copied()
            .filter(|a| a.category() == self)
            .collect()
    }
}

/// 修饰键掩码：打包进 u32 的高位，低 16 位是虚拟键码。
pub const MOD_CTRL: u32 = 1 << 16;
pub const MOD_ALT: u32 = 1 << 17;
pub const MOD_SHIFT: u32 = 1 << 18;

/// 一条绑定：虚拟键码 + 三个修饰键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub vk: u16,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Binding {
    /// 只按键、不带修饰。
    pub fn plain(vk: u16) -> Self {
        Self {
            vk,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// 是否有修饰键要求（钩子回调据此决定要不要多查一次键盘状态）。
    pub fn has_modifier(self) -> bool {
        self.ctrl || self.alt || self.shift
    }

    /// 打包成一个 u32 放进平台层的原子表（0 = 未绑定，所以 vk 不能是 0）。
    pub fn pack(self) -> u32 {
        let mut packed = self.vk as u32;
        if self.ctrl {
            packed |= MOD_CTRL;
        }
        if self.alt {
            packed |= MOD_ALT;
        }
        if self.shift {
            packed |= MOD_SHIFT;
        }
        packed
    }

    /// 解包；0（未绑定）和键码 0 都返回 `None`。
    pub fn unpack(packed: u32) -> Option<Self> {
        let vk = (packed & 0xFFFF) as u16;
        if vk == 0 {
            return None;
        }
        Some(Self {
            vk,
            ctrl: packed & MOD_CTRL != 0,
            alt: packed & MOD_ALT != 0,
            shift: packed & MOD_SHIFT != 0,
        })
    }

    /// 显示名（例如 `Ctrl+Space`）。
    pub fn text(self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        out.push_str(vk_name(self.vk).unwrap_or("?"));
        out
    }

    /// 本次按键（键码 + 修饰键状态）是否正好命中这条绑定。
    ///
    /// **修饰键要求完全相等**，不是"包含"：绑定 `Space` 时按 `Ctrl+Space` 不算命中。
    /// 理由：`Ctrl+Space` 是中文输入法切换、`Alt+Space` 是系统菜单，把空格绑成
    /// 预览已经够激进，再"包含式匹配"会连这两个组合一起抢走。
    pub fn matches(self, vk: u16, ctrl: bool, alt: bool, shift: bool) -> bool {
        self.vk == vk && self.ctrl == ctrl && self.alt == alt && self.shift == shift
    }

    /// 同 [`Binding::matches`]，但**不强求 Shift 的状态**。
    ///
    /// 只给"物理上是同一个键"的动作用（放大/缩小：`=` 与 `+` 都是 `VK_OEM_PLUS`）。
    /// 1.8.2 之前这类动作能工作，是因为匹配时"没声明的修饰键一律当 false"——
    /// 那是个 bug（`Ctrl+Space` 会被空格热键吞掉），修好它就会顺带打死 `Shift+=`，
    /// 所以这里显式放宽 Shift 这一维。
    pub fn matches_ignoring_shift(self, vk: u16, ctrl: bool, alt: bool, _shift: bool) -> bool {
        self.vk == vk && self.ctrl == ctrl && self.alt == alt
    }
}

/// 键名 → 虚拟键码（大小写不敏感）。单字母/单数字单独处理。
pub fn vk_from_name(name: &str) -> Option<u16> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Some(&(_, vk)) = VK_NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(vk);
    }
    // 单字符：A-Z、0-9
    let mut chars = name.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let upper = ch.to_ascii_uppercase();
    if upper.is_ascii_uppercase() {
        return Some(upper as u16); // VK_A == 'A'
    }
    if ch.is_ascii_digit() {
        return Some(ch as u16); // VK_0 == '0'
    }
    None
}

/// 虚拟键码 → 键名（[`Binding::text`] 与设置界面用）。认不出来就 `None`。
pub fn vk_name(vk: u16) -> Option<&'static str> {
    if let Some(&(name, _)) = VK_NAMES.iter().find(|(_, code)| *code == vk) {
        return Some(name);
    }
    // 单字母/单数字：静态表里放不下，交给调用方显示成字符。
    if (0x41..=0x5A).contains(&vk) {
        return Some(LETTERS[(vk - 0x41) as usize]);
    }
    if (0x30..=0x39).contains(&vk) {
        return Some(DIGITS[(vk - 0x30) as usize]);
    }
    None
}

const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];
const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

/// 键名表。第一个匹配到的名字用于反查显示，所以**常用名放前面**
/// （`Escape` 在 `Esc` 之后，显示时统一用 `Esc`）。
const VK_NAMES: &[(&str, u16)] = &[
    ("Space", 0x20),
    ("Esc", 0x1B),
    ("Escape", 0x1B),
    ("Enter", 0x0D),
    ("Tab", 0x09),
    ("Backspace", 0x08),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("F1", 0x70),
    ("F2", 0x71),
    ("F3", 0x72),
    ("F4", 0x73),
    ("F5", 0x74),
    ("F6", 0x75),
    ("F7", 0x76),
    ("F8", 0x77),
    ("F9", 0x78),
    ("F10", 0x79),
    ("F11", 0x7A),
    ("F12", 0x7B),
    ("Shift", 0x10),
    ("Ctrl", 0x11),
    ("Alt", 0x12),
    ("+", 0xBB),
    ("=", 0xBB),
    ("Plus", 0xBB),
    ("-", 0xBD),
    ("Minus", 0xBD),
    ("[", 0xDB),
    ("]", 0xDD),
    (";", 0xBA),
    ("'", 0xDE),
    (",", 0xBC),
    (".", 0xBE),
    ("/", 0xBF),
    ("\\", 0xDC),
    ("`", 0xC0),
    ("NumPlus", 0x6B),
    ("NumMinus", 0x6D),
    ("NumMultiply", 0x6A),
    ("NumDivide", 0x6F),
];

/// 解析键名。失败返回 `None`（调用方应退回默认值，而不是让程序崩掉）。
pub fn parse(text: &str) -> Option<Binding> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let parts: Vec<&str> = text.split('+').collect();
    // `Ctrl++` 这种写法不支持（键名带加号时请用 `Plus`）；
    // 但**单个** `+` 是合法绑定（放大键），所以先单独认一下。
    if text == "+" {
        return Some(Binding::plain(0xBB));
    }
    if parts.iter().any(|p| p.trim().is_empty()) {
        return None;
    }
    let (key, modifiers) = parts.split_last()?;
    let mut binding = Binding::plain(vk_from_name(key)?);
    for modifier in modifiers {
        match modifier.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => binding.ctrl = true,
            "alt" => binding.alt = true,
            "shift" => binding.shift = true,
            _ => return None,
        }
    }
    Some(binding)
}

/// 一组可配置的快捷键（对应 `config.json` 的 `hotkeys` 段）。
///
/// 全是字符串：用户可能手改配置文件，非法值在 [`Hotkeys::binding`] 里退回默认，
/// 不让一个错字把整个程序变成"按键全失效"。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub toggle: String,
    pub close: String,
    pub page_next: String,
    pub page_prev: String,
    pub scroll_down: String,
    pub scroll_up: String,
    pub zoom_in: String,
    pub zoom_out: String,
    /// 放大镜的**按住键**（不是点按绑定：按住期间显示镜头）。
    pub lens_hold: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            toggle: Action::Toggle.default_binding().to_string(),
            close: Action::Close.default_binding().to_string(),
            page_next: Action::PageNext.default_binding().to_string(),
            page_prev: Action::PagePrev.default_binding().to_string(),
            scroll_down: Action::ScrollDown.default_binding().to_string(),
            scroll_up: Action::ScrollUp.default_binding().to_string(),
            zoom_in: Action::ZoomIn.default_binding().to_string(),
            zoom_out: Action::ZoomOut.default_binding().to_string(),
            lens_hold: "Shift".to_string(),
        }
    }
}

impl Hotkeys {
    /// 某个动作当前的文本绑定。
    pub fn get(&self, action: Action) -> &str {
        match action {
            Action::Toggle => &self.toggle,
            Action::Close => &self.close,
            Action::PageNext => &self.page_next,
            Action::PagePrev => &self.page_prev,
            Action::ScrollDown => &self.scroll_down,
            Action::ScrollUp => &self.scroll_up,
            Action::ZoomIn => &self.zoom_in,
            Action::ZoomOut => &self.zoom_out,
        }
    }

    pub fn set(&mut self, action: Action, value: String) {
        match action {
            Action::Toggle => self.toggle = value,
            Action::Close => self.close = value,
            Action::PageNext => self.page_next = value,
            Action::PagePrev => self.page_prev = value,
            Action::ScrollDown => self.scroll_down = value,
            Action::ScrollUp => self.scroll_up = value,
            Action::ZoomIn => self.zoom_in = value,
            Action::ZoomOut => self.zoom_out = value,
        }
    }

    /// 解析某个动作的绑定；解析不出来就用默认值（**永不失败**）。
    pub fn binding(&self, action: Action) -> Binding {
        parse(self.get(action))
            .or_else(|| parse(action.default_binding()))
            .unwrap_or_else(|| Binding::plain(action.canonical_vk()))
    }

    /// 放大镜按住键；解析不出来退回 Shift（`VK_SHIFT`）。
    pub fn lens_binding(&self) -> Binding {
        parse(&self.lens_hold).unwrap_or_else(|| Binding::plain(0x10))
    }

    /// 打包成平台层用的定长数组（下标 = [`Action::index`]）。
    pub fn packed(&self) -> [u32; Action::COUNT] {
        let mut out = [0u32; Action::COUNT];
        for action in Action::ALL {
            out[action.index()] = self.binding(action).pack();
        }
        out
    }

    /// 把非法/空白的项恢复成默认值（读配置后调用）。
    pub fn sanitized(mut self) -> Self {
        for action in Action::ALL {
            if parse(self.get(action)).is_none() {
                self.set(action, action.default_binding().to_string());
            }
        }
        if parse(&self.lens_hold).is_none() {
            self.lens_hold = "Shift".to_string();
        }
        self
    }

    /// 给日志看的摘要（例：`toggle=Space close=Esc page=PageDown/PageUp`）。
    pub fn summary(&self) -> String {
        format!(
            "toggle={} close={} page={}/{} scroll={}/{} zoom={}/{} lens={}",
            self.toggle,
            self.close,
            self.page_next,
            self.page_prev,
            self.scroll_down,
            self.scroll_up,
            self.zoom_in,
            self.zoom_out,
            self.lens_hold
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_indices_are_dense_and_unique() {
        let mut seen = [false; Action::COUNT];
        for (position, action) in Action::ALL.iter().enumerate() {
            assert_eq!(action.index(), position, "{action:?} 索引与 ALL 顺序不一致");
            assert!(!seen[action.index()]);
            seen[action.index()] = true;
        }
        assert!(seen.iter().all(|v| *v));
    }

    #[test]
    fn action_keys_and_labels_are_unique() {
        let mut keys = std::collections::HashSet::new();
        for action in Action::ALL {
            assert!(
                keys.insert(action.key()),
                "配置字段名重复：{}",
                action.key()
            );
            assert!(!action.label().is_empty());
        }
    }

    #[test]
    fn parses_plain_keys() {
        assert_eq!(parse("Space"), Some(Binding::plain(0x20)));
        assert_eq!(parse("space"), Some(Binding::plain(0x20)));
        assert_eq!(parse(" Esc "), Some(Binding::plain(0x1B)));
        assert_eq!(parse("PageDown"), Some(Binding::plain(0x22)));
        assert_eq!(parse("F5"), Some(Binding::plain(0x74)));
        assert_eq!(parse("A"), Some(Binding::plain(0x41)));
        assert_eq!(parse("7"), Some(Binding::plain(0x37)));
    }

    #[test]
    fn parses_modifiers() {
        let b = parse("Ctrl+Space").unwrap();
        assert_eq!(b.vk, 0x20);
        assert!(b.ctrl && !b.alt && !b.shift);
        let b = parse("ctrl + shift + s").unwrap();
        assert_eq!(b.vk, 0x53);
        assert!(b.ctrl && b.shift && !b.alt);
    }

    #[test]
    fn rejects_junk() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse("Ctrl+"), None);
        assert_eq!(parse("Ctrl+Meta+X"), None);
        assert_eq!(parse("NoSuchKey"), None);
        assert_eq!(parse("Ctrl++"), None);
    }

    #[test]
    fn pack_round_trips() {
        let b = Binding {
            vk: 0x22,
            ctrl: true,
            alt: false,
            shift: true,
        };
        let packed = b.pack();
        assert_eq!(Binding::unpack(packed), Some(b));
        assert_eq!(Binding::unpack(0), None);
        // 无修饰时打包结果就是键码本身，方便排查。
        assert_eq!(Binding::plain(0x20).pack(), 0x20);
    }

    #[test]
    fn text_round_trips_through_parse() {
        // `=` 与 `+` 是同一个物理键（VK_OEM_PLUS）：都接受，显示统一成 `+`（与默认值一致）。
        for (text, shown) in [
            ("Space", "Space"),
            ("Ctrl+Shift+S", "Ctrl+Shift+S"),
            ("Alt+F4", "Alt+F4"),
            ("PageDown", "PageDown"),
            ("=", "+"),
            ("+", "+"),
            ("-", "-"),
        ] {
            let binding = parse(text).unwrap_or_else(|| panic!("解析失败：{text}"));
            assert_eq!(binding.text(), shown);
        }
    }

    #[test]
    fn defaults_round_trip_and_cover_every_action() {
        let hotkeys = Hotkeys::default();
        for action in Action::ALL {
            let binding = hotkeys.binding(action);
            assert_eq!(
                binding,
                parse(action.default_binding()).unwrap(),
                "{} 的默认绑定解析不一致",
                action.key()
            );
        }
        assert_eq!(hotkeys.lens_binding().vk, 0x10);
    }

    #[test]
    fn broken_config_falls_back_to_defaults() {
        let hotkeys = Hotkeys {
            toggle: "不是键名".to_string(),
            close: String::new(),
            lens_hold: "??".to_string(),
            ..Hotkeys::default()
        }
        .sanitized();
        assert_eq!(hotkeys.toggle, "Space");
        assert_eq!(hotkeys.close, "Esc");
        assert_eq!(hotkeys.lens_hold, "Shift");
        // 未损坏的项保持不变。
        assert_eq!(hotkeys.page_next, "PageDown");
    }

    #[test]
    fn packed_matches_binding_per_index() {
        let hotkeys = Hotkeys {
            page_next: "F5".to_string(),
            ..Hotkeys::default()
        };
        let packed = hotkeys.packed();
        assert_eq!(packed[Action::PageNext.index()], 0x74);
        assert_eq!(
            Binding::unpack(packed[Action::Toggle.index()]),
            Some(Binding::plain(0x20))
        );
    }

    #[test]
    fn categories_partition_all_actions() {
        let mut total = 0;
        for category in Category::ALL {
            let actions = category.actions();
            assert!(!actions.is_empty(), "{category:?} 分组是空的");
            for action in &actions {
                assert_eq!(action.category(), category);
            }
            total += actions.len();
        }
        assert_eq!(total, Action::COUNT, "分组漏了动作");
    }

    /// 1.8.3 T04：修饰键必须**精确**匹配。
    ///
    /// 曾经的 bug 不在 `matches` 里（它一直是严格相等），而在调用方：没声明的修饰键
    /// 被直接当成 false，于是 `Ctrl+Space`（输入法切换）被空格热键吞掉。
    #[test]
    fn modifiers_must_match_exactly() {
        let space = parse("Space").unwrap();
        assert!(space.matches(0x20, false, false, false), "裸空格要命中");
        assert!(
            !space.matches(0x20, true, false, false),
            "Ctrl+Space 不该命中"
        );
        assert!(
            !space.matches(0x20, false, true, false),
            "Alt+Space 不该命中"
        );
        assert!(
            !space.matches(0x20, false, false, true),
            "Shift+Space 不该命中"
        );
    }

    /// 放大/缩小是"同一物理键的两个字符"（`=`/`+`），匹配时忽略 Shift。
    #[test]
    fn zoom_actions_ignore_shift() {
        assert!(Action::ZoomIn.ignores_shift() && Action::ZoomOut.ignores_shift());
        assert!(!Action::Toggle.ignores_shift(), "空格必须严格");
        assert!(!Action::PageNext.ignores_shift(), "翻页必须严格");

        let eq = parse("=").unwrap();
        // 按 `=`（没有 Shift）和按 `Shift+=`（打出 `+`）都要放大。
        assert!(eq.matches_ignoring_shift(0xBB, false, false, false));
        assert!(eq.matches_ignoring_shift(0xBB, false, false, true));
        // 但 Ctrl+= / Alt+= 不算：那一维仍然严格。
        assert!(!eq.matches_ignoring_shift(0xBB, true, false, false));
        assert!(!eq.matches_ignoring_shift(0xBB, false, true, true));
    }
}
