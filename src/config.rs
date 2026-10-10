//! Configuration schema, defaults, validation and comment-preserving edits.
//!
//! Parsing and editing are pure (text in, text out); file access lives in `load_or_create` and `write_atomic`.

use std::path::Path;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::keys::Shortcut;

pub const DEFAULT_CONFIG: &str = r#"# CapsLangChange configuration.
# Edit and save: changes are applied automatically. Reference: README.md in the project repository.

enabled = true

# How switch_layout changes the layout by default:
#   "win_space" - presses Win+Space (Win+Shift+Space for "previous"), like Windows itself
#   "message"   - asks the active window directly through the Windows API, no synthetic keys
method = "win_space"

# Return Caps Lock to its previous state when leaving an excluded program.
restore_capslock_on_leave = true

# When the active program cannot be identified: "act" = treat it as a normal program, "pass" = do nothing.
on_lookup_failure = "act"

[[binding]]
name = "Switch layout"
keys = "CapsLock"
action = "switch_layout"

# More examples (remove the leading '#' to use them):
#
# [[binding]]
# name = "Real Caps Lock"
# keys = "Ctrl+Alt+Shift+Space"
# action = "capslock"            # state = "toggle" (default), "on" or "off"
#
# [[binding]]
# name = "English"
# keys = "Win+Ctrl+Shift+1"
# action = "switch_layout"
# mode = "set"                   # "next" (default), "previous" or "set"
# layout = "00000409"            # see tray menu > Installed layouts
#
# [[binding]]
# name = "Only English and Russian"
# keys = "RAlt+Space"
# action = "switch_layout"
# cycle = ["00000409", "00000419"]
#
# [[binding]]
# keys = "Ctrl+Q"
# action = "block"
# apps_only = ["notepad.exe"]

# Programs where all bindings are suspended (the keys reach the program unchanged).
# Add them from the tray menu, or by hand:
#
# [[exclude]]
# exe = "SomeGame.exe"
"#;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    WinSpace,
    Message,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::WinSpace => "win_space",
            Method::Message => "message",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookupFailure {
    Act,
    Pass,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutMode {
    Next,
    Previous,
    /// Keyboard layout identifier (KLID), e.g. 0x00000419.
    Set(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapsState {
    Toggle,
    On,
    Off,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    SwitchLayout { mode: LayoutMode, cycle: Vec<u32>, method: Method },
    CapsLock(CapsState),
    SendKeys(Shortcut),
    Block,
    Passthrough,
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::SwitchLayout { mode: LayoutMode::Next, .. } => "switch layout".into(),
            Action::SwitchLayout { mode: LayoutMode::Previous, .. } => "previous layout".into(),
            Action::SwitchLayout { mode: LayoutMode::Set(klid), .. } => format!("layout {klid:08X}"),
            Action::CapsLock(CapsState::Toggle) => "Caps Lock".into(),
            Action::CapsLock(CapsState::On) => "Caps Lock on".into(),
            Action::CapsLock(CapsState::Off) => "Caps Lock off".into(),
            Action::SendKeys(s) => format!("send {s}"),
            Action::Block => "block".into(),
            Action::Passthrough => "pass through".into(),
        }
    }
}

/// Information about a program window, as matched by exclusions and scopes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppInfo {
    /// Lower-case executable file name, e.g. `notepad.exe`.
    pub exe: String,
    /// Lower-case full image path.
    pub path: String,
    /// Window class name (case-sensitive as reported by Windows).
    pub class: String,
}

impl AppInfo {
    pub fn from_path(path: &str, class: &str) -> AppInfo {
        let path = path.to_lowercase();
        let exe = path.rsplit(['\\', '/']).next().unwrap_or("").to_string();
        AppInfo { exe, path, class: class.to_string() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppMatcher {
    pub exe: Option<String>,
    pub path_prefix: Option<String>,
    pub class: Option<String>,
}

impl AppMatcher {
    pub fn exe(name: &str) -> AppMatcher {
        AppMatcher { exe: Some(name.to_lowercase()), path_prefix: None, class: None }
    }

    pub fn matches(&self, app: &AppInfo) -> bool {
        self.exe.as_ref().is_none_or(|e| *e == app.exe)
            && self.path_prefix.as_ref().is_none_or(|p| app.path.starts_with(p.as_str()))
            && self.class.as_ref().is_none_or(|c| *c == app.class)
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(e) = &self.exe {
            parts.push(e.clone());
        }
        if let Some(p) = &self.path_prefix {
            parts.push(format!("path {p}"));
        }
        if let Some(c) = &self.class {
            parts.push(format!("class {c}"));
        }
        parts.join(", ")
    }
}

#[derive(Clone, Debug)]
pub struct Binding {
    /// Position in the `[[binding]]` array of the file.
    pub index: usize,
    pub name: String,
    pub shortcut: Shortcut,
    pub action: Action,
    pub apps_only: Vec<AppMatcher>,
    pub apps_except: Vec<AppMatcher>,
    pub enabled: bool,
    pub repeat: bool,
}

impl Binding {
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            format!("{} -> {}", self.shortcut, self.action.describe())
        } else {
            format!("{} -> {} ({})", self.shortcut, self.action.describe(), self.name)
        }
    }

    /// Whether the binding applies to the given foreground program (`None` = unknown program).
    pub fn scope_allows(&self, app: Option<&AppInfo>) -> bool {
        match app {
            Some(app) => {
                (self.apps_only.is_empty() || self.apps_only.iter().any(|m| m.matches(app)))
                    && !self.apps_except.iter().any(|m| m.matches(app))
            }
            None => self.apps_only.is_empty(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub enabled: bool,
    pub method: Method,
    pub restore_capslock_on_leave: bool,
    pub on_lookup_failure: LookupFailure,
    pub bindings: Vec<Binding>,
    pub excludes: Vec<AppMatcher>,
    pub warnings: Vec<String>,
    /// Enabled binding indices per virtual-key code, in priority order.
    by_vk: Vec<Vec<usize>>,
}

impl Config {
    pub fn candidates(&self, vk: u8) -> &[usize] {
        &self.by_vk[vk as usize]
    }

    pub fn is_excluded(&self, app: &AppInfo) -> bool {
        self.excludes.iter().any(|m| m.matches(app))
    }

    fn compile(&mut self) {
        let mut by_vk = vec![Vec::new(); 256];
        let mut order: Vec<usize> = (0..self.bindings.len()).filter(|&i| self.bindings[i].enabled).collect();
        // Priority: program-specific bindings first, then side-specific modifiers, then file order.
        order.sort_by_key(|&i| {
            let b = &self.bindings[i];
            (b.apps_only.is_empty(), u8::MAX - b.shortcut.mods.specificity(), i)
        });
        for i in order {
            by_vk[self.bindings[i].shortcut.vk as usize].push(i);
        }
        self.by_vk = by_vk;
    }
}

/// Parses and validates configuration text.
pub fn parse(text: &str) -> Result<Config, Vec<String>> {
    let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| vec![e.to_string().trim().to_string()])?;
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    const TOP_KEYS: [&str; 6] =
        ["enabled", "method", "restore_capslock_on_leave", "on_lookup_failure", "binding", "exclude"];
    for (key, _) in doc.iter() {
        if !TOP_KEYS.contains(&key) {
            warnings.push(format!("unknown setting '{key}' is ignored"));
        }
    }

    let root = doc.as_table();
    let enabled = get_bool(root, "enabled", true, "", &mut errors);
    let restore = get_bool(root, "restore_capslock_on_leave", true, "", &mut errors);
    let method = match get_str(root, "method", "", &mut errors).as_deref() {
        None | Some("win_space") => Method::WinSpace,
        Some("message") => Method::Message,
        Some(other) => {
            errors.push(format!("method: unknown value '{other}' (use \"win_space\" or \"message\")"));
            Method::WinSpace
        }
    };
    let on_lookup_failure = match get_str(root, "on_lookup_failure", "", &mut errors).as_deref() {
        None | Some("act") => LookupFailure::Act,
        Some("pass") => LookupFailure::Pass,
        Some(other) => {
            errors.push(format!("on_lookup_failure: unknown value '{other}' (use \"act\" or \"pass\")"));
            LookupFailure::Act
        }
    };

    let mut bindings = Vec::new();
    for (index, table) in tables(root, "binding", &mut errors).into_iter().enumerate() {
        let ctx = binding_context(index, table);
        if let Some(b) = parse_binding(index, table, method, &ctx, &mut errors, &mut warnings) {
            bindings.push(b);
        }
    }

    let mut excludes = Vec::new();
    for (index, table) in tables(root, "exclude", &mut errors).into_iter().enumerate() {
        let ctx = format!("exclude {}", index + 1);
        check_keys(table, &["exe", "path", "class"], &ctx, &mut warnings);
        if let Some(m) = parse_matcher_table(table, &ctx, &mut errors) {
            excludes.push(m);
        }
    }

    // Duplicate triggers with identical scope: the first one wins.
    for (i, a) in bindings.iter().enumerate() {
        if !a.enabled {
            continue;
        }
        for b in bindings.iter().skip(i + 1) {
            if b.enabled && a.shortcut == b.shortcut && a.apps_only == b.apps_only && a.apps_except == b.apps_except {
                warnings.push(format!(
                    "bindings {} and {} use the same shortcut {}; binding {} wins",
                    a.index + 1,
                    b.index + 1,
                    a.shortcut,
                    a.index + 1
                ));
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    let mut config = Config {
        enabled,
        method,
        restore_capslock_on_leave: restore,
        on_lookup_failure,
        bindings,
        excludes,
        warnings,
        by_vk: Vec::new(),
    };
    config.compile();
    Ok(config)
}

fn binding_context(index: usize, table: &Table) -> String {
    match table.get("name").and_then(Item::as_str) {
        Some(name) if !name.is_empty() => format!("binding {} (\"{name}\")", index + 1),
        _ => format!("binding {}", index + 1),
    }
}

fn parse_binding(
    index: usize,
    t: &Table,
    global_method: Method,
    ctx: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Option<Binding> {
    check_keys(
        t,
        &[
            "name",
            "keys",
            "action",
            "mode",
            "layout",
            "cycle",
            "method",
            "state",
            "send",
            "apps_only",
            "apps_except",
            "enabled",
            "repeat",
        ],
        ctx,
        warnings,
    );
    let before = errors.len();
    let name = get_str(t, "name", ctx, errors).unwrap_or_default();
    let enabled = get_bool(t, "enabled", true, ctx, errors);
    let repeat = get_bool(t, "repeat", false, ctx, errors);

    let shortcut = match get_str(t, "keys", ctx, errors) {
        Some(text) => match Shortcut::parse(&text) {
            Ok(s) => Some(s),
            Err(e) => {
                errors.push(format!("{ctx}: keys: {e}"));
                None
            }
        },
        None => {
            errors.push(format!("{ctx}: 'keys' is required"));
            None
        }
    };

    let action_name = get_str(t, "action", ctx, errors);
    let action = match action_name.as_deref() {
        Some("switch_layout") => parse_switch_layout(t, global_method, ctx, errors),
        Some("capslock") => match get_str(t, "state", ctx, errors).as_deref() {
            None | Some("toggle") => Some(Action::CapsLock(CapsState::Toggle)),
            Some("on") => Some(Action::CapsLock(CapsState::On)),
            Some("off") => Some(Action::CapsLock(CapsState::Off)),
            Some(other) => {
                errors.push(format!("{ctx}: state: unknown value '{other}' (use toggle, on or off)"));
                None
            }
        },
        Some("send_keys") => match get_str(t, "send", ctx, errors) {
            Some(text) => match Shortcut::parse(&text) {
                Ok(s) => Some(Action::SendKeys(s)),
                Err(e) => {
                    errors.push(format!("{ctx}: send: {e}"));
                    None
                }
            },
            None => {
                errors.push(format!("{ctx}: action \"send_keys\" needs 'send', e.g. send = \"Ctrl+C\""));
                None
            }
        },
        Some("block") => Some(Action::Block),
        Some("passthrough") => Some(Action::Passthrough),
        Some(other) => {
            errors.push(format!(
                "{ctx}: unknown action '{other}' (use switch_layout, capslock, send_keys, block or passthrough)"
            ));
            None
        }
        None => {
            errors.push(format!("{ctx}: 'action' is required"));
            None
        }
    };

    let apps_only = parse_matcher_list(t, "apps_only", ctx, errors);
    let apps_except = parse_matcher_list(t, "apps_except", ctx, errors);

    if errors.len() != before {
        return None;
    }
    Some(Binding { index, name, shortcut: shortcut?, action: action?, apps_only, apps_except, enabled, repeat })
}

fn parse_switch_layout(t: &Table, global_method: Method, ctx: &str, errors: &mut Vec<String>) -> Option<Action> {
    let mode = match get_str(t, "mode", ctx, errors).as_deref() {
        None | Some("next") => LayoutMode::Next,
        Some("previous") => LayoutMode::Previous,
        Some("set") => match get_str(t, "layout", ctx, errors) {
            Some(text) => LayoutMode::Set(parse_klid(&text).or_else(|| {
                errors.push(format!("{ctx}: layout: '{text}' is not a layout id like \"00000409\""));
                None
            })?),
            None => {
                errors.push(format!("{ctx}: mode \"set\" needs 'layout', e.g. layout = \"00000409\""));
                return None;
            }
        },
        Some(other) => {
            errors.push(format!("{ctx}: mode: unknown value '{other}' (use next, previous or set)"));
            return None;
        }
    };
    let mut cycle = Vec::new();
    if let Some(item) = t.get("cycle") {
        match item.as_array() {
            Some(array) => {
                for v in array.iter() {
                    match v.as_str().and_then(parse_klid) {
                        Some(k) => cycle.push(k),
                        None => {
                            errors.push(format!("{ctx}: cycle: every entry must be a layout id like \"00000409\""));
                            return None;
                        }
                    }
                }
            }
            None => {
                errors.push(format!("{ctx}: cycle must be a list, e.g. [\"00000409\", \"00000419\"]"));
                return None;
            }
        }
    }
    let explicit = match get_str(t, "method", ctx, errors).as_deref() {
        None => None,
        Some("win_space") => Some(Method::WinSpace),
        Some("message") => Some(Method::Message),
        Some(other) => {
            errors.push(format!("{ctx}: method: unknown value '{other}' (use \"win_space\" or \"message\")"));
            return None;
        }
    };
    let needs_message = matches!(mode, LayoutMode::Set(_)) || !cycle.is_empty();
    let method = match (explicit, needs_message) {
        (Some(Method::WinSpace), true) => {
            errors.push(format!(
                "{ctx}: mode \"set\" and 'cycle' work only with method = \"message\" (Win+Space always cycles all layouts)"
            ));
            return None;
        }
        (_, true) => Method::Message,
        (Some(m), false) => m,
        (None, false) => global_method,
    };
    Some(Action::SwitchLayout { mode, cycle, method })
}

/// Parses an 8-digit hexadecimal keyboard layout identifier.
pub fn parse_klid(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.len() != 8 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

fn parse_matcher_list(t: &Table, key: &str, ctx: &str, errors: &mut Vec<String>) -> Vec<AppMatcher> {
    let Some(item) = t.get(key) else { return Vec::new() };
    let Some(array) = item.as_array() else {
        errors.push(format!("{ctx}: {key} must be a list, e.g. [\"game.exe\"]"));
        return Vec::new();
    };
    let mut out = Vec::new();
    for v in array.iter() {
        match v {
            Value::String(s) if !s.value().trim().is_empty() => out.push(AppMatcher::exe(s.value().trim())),
            Value::InlineTable(it) => {
                let mut table = Table::new();
                for (k, v) in it.iter() {
                    table.insert(k, Item::Value(v.clone()));
                }
                if let Some(m) = parse_matcher_table(&table, &format!("{ctx}: {key}"), errors) {
                    out.push(m);
                }
            }
            _ => errors
                .push(format!("{ctx}: {key}: entries must be program names like \"game.exe\" or {{ exe = \"...\" }}")),
        }
    }
    out
}

fn parse_matcher_table(t: &Table, ctx: &str, errors: &mut Vec<String>) -> Option<AppMatcher> {
    let exe = get_str(t, "exe", ctx, errors).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let path_prefix = get_str(t, "path", ctx, errors).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let class = get_str(t, "class", ctx, errors).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if exe.is_none() && path_prefix.is_none() && class.is_none() {
        errors.push(format!("{ctx}: needs at least one of exe, path or class"));
        return None;
    }
    Some(AppMatcher { exe, path_prefix, class })
}

fn tables<'a>(root: &'a Table, key: &str, errors: &mut Vec<String>) -> Vec<&'a Table> {
    match root.get(key) {
        None => Vec::new(),
        Some(Item::ArrayOfTables(aot)) => aot.iter().collect(),
        Some(_) => {
            errors.push(format!("'{key}' must be written as [[{key}]] sections"));
            Vec::new()
        }
    }
}

fn check_keys(t: &Table, allowed: &[&str], ctx: &str, warnings: &mut Vec<String>) {
    for (key, _) in t.iter() {
        if !allowed.contains(&key) {
            warnings.push(format!("{ctx}: unknown setting '{key}' is ignored"));
        }
    }
}

fn prefix(ctx: &str) -> String {
    if ctx.is_empty() { String::new() } else { format!("{ctx}: ") }
}

fn get_bool(t: &Table, key: &str, default: bool, ctx: &str, errors: &mut Vec<String>) -> bool {
    match t.get(key) {
        None => default,
        Some(item) => item.as_bool().unwrap_or_else(|| {
            errors.push(format!("{}{key} must be true or false", prefix(ctx)));
            default
        }),
    }
}

fn get_str(t: &Table, key: &str, ctx: &str, errors: &mut Vec<String>) -> Option<String> {
    let item = t.get(key)?;
    match item.as_str() {
        Some(s) => Some(s.to_string()),
        None => {
            errors.push(format!("{}{key} must be text in quotes", prefix(ctx)));
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Comment-preserving edits used by the tray menu.

pub fn edit_set_bool(doc: &mut DocumentMut, key: &str, value: bool) {
    set_value(doc.as_table_mut(), key, Value::from(value));
}

pub fn edit_set_method(doc: &mut DocumentMut, method: Method) {
    set_value(doc.as_table_mut(), "method", Value::from(method.as_str()));
}

fn set_value(table: &mut Table, key: &str, value: Value) {
    match table.get_mut(key) {
        Some(Item::Value(existing)) => {
            // Keep the existing decoration (spacing and trailing comment).
            let decor = existing.decor().clone();
            *existing = value;
            *existing.decor_mut() = decor;
        }
        _ => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// Enables or disables one `[[binding]]`. Returns false if the index does not exist.
pub fn edit_binding_enabled(doc: &mut DocumentMut, index: usize, enabled: bool) -> bool {
    let Some(Item::ArrayOfTables(aot)) = doc.get_mut("binding") else { return false };
    let Some(table) = aot.get_mut(index) else { return false };
    if enabled {
        table.remove("enabled");
    } else {
        set_value(table, "enabled", Value::from(false));
    }
    true
}

/// Adds `[[exclude]] exe = "..."` unless an identical entry exists. Returns true if added.
pub fn edit_add_exclude(doc: &mut DocumentMut, exe: &str) -> bool {
    let exe_lower = exe.to_lowercase();
    let mut table = Table::new();
    table.insert("exe", Item::Value(Value::from(exe)));
    if doc.get("exclude").is_none() {
        // New tables are rendered before the document's trailing comments. Move those comments in
        // front of the first [[exclude]] so the section ends up at the end of the file.
        let trailing = doc.trailing().as_str().unwrap_or("").to_string();
        if !trailing.trim().is_empty() {
            table.decor_mut().set_prefix(format!("{}\n\n", trailing.trim_end()));
            doc.set_trailing("");
        }
        doc.insert("exclude", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let Some(Item::ArrayOfTables(aot)) = doc.get_mut("exclude") else { return false };
    let exists = aot
        .iter()
        .any(|t| t.len() == 1 && t.get("exe").and_then(Item::as_str).is_some_and(|e| e.to_lowercase() == exe_lower));
    if exists {
        return false;
    }
    aot.push(table);
    true
}

/// Removes the `[[exclude]]` entry at `index`, keeping any comment block written above it.
/// Returns false if the entry does not exist.
pub fn edit_remove_exclude(doc: &mut DocumentMut, index: usize) -> bool {
    let Some(Item::ArrayOfTables(aot)) = doc.get_mut("exclude") else { return false };
    if index >= aot.len() {
        return false;
    }
    let prefix = aot
        .get(index)
        .and_then(|t| t.decor().prefix())
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    let keep = prefix.contains('#');
    aot.remove(index);
    if keep {
        if let Some(next) = aot.get_mut(index) {
            let next_prefix = next.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
            next.decor_mut().set_prefix(format!("{}{}", prefix, next_prefix.trim_start_matches('\n')));
        } else {
            let trailing = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing(format!("{}{}", prefix.trim_end_matches('\n'), trailing));
            if !doc.trailing().as_str().unwrap_or("").ends_with('\n') {
                let t = format!("{}\n", doc.trailing().as_str().unwrap_or(""));
                doc.set_trailing(t);
            }
        }
    }
    if matches!(doc.get("exclude"), Some(Item::ArrayOfTables(aot)) if aot.is_empty()) {
        doc.remove("exclude");
    }
    true
}

// ---------------------------------------------------------------------------------------------
// File access.

/// Reads the configuration file, creating it with defaults if it does not exist.
pub fn load_or_create(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            write_atomic(path, DEFAULT_CONFIG)?;
            Ok(DEFAULT_CONFIG.to_string())
        }
        Err(e) => Err(e),
    }
}

/// Writes through a temporary file and a rename so readers never see a partial file.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::VK_CAPITAL;

    #[test]
    fn default_config_is_valid() {
        let c = parse(DEFAULT_CONFIG).unwrap();
        assert!(c.enabled);
        assert_eq!(c.method, Method::WinSpace);
        assert!(c.restore_capslock_on_leave);
        assert_eq!(c.bindings.len(), 1);
        assert_eq!(c.bindings[0].shortcut.vk, VK_CAPITAL);
        assert_eq!(
            c.bindings[0].action,
            Action::SwitchLayout { mode: LayoutMode::Next, cycle: vec![], method: Method::WinSpace }
        );
        assert!(c.excludes.is_empty());
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.candidates(VK_CAPITAL), &[0]);
    }

    #[test]
    fn empty_file_is_valid() {
        let c = parse("").unwrap();
        assert!(c.enabled);
        assert!(c.bindings.is_empty());
    }

    #[test]
    fn full_example_parses() {
        let text = r#"
method = "message"
[[binding]]
keys = "CapsLock"
action = "switch_layout"
[[binding]]
name = "Real Caps"
keys = "Ctrl+Alt+Shift+Space"
action = "capslock"
[[binding]]
keys = "Win+Ctrl+Shift+Space"
action = "capslock"
state = "off"
[[binding]]
keys = "Win+Ctrl+Shift+1"
action = "switch_layout"
mode = "set"
layout = "00000409"
[[binding]]
keys = "RAlt+Space"
action = "switch_layout"
cycle = ["00000409", "00000419"]
[[binding]]
keys = "Ctrl+J"
action = "send_keys"
send = "Down"
repeat = true
apps_only = ["Notepad.exe", { class = "Chrome_WidgetWin_1" }]
[[binding]]
keys = "Ctrl+Q"
action = "block"
apps_except = ["code.exe"]
enabled = false
[[exclude]]
exe = "Game.exe"
[[exclude]]
path = "D:\\Games\\"
"#;
        let c = parse(text).unwrap();
        assert_eq!(c.bindings.len(), 7);
        assert_eq!(
            c.bindings[0].action,
            Action::SwitchLayout { mode: LayoutMode::Next, cycle: vec![], method: Method::Message }
        );
        assert_eq!(c.bindings[2].action, Action::CapsLock(CapsState::Off));
        assert_eq!(
            c.bindings[3].action,
            Action::SwitchLayout { mode: LayoutMode::Set(0x409), cycle: vec![], method: Method::Message }
        );
        assert_eq!(
            c.bindings[4].action,
            Action::SwitchLayout { mode: LayoutMode::Next, cycle: vec![0x409, 0x419], method: Method::Message }
        );
        assert!(c.bindings[5].repeat);
        assert_eq!(c.bindings[5].apps_only[0].exe.as_deref(), Some("notepad.exe"));
        assert_eq!(c.bindings[5].apps_only[1].class.as_deref(), Some("Chrome_WidgetWin_1"));
        assert!(!c.bindings[6].enabled);
        assert!(c.candidates(b'Q').is_empty(), "disabled bindings are not compiled");
        assert_eq!(c.excludes.len(), 2);
        assert!(c.is_excluded(&AppInfo::from_path(r"C:\x\GAME.EXE", "")));
        assert!(c.is_excluded(&AppInfo::from_path(r"D:\Games\Other\o.exe", "")));
        assert!(!c.is_excluded(&AppInfo::from_path(r"C:\x\notepad.exe", "")));
    }

    #[test]
    fn errors_are_reported_with_context() {
        let text = r#"
method = "fast"
[[binding]]
name = "Typo"
keys = "Ctrl+Spcae"
action = "switch_layout"
[[binding]]
keys = "A"
action = "launch"
[[binding]]
keys = "B"
action = "switch_layout"
mode = "set"
layout = "00000409"
method = "win_space"
[[binding]]
action = "block"
"#;
        let errors = parse(text).unwrap_err();
        let all = errors.join("\n");
        assert!(all.contains("method: unknown value 'fast'"), "{all}");
        assert!(all.contains("binding 1 (\"Typo\"): keys:"), "{all}");
        assert!(all.contains("unknown key 'Spcae'"), "{all}");
        assert!(all.contains("binding 2: unknown action 'launch'"), "{all}");
        assert!(all.contains("binding 3: mode \"set\""), "{all}");
        assert!(all.contains("binding 4: 'keys' is required"), "{all}");
    }

    #[test]
    fn syntax_error_is_reported() {
        assert!(parse("enabled = ").is_err());
        assert!(parse("binding = 5").is_err());
        assert!(parse("enabled = \"yes\"").is_err());
    }

    #[test]
    fn warnings_for_unknown_keys_and_duplicates() {
        let text = r#"
colour = "red"
[[binding]]
keys = "CapsLock"
action = "switch_layout"
speed = 3
[[binding]]
keys = "capslock"
action = "block"
"#;
        let c = parse(text).unwrap();
        let all = c.warnings.join("\n");
        assert!(all.contains("unknown setting 'colour'"), "{all}");
        assert!(all.contains("binding 1: unknown setting 'speed'"), "{all}");
        assert!(all.contains("bindings 1 and 2 use the same shortcut CapsLock"), "{all}");
        assert_eq!(c.candidates(VK_CAPITAL), &[0, 1]);
    }

    #[test]
    fn priority_prefers_program_specific_then_side_specific() {
        let text = r#"
[[binding]]
keys = "Ctrl+A"
action = "block"
[[binding]]
keys = "LCtrl+A"
action = "block"
[[binding]]
keys = "Ctrl+A"
action = "block"
apps_only = ["x.exe"]
"#;
        let c = parse(text).unwrap();
        assert_eq!(c.candidates(b'A'), &[2, 1, 0]);
    }

    #[test]
    fn edits_preserve_comments() {
        let mut doc: DocumentMut = DEFAULT_CONFIG.parse().unwrap();
        edit_set_bool(&mut doc, "enabled", false);
        edit_set_method(&mut doc, Method::Message);
        assert!(edit_binding_enabled(&mut doc, 0, false));
        assert!(!edit_binding_enabled(&mut doc, 5, false));
        assert!(edit_add_exclude(&mut doc, "Game.exe"));
        assert!(!edit_add_exclude(&mut doc, "game.EXE"));
        assert!(edit_add_exclude(&mut doc, "other.exe"));
        let text = doc.to_string();
        assert!(
            text.ends_with(
                "# exe = \"SomeGame.exe\"\n\n[[exclude]]\nexe = \"Game.exe\"\n\n[[exclude]]\nexe = \"other.exe\"\n"
            ),
            "{text}"
        );
        assert!(text.contains("# How switch_layout changes the layout by default:"), "{text}");
        assert!(text.contains("# [[exclude]]"), "{text}");
        let c = parse(&text).unwrap();
        assert!(!c.enabled);
        assert_eq!(c.method, Method::Message);
        assert!(!c.bindings[0].enabled);
        assert_eq!(c.excludes.len(), 2);

        let mut doc: DocumentMut = text.parse().unwrap();
        assert!(edit_binding_enabled(&mut doc, 0, true));
        assert!(edit_remove_exclude(&mut doc, 0));
        assert!(!edit_remove_exclude(&mut doc, 3));
        let c = parse(&doc.to_string()).unwrap();
        assert!(c.bindings[0].enabled);
        assert_eq!(c.excludes, vec![AppMatcher::exe("other.exe")]);

        let mut doc: DocumentMut = doc.to_string().parse().unwrap();
        assert!(edit_remove_exclude(&mut doc, 0));
        let text = doc.to_string();
        assert!(parse(&text).unwrap().excludes.is_empty());
        assert!(!text.lines().any(|l| l.trim() == "[[exclude]]"), "{text}");
        assert!(text.ends_with("# exe = \"SomeGame.exe\"\n"), "{text}");
        assert!(text.contains("# More examples"), "{text}");

        // Adding again after everything was removed still works.
        let mut doc: DocumentMut = text.parse().unwrap();
        assert!(edit_add_exclude(&mut doc, "again.exe"));
        let text = doc.to_string();
        assert!(text.ends_with("# exe = \"SomeGame.exe\"\n\n[[exclude]]\nexe = \"again.exe\"\n"), "{text}");
    }

    #[test]
    fn klid_parsing() {
        assert_eq!(parse_klid("00000419"), Some(0x419));
        assert_eq!(parse_klid("0001041F"), Some(0x1041F));
        assert_eq!(parse_klid("419"), None);
        assert_eq!(parse_klid("0000041G"), None);
    }

    #[test]
    fn scope_rules() {
        let text = r#"
[[binding]]
keys = "A"
action = "block"
apps_only = ["a.exe"]
[[binding]]
keys = "B"
action = "block"
apps_except = ["a.exe"]
"#;
        let c = parse(text).unwrap();
        let a = AppInfo::from_path(r"C:\a.exe", "");
        let other = AppInfo::from_path(r"C:\b.exe", "");
        assert!(c.bindings[0].scope_allows(Some(&a)));
        assert!(!c.bindings[0].scope_allows(Some(&other)));
        assert!(!c.bindings[0].scope_allows(None));
        assert!(!c.bindings[1].scope_allows(Some(&a)));
        assert!(c.bindings[1].scope_allows(Some(&other)));
        assert!(c.bindings[1].scope_allows(None));
    }
}
