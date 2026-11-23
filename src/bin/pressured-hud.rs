// pressured-hud — the control window for the pressured daemon.
//
// Thin client: all privilege lives in the daemon, reached over /run/pressured.sock.
// This binary renders live state and sends list/act requests. Build with:
//   cargo build --release --features hud
//
// Design: a native GNOME (libadwaita) window in the steel-grey of the shell's own
// prompts. A status block says in plain words what the machine is doing; three
// cards show memory, swap and the last minute of pressure; the apps are a boxed
// list with each app's real icon and name, and the actions for an app appear on
// the row you point at or select. Rows update in place and only re-sort while the
// pointer is outside the list, so nothing moves under the cursor. Every colour is
// an Adwaita palette colour, so it follows light and dark.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4 as gtk;
use libadwaita as adw;
use gtk::glib;
use gtk::pango;
// adw's prelude re-exports gtk's, so this one glob covers both toolkits.
use adw::prelude::*;

const SOCKET_PATH: &str = "/run/pressured.sock";
const APP_ID: &str = "dev.pressured.Hud";

/// Bumped on every (re)build of the window. The live-refresh timer captures the
/// generation it was born under and stops itself once a newer window exists, so
/// recreate-on-summon never leaks an orphaned socket-polling timer.
static HUD_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone)]
struct App {
    id: String,
    name: String,
    mem: u64,
    swap: u64,
    frozen: bool,
    protected: bool,
    freezable: bool,
    hog: bool,
}

/// How an app is shown: its real name and icon where a desktop file names it.
#[derive(Clone)]
struct Look {
    title: String,
    kind: Option<String>,
    icon: Icon,
}

#[derive(Clone)]
enum Icon {
    Named(String),
    File(PathBuf),
}

#[derive(Clone)]
struct Row {
    root: gtk::ListBoxRow,
    icon: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
    meter: gtk::ProgressBar,
    badge: gtk::Label,
    btn_pin: gtk::Button,
    btn_cap: gtk::Button,
    btn_pause: gtk::Button,
    btn_close: gtk::Button,
}

struct Stat {
    value: gtk::Label,
    detail: gtk::Label,
    meter: gtk::ProgressBar,
}

fn call(req: &serde_json::Value) -> Option<serde_json::Value> {
    let mut s = UnixStream::connect(SOCKET_PATH).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    writeln!(s, "{}", req).ok()?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).ok()?;
    serde_json::from_str(resp.trim()).ok()
}

fn human_bytes(b: u64) -> String {
    const G: f64 = (1024 * 1024 * 1024) as f64;
    const M: f64 = (1024 * 1024) as f64;
    let bf = b as f64;
    if bf >= 10.0 * G {
        format!("{:.0} GB", bf / G)
    } else if bf >= G {
        format!("{:.1} GB", bf / G)
    } else if bf >= M {
        format!("{:.0} MB", bf / M)
    } else {
        format!("{} B", b)
    }
}

/// Plain-language relative age for the activity trail.
fn ago_str(secs: u64) -> String {
    if secs < 45 {
        "just now".to_string()
    } else if secs < 5400 {
        format!("{} min ago", (secs + 30) / 60)
    } else {
        format!("{} h ago", (secs + 1800) / 3600)
    }
}

fn set_level(w: &impl IsA<gtk::Widget>, level: &str) {
    for c in ["good", "warn", "crit"] {
        w.remove_css_class(c);
    }
    w.add_css_class(level);
}

fn share_level(share: f64) -> &'static str {
    if share > 0.20 {
        "crit"
    } else if share > 0.10 {
        "warn"
    } else {
        "good"
    }
}

// ---------------------------------------------------------------- naming ----

/// Undo systemd's unit-name escaping (\x2d -> '-').
fn unescape_unit(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn strip_pid(s: &str) -> &str {
    match s.rfind('-') {
        Some(p) if !s[p + 1..].is_empty() && s[p + 1..].chars().all(|c| c.is_ascii_digit()) => &s[..p],
        _ => s,
    }
}

/// Desktop-file ids a cgroup unit could belong to, most likely first.
fn desktop_candidates(unit: &str) -> Vec<String> {
    let raw = unescape_unit(
        unit.trim_end_matches(".scope").trim_end_matches(".service").trim_end_matches(".slice"),
    );
    let mut out = Vec::new();
    if let Some(rest) = raw.strip_prefix("snap.") {
        let mut parts = rest.split('.');
        if let (Some(snap), Some(app)) = (parts.next(), parts.next()) {
            let app = app.split('-').next().unwrap_or(app);
            out.push(format!("{snap}_{app}"));
            out.push(format!("{snap}_{snap}"));
        }
        return out;
    }
    let body = ["app-gnome-", "app-vicinae-", "app-flatpak-", "app-kde-", "app-"]
        .iter()
        .find_map(|p| raw.strip_prefix(p))
        .unwrap_or(&raw);
    let body = body.split('@').next().unwrap_or(body);
    let id = strip_pid(body);
    out.push(id.to_string());
    out.push(id.to_lowercase());
    if let Some(last) = id.rsplit('.').next() {
        out.push(last.to_lowercase());
    }
    out
}

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    dirs.push(
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".local/share")),
    );
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    for d in sys.split(':').filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(d));
    }
    for d in [
        format!("{home}/.local/share/flatpak/exports/share"),
        "/var/lib/flatpak/exports/share".to_string(),
        "/var/lib/snapd/desktop".to_string(),
        "/usr/local/share".to_string(),
        "/usr/share".to_string(),
    ] {
        let p = PathBuf::from(d);
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    dirs
}

/// Name and icon from a desktop file, if one exists for this id.
fn read_desktop(id: &str, dirs: &[PathBuf]) -> Option<(String, Option<String>)> {
    for d in dirs {
        let path = d.join("applications").join(format!("{id}.desktop"));
        if !path.is_file() {
            continue;
        }
        let kf = glib::KeyFile::new();
        if kf.load_from_file(&path, glib::KeyFileFlags::NONE).is_err() {
            continue;
        }
        let name = kf.locale_string("Desktop Entry", "Name", None).ok()?.to_string();
        let icon = kf.string("Desktop Entry", "Icon").ok().map(|s| s.to_string());
        return Some((name, icon));
    }
    None
}

/// Docker container names by full id, refreshed every 15 s (the CLI works for
/// members of the docker group; without it, containers keep their short id).
struct DockerNames {
    at: Option<Instant>,
    names: HashMap<String, String>,
}

impl DockerNames {
    fn get(&mut self, id: &str) -> Option<String> {
        if self.at.is_none_or(|t| t.elapsed() > Duration::from_secs(15)) {
            self.at = Some(Instant::now());
            self.names.clear();
            if let Ok(out) = std::process::Command::new("docker")
                .args(["ps", "--no-trunc", "--format", "{{.ID}}\t{{.Names}}"])
                .stderr(std::process::Stdio::null())
                .output()
            {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some((cid, name)) = line.split_once('\t') {
                        self.names.insert(cid.to_string(), name.to_string());
                    }
                }
            }
        }
        self.names.get(id).cloned()
    }
}

struct Namer {
    dirs: Vec<PathBuf>,
    cache: HashMap<String, Option<(String, Option<String>)>>,
    docker: DockerNames,
}

impl Namer {
    fn new() -> Self {
        Namer {
            dirs: data_dirs(),
            cache: HashMap::new(),
            docker: DockerNames { at: None, names: HashMap::new() },
        }
    }

    fn desktop(&mut self, id: &str) -> Option<(String, Option<String>)> {
        if let Some(hit) = self.cache.get(id) {
            return hit.clone();
        }
        let found = read_desktop(id, &self.dirs);
        self.cache.insert(id.to_string(), found.clone());
        found
    }

    fn look(&mut self, a: &App) -> Look {
        let unit = a.id.rsplit('/').next().unwrap_or(&a.id).to_string();
        let raw = unescape_unit(&unit);
        let icon_for = |name: Option<String>| match name {
            Some(n) if n.starts_with('/') => Icon::File(PathBuf::from(n)),
            Some(n) => Icon::Named(n),
            None => Icon::Named("system-run-symbolic".into()),
        };
        // Agents T3 Code runs: one scope per session.
        for (prefix, title) in [("t3code-claude-", "Claude agent"), ("t3code-acp-", "Agent"),
                                ("t3code-codex-", "Codex agent")] {
            if raw.starts_with(prefix) {
                let icon = self.desktop("com.t3tools.T3Code").and_then(|d| d.1);
                return Look { title: title.into(), kind: Some("T3 Code".into()), icon: icon_for(icon) };
            }
        }
        if let Some(cid) = raw.strip_prefix("docker-").map(|s| s.trim_end_matches(".scope")) {
            let name = self.docker.get(cid).unwrap_or_else(|| format!("Container {}", &cid[..cid.len().min(12)]));
            return Look {
                title: name,
                kind: Some("Docker container".into()),
                icon: Icon::Named("package-x-generic-symbolic".into()),
            };
        }
        for cand in desktop_candidates(&unit) {
            if let Some((name, icon)) = self.desktop(&cand) {
                return Look { title: name, kind: None, icon: icon_for(icon) };
            }
        }
        Look { title: a.name.clone(), kind: None, icon: icon_for(None) }
    }
}

// ------------------------------------------------------------------- CSS ----

const CSS: &str = "
/* The shell's own prompts: dialog grey, slate accent (all Adwaita palette). */
window.rtux {
  background-color: var(--dialog-bg-color);
  color: var(--dialog-fg-color);
  --accent-bg-color: var(--accent-slate);
}
window.rtux headerbar { background: transparent; box-shadow: none; }

.status-icon {
  min-width: 48px; min-height: 48px;
  border-radius: 999px;
  background-color: alpha(currentColor, 0.14);
}
.status-icon.good { color: var(--success-color); }
.status-icon.warn { color: var(--warning-color); }
.status-icon.crit { color: var(--error-color); }
.status-title { font-weight: 800; font-size: 1.35em; }

.stat { padding: 12px 14px; border-radius: 12px; }
.stat .stat-caption { font-size: 0.8em; font-weight: 700; opacity: 0.6; }
.stat .stat-value { font-size: 1.45em; font-weight: 800; font-feature-settings: 'tnum' 1; }
.stat .stat-detail { font-size: 0.85em; opacity: 0.6; font-feature-settings: 'tnum' 1; }

progressbar.level, progressbar.level > trough, progressbar.level > trough > progress { min-height: 6px; }
progressbar.level > trough { border-radius: 99px; background-color: alpha(currentColor, 0.10); }
progressbar.level > trough > progress { border-radius: 99px; transition: background-color 400ms ease; }
progressbar.level.good > trough > progress { background-color: var(--success-bg-color); }
progressbar.level.warn > trough > progress { background-color: var(--warning-bg-color); }
progressbar.level.crit > trough > progress { background-color: var(--error-bg-color); }

.activity { font-size: 0.85em; opacity: 0.6; }
.section-title { font-weight: 700; }
.section-detail { opacity: 0.6; font-feature-settings: 'tnum' 1; }

/* GTK's default progress bar is 150px wide; a row's share bar is a glance. */
progressbar.row-meter, progressbar.row-meter > trough { min-width: 56px; }
.apps > row { padding: 8px 10px 8px 12px; transition: background-color 160ms ease; }
.apps > row .app-title { font-weight: 600; }
.apps > row .app-subtitle { font-size: 0.88em; opacity: 0.6; font-feature-settings: 'tnum' 1; }
.apps > row.frozen .app-icon, .apps > row.frozen .app-title { opacity: 0.55; }
.apps > row .row-actions { opacity: 0; transition: opacity 140ms ease; }
.apps > row:hover .row-actions, .apps > row:selected .row-actions, .apps > row:focus-within .row-actions { opacity: 1; }
.apps > row:selected { background-color: alpha(currentColor, 0.08); }
.row-actions button { min-width: 30px; min-height: 30px; padding: 0; }
.row-actions button.danger:hover { color: var(--error-color); background-color: alpha(var(--error-color), 0.14); }

.badge { font-size: 0.75em; font-weight: 700; padding: 2px 8px; border-radius: 99px;
         background-color: alpha(currentColor, 0.12); }
.badge.paused { color: var(--accent-color); }
.badge.pinned { color: var(--accent-color); }
.badge.system { opacity: 0.7; }
.badge.top    { color: var(--warning-color); }
";

// -------------------------------------------------------------------- UI ----

fn main() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run_with_args::<&str>(&[]);
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

fn level_bar() -> gtk::ProgressBar {
    let pb = gtk::ProgressBar::new();
    pb.set_show_text(false);
    pb.set_hexpand(true);
    pb.set_valign(gtk::Align::Center);
    pb.add_css_class("level");
    pb.add_css_class("good");
    pb
}

/// A stat card: caption, big value, detail, and a level bar (or a sparkline).
fn stat_card(caption: &str, help: &str) -> (gtk::Box, Stat) {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
    card.add_css_class("card");
    card.add_css_class("stat");
    card.set_hexpand(true);
    card.set_tooltip_text(Some(help));
    card.append(&label(caption, &["stat-caption"]));
    let value = label("—", &["stat-value"]);
    let detail = label("", &["stat-detail"]);
    detail.set_ellipsize(pango::EllipsizeMode::End);
    let meter = level_bar();
    meter.set_margin_top(6);
    card.append(&value);
    card.append(&detail);
    card.append(&meter);
    (card, Stat { value, detail, meter })
}

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("flat");
    b.add_css_class("circular");
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some(tip));
    b.update_property(&[gtk::accessible::Property::Label(tip)]);
    b
}

fn build_ui(app: &adw::Application) {
    // Re-summoning the HUD *recreates* the window rather than present()ing the
    // existing one: on Wayland a GNOME custom keybinding spawns us with no
    // activation token, so present() couldn't raise a mapped window, while a
    // freshly mapped one gets focus. The HUD is a stateless view of the daemon.
    for win in app.windows() {
        win.destroy();
    }
    let my_gen = HUD_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    // Developer aid: render the window to a PNG and quit, with no keys or actions.
    let snapshot_to = std::env::var("PRESSURED_HUD_SNAPSHOT").ok();

    // Ask the daemon to keep us resident + OOM-protected so the control surface
    // stays operable exactly when it's needed most (under heavy pressure).
    let _ = call(&serde_json::json!({"cmd": "pin_self", "pid": std::process::id()}));

    static CSS_ONCE: std::sync::Once = std::sync::Once::new();
    CSS_ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    // ---- window chrome ----
    let title = adw::WindowTitle::new("Memory", "Checking\u{2026}");
    let header_bar = adw::HeaderBar::new();
    header_bar.set_title_widget(Some(&title));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header_bar);
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(560)
        .default_height(680)
        .content(&toolbar)
        .build();
    window.set_title(Some("Memory \u{b7} rtux"));
    window.add_css_class("rtux");

    // ---- content ----
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(6);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);

    // status: icon in a tinted circle + a plain sentence + the detail
    let status_box = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    let status_icon = gtk::Image::from_icon_name("emblem-ok-symbolic");
    status_icon.set_pixel_size(24);
    status_icon.add_css_class("status-icon");
    status_icon.add_css_class("good");
    status_icon.set_valign(gtk::Align::Center);
    let status_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    status_text.set_valign(gtk::Align::Center);
    let status_title = label("Checking\u{2026}", &["status-title"]);
    status_title.set_wrap(true);
    let status_detail = label("", &["dim-label"]);
    status_detail.set_wrap(true);
    status_text.append(&status_title);
    status_text.append(&status_detail);
    status_box.append(&status_icon);
    status_box.append(&status_text);
    content.append(&status_box);

    // three stat cards
    let cards = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    cards.set_homogeneous(true);
    let (ram_card, ram) = stat_card("MEMORY", "Fast memory (RAM) in use, out of the total.");
    let (swap_card, swap) = stat_card(
        "SWAP",
        "Backup memory used when RAM fills up. Some is compressed and still quick; \
         some is on disk and slow.",
    );
    let (press_card, press) = stat_card(
        "PRESSURE",
        "How hard the computer is working to find free memory, over the last minute. \
         The faint line marks where it would start to stutter.",
    );
    // Pressure card: the sparkline in place of a level bar.
    press.meter.set_visible(false);
    let (spark, spark_data) = sparkline();
    spark.set_margin_top(2);
    press_card.append(&spark);
    cards.append(&ram_card);
    cards.append(&swap_card);
    cards.append(&press_card);
    content.append(&cards);

    // what rtux recently did, quietly
    let activity_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    activity_box.add_css_class("activity");
    let activity_icon = gtk::Image::from_icon_name("document-open-recent-symbolic");
    let activity = label("", &[]);
    activity.set_ellipsize(pango::EllipsizeMode::End);
    activity.set_hexpand(true);
    activity_box.append(&activity_icon);
    activity_box.append(&activity);
    activity_box.set_visible(false);
    content.append(&activity_box);

    // apps header
    let apps_head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    apps_head.set_margin_top(4);
    let apps_title = label("Apps", &["section-title"]);
    apps_title.set_hexpand(true);
    let apps_detail = label("", &["section-detail"]);
    apps_head.append(&apps_title);
    apps_head.append(&apps_detail);
    content.append(&apps_head);

    // apps list
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.add_css_class("apps");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_valign(gtk::Align::Start);
    let mems: Rc<RefCell<HashMap<String, u64>>> = Rc::new(RefCell::new(HashMap::new()));
    {
        let mems = mems.clone();
        list.set_sort_func(move |a, b| {
            let m = mems.borrow();
            let ma = m.get(a.widget_name().as_str()).copied().unwrap_or(0);
            let mb = m.get(b.widget_name().as_str()).copied().unwrap_or(0);
            mb.cmp(&ma).into()
        });
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    content.append(&scroller);

    // Freeze re-sorting while the pointer is inside, so rows hold still to click.
    let hovering = Rc::new(Cell::new(false));
    {
        let motion = gtk::EventControllerMotion::new();
        let h1 = hovering.clone();
        motion.connect_enter(move |_, _, _| h1.set(true));
        let h2 = hovering.clone();
        motion.connect_leave(move |_| h2.set(false));
        scroller.add_controller(motion);
    }

    // unreachable: a proper status page instead of the panel
    let retry = gtk::Button::with_label("Try Again");
    retry.add_css_class("pill");
    retry.set_halign(gtk::Align::Center);
    let unreachable = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title("Can't Reach rtux")
        .description(
            "The pressured service isn't running, or this account can't open its \
             control socket. Check it with: systemctl status rtux.service",
        )
        .child(&retry)
        .build();

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.add_named(&content, Some("panel"));
    stack.add_named(&unreachable, Some("unreachable"));
    toolbar.set_content(Some(&stack));

    // ---- shared state ----
    let rows: Rc<RefCell<HashMap<String, Row>>> = Rc::new(RefCell::new(HashMap::new()));
    let apps: Rc<RefCell<HashMap<String, App>>> = Rc::new(RefCell::new(HashMap::new()));
    let looks: Rc<RefCell<HashMap<String, Look>>> = Rc::new(RefCell::new(HashMap::new()));
    let selected: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let namer = Rc::new(RefCell::new(Namer::new()));
    let refresh_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let do_refresh = {
        let cell = refresh_cell.clone();
        move || {
            let r = cell.borrow().clone();
            if let Some(r) = r {
                r();
            }
        }
    };

    // ---- actions (pin / cap / pause / close), for any row ----
    let act: Rc<dyn Fn(&str, &str)> = {
        let apps = apps.clone();
        let looks = looks.clone();
        let window = window.clone();
        let do_refresh = do_refresh.clone();
        let snapshot = snapshot_to.is_some();
        Rc::new(move |action: &str, id: &str| {
            if snapshot {
                return;
            }
            let Some(a) = apps.borrow().get(id).cloned() else { return };
            let send = {
                let id = id.to_string();
                let do_refresh = do_refresh.clone();
                move |resolved: &str| {
                    let _ = call(&serde_json::json!({"cmd": "act", "action": resolved, "id": id}));
                    do_refresh();
                }
            };
            match action {
                "pause" => send(if a.frozen { "thaw" } else { "freeze" }),
                "pin" => send(if a.protected { "unprotect" } else { "protect" }),
                "cap" => send("cap"),
                "kill" => {
                    // The one action that can't be undone: always confirm.
                    let name = looks.borrow().get(id).map(|l| l.title.clone()).unwrap_or(a.name.clone());
                    let dialog = adw::AlertDialog::new(
                        Some(&format!("Close {name}?")),
                        Some("It stops right away, and anything unsaved in it is lost. \
                              Pausing it instead can be undone."),
                    );
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("close", "Close");
                    dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    dialog.connect_response(None, move |_, resp| {
                        if resp == "close" {
                            send("kill");
                        }
                    });
                    dialog.present(Some(&window));
                }
                _ => {}
            }
        })
    };

    {
        let selected = selected.clone();
        list.connect_row_selected(move |_, row| {
            *selected.borrow_mut() = row.map(|r| r.widget_name().to_string());
        });
    }

    // ---- refresh: update everything in place ----
    let refresh: Rc<dyn Fn()> = {
        let list = list.clone();
        let stack = stack.clone();
        let rows = rows.clone();
        let apps = apps.clone();
        let looks = looks.clone();
        let mems = mems.clone();
        let selected = selected.clone();
        let hovering = hovering.clone();
        let namer = namer.clone();
        let act = act.clone();
        let title = title.clone();
        Rc::new(move || {
            let reply = match call(&serde_json::json!({"cmd": "list"})) {
                Some(r) => r,
                None => {
                    stack.set_visible_child_name("unreachable");
                    title.set_subtitle("rtux unreachable");
                    return;
                }
            };
            stack.set_visible_child_name("panel");

            // pressure, memory, swap
            let some = reply["some_avg10"].as_f64().unwrap_or(0.0);
            let full = reply["full_avg10"].as_f64().unwrap_or(0.0);
            let (pword, plevel) = if some > 25.0 || full > 10.0 {
                ("Critical", "crit")
            } else if some > 5.0 {
                ("Elevated", "warn")
            } else {
                ("Normal", "good")
            };
            press.value.set_text(pword);
            press.detail.set_text(&format!("{some:.1}% stalled"));
            {
                let mut buf = spark_data.borrow_mut();
                match reply["pressure_trend"].as_array() {
                    Some(arr) if !arr.is_empty() => {
                        buf.clear();
                        buf.extend(arr.iter().filter_map(|v| v.as_f64()));
                    }
                    _ => {
                        buf.push(some);
                        let len = buf.len();
                        if len > 60 {
                            buf.drain(0..len - 60);
                        }
                    }
                }
            }
            spark.queue_draw();

            let mem_total = reply["mem_total"].as_u64().unwrap_or(1).max(1);
            let mem_used = reply["mem_used"].as_u64().unwrap_or(0);
            let swap_total = reply["swap_total"].as_u64().unwrap_or(0);
            let swap_used = reply["swap_used"].as_u64().unwrap_or(0);
            let ram_frac = mem_used as f64 / mem_total as f64;
            ram.value.set_text(&human_bytes(mem_used));
            ram.detail.set_text(&format!("of {}", human_bytes(mem_total)));
            ram.meter.set_fraction(ram_frac.clamp(0.0, 1.0));
            set_level(&ram.meter, if ram_frac > 0.9 { "crit" } else if ram_frac > 0.75 { "warn" } else { "good" });
            if swap_total > 0 {
                let sf = swap_used as f64 / swap_total as f64;
                swap.value.set_text(&human_bytes(swap_used));
                swap.detail.set_text(&format!("of {}", human_bytes(swap_total)));
                swap.meter.set_fraction(sf.clamp(0.0, 1.0));
                set_level(&swap.meter, if sf > 0.8 { "crit" } else if sf > 0.5 { "warn" } else { "good" });
            } else {
                swap.value.set_text("None");
                swap.detail.set_text("no swap");
                swap.meter.set_fraction(0.0);
            }

            let parsed = parse_apps(&reply);

            // The status sentence states only what's true: a paused count comes
            // from live state. Paused apps mean the machine is coping (amber).
            let paused = parsed.iter().filter(|a| a.frozen).count();
            let (sentence, slevel, icon) = if paused > 0 {
                (
                    format!("Paused {} background app{}", paused, if paused == 1 { "" } else { "s" }),
                    "warn",
                    "media-playback-pause-symbolic",
                )
            } else if plevel == "crit" {
                ("Under heavy load".to_string(), "crit", "dialog-warning-symbolic")
            } else if plevel == "warn" {
                ("Getting busy".to_string(), "warn", "dialog-warning-symbolic")
            } else {
                ("Running smoothly".to_string(), "good", "emblem-ok-symbolic")
            };
            status_title.set_text(&sentence);
            status_icon.set_icon_name(Some(icon));
            set_level(&status_icon, slevel);
            let detail = match slevel {
                "good" => "Plenty of memory for what you're running. Your foreground app stays fast.".to_string(),
                _ if paused > 0 => "Paused apps resume by themselves once memory frees up.".to_string(),
                _ => "rtux keeps your foreground app fast and pauses background ones if needed.".to_string(),
            };
            status_detail.set_text(&detail);
            title.set_subtitle(&sentence);

            // recent activity: "Paused Chrome · 2 min ago"
            if let Some(evs) = reply["recent"].as_array().filter(|a| !a.is_empty()) {
                let fmt = |e: &serde_json::Value| {
                    format!(
                        "{} \u{b7} {}",
                        e["text"].as_str().unwrap_or(""),
                        ago_str(e["ago_secs"].as_u64().unwrap_or(0))
                    )
                };
                activity.set_text(&fmt(&evs[0]));
                activity_box.set_tooltip_text(Some(&evs.iter().map(&fmt).collect::<Vec<_>>().join("\n")));
                activity_box.set_visible(true);
            } else {
                activity_box.set_visible(false);
            }

            let total_apps: u64 = parsed.iter().map(|a| a.mem).sum();
            apps_detail.set_text(&format!(
                "{} app{} \u{b7} {}",
                parsed.len(),
                if parsed.len() == 1 { "" } else { "s" },
                human_bytes(total_apps)
            ));

            let mut seen: HashSet<String> = HashSet::new();
            for a in &parsed {
                seen.insert(a.id.clone());
                mems.borrow_mut().insert(a.id.clone(), a.mem);
                apps.borrow_mut().insert(a.id.clone(), a.clone());
                let look = looks
                    .borrow_mut()
                    .entry(a.id.clone())
                    .or_insert_with(|| namer.borrow_mut().look(a))
                    .clone();
                let mut rows_m = rows.borrow_mut();
                if let Some(row) = rows_m.get(&a.id) {
                    update_row(row, a, &look, mem_total);
                } else {
                    let row = build_row(a, &look, mem_total, &act);
                    list.append(&row.root);
                    rows_m.insert(a.id.clone(), row);
                }
            }
            let gone: Vec<String> = rows.borrow().keys().filter(|k| !seen.contains(*k)).cloned().collect();
            for id in gone {
                if let Some(row) = rows.borrow_mut().remove(&id) {
                    list.remove(&row.root);
                }
                mems.borrow_mut().remove(&id);
                apps.borrow_mut().remove(&id);
                looks.borrow_mut().remove(&id);
                if selected.borrow().as_deref() == Some(id.as_str()) {
                    *selected.borrow_mut() = None;
                }
            }
            if !hovering.get() {
                list.invalidate_sort();
            }
        })
    };
    *refresh_cell.borrow_mut() = Some(refresh.clone());
    {
        let refresh = refresh.clone();
        retry.connect_clicked(move |_| refresh());
    }

    // Keys: Esc closes; P/C/I and Delete act on the selected app.
    if snapshot_to.is_none() {
        let keys = gtk::EventControllerKey::new();
        let w = window.clone();
        let selected = selected.clone();
        let act = act.clone();
        keys.connect_key_pressed(move |_, k, _, _| {
            use gtk::gdk::Key;
            if k == Key::Escape {
                w.close();
                return glib::Propagation::Stop;
            }
            let hit = match k {
                Key::p | Key::P => Some("pause"),
                Key::c | Key::C => Some("cap"),
                Key::i | Key::I => Some("pin"),
                // Close: deliberate Delete only (and it asks first).
                Key::Delete => Some("kill"),
                _ => None,
            };
            let id = selected.borrow().clone();
            match (hit, id) {
                (Some(a), Some(id)) => {
                    act(a, &id);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        window.add_controller(keys);
    }

    refresh();
    if let Some(row) = list.row_at_index(0) {
        list.select_row(Some(&row));
    }
    {
        let refresh = refresh.clone();
        glib::timeout_add_seconds_local(1, move || {
            if HUD_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                return glib::ControlFlow::Break;
            }
            refresh();
            glib::ControlFlow::Continue
        });
    }

    window.present();
    list.grab_focus();

    if let Some(path) = snapshot_to {
        let w = window.clone();
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            let (width, height) = (w.width(), w.height());
            let paintable = gtk::WidgetPaintable::new(Some(&w));
            let snap = gtk::Snapshot::new();
            paintable.snapshot(&snap, width as f64, height as f64);
            if let (Some(node), Some(renderer)) = (snap.to_node(), w.renderer()) {
                let tex = renderer.render_texture(&node, None);
                let _ = tex.save_to_png(&path);
            }
            app.quit();
        });
    }
}

/// The last minute of pressure (`some.avg10`, percent) over a 0–50 ceiling, with
/// a faint guide at the critical level (25), in the Adwaita colour of its level.
fn sparkline() -> (gtk::DrawingArea, Rc<RefCell<Vec<f64>>>) {
    let data: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));
    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_content_height(28);
    let d = data.clone();
    area.set_draw_func(move |_, cr, w, h| {
        let data = d.borrow();
        let (w, h) = (w as f64, h as f64);
        const CEIL: f64 = 50.0;
        let pad = 2.0;
        let plot_h = (h - 2.0 * pad).max(1.0);
        let y = |v: f64| pad + plot_h * (1.0 - (v / CEIL).clamp(0.0, 1.0));
        // A neutral grey reads in light and dark alike.
        cr.set_line_width(1.0);
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.35);
        cr.set_dash(&[3.0, 3.0], 0.0);
        cr.move_to(0.0, y(25.0));
        cr.line_to(w, y(25.0));
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
        if data.len() < 2 {
            return;
        }
        let n = data.len();
        let x = |i: usize| w * (i as f64) / ((n - 1) as f64);
        let cur = *data.last().unwrap();
        // Adwaita's success / warning / error background colours.
        let (r, g, b) = if cur > 25.0 {
            (0.878, 0.106, 0.141)
        } else if cur > 5.0 {
            (0.898, 0.647, 0.039)
        } else {
            (0.180, 0.761, 0.494)
        };
        let base = y(0.0);
        cr.move_to(0.0, base);
        for (i, v) in data.iter().enumerate() {
            cr.line_to(x(i), y(*v));
        }
        cr.line_to(w, base);
        cr.close_path();
        cr.set_source_rgba(r, g, b, 0.16);
        let _ = cr.fill();
        cr.move_to(x(0), y(data[0]));
        for (i, v) in data.iter().enumerate().skip(1) {
            cr.line_to(x(i), y(*v));
        }
        cr.set_source_rgba(r, g, b, 0.95);
        cr.set_line_width(1.8);
        let _ = cr.stroke();
    });
    (area, data)
}

fn parse_apps(reply: &serde_json::Value) -> Vec<App> {
    let empty = vec![];
    reply["apps"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|a| App {
            id: a["id"].as_str().unwrap_or("").to_string(),
            name: a["name"].as_str().unwrap_or("?").to_string(),
            mem: a["mem_bytes"].as_u64().unwrap_or(0),
            swap: a["swap_bytes"].as_u64().unwrap_or(0),
            frozen: a["frozen"].as_bool().unwrap_or(false),
            protected: a["protected"].as_bool().unwrap_or(false),
            freezable: a["freezable"].as_bool().unwrap_or(false),
            hog: a["flagged"].as_str() == Some("top_consumer"),
        })
        .collect()
}

fn build_row(a: &App, look: &Look, mem_total: u64, act: &Rc<dyn Fn(&str, &str)>) -> Row {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);

    let icon = gtk::Image::new();
    icon.set_pixel_size(32);
    icon.add_css_class("app-icon");
    icon.set_valign(gtk::Align::Center);
    line.append(&icon);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let title_line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let title = label("", &["app-title"]);
    title.set_ellipsize(pango::EllipsizeMode::End);
    let badge = label("", &["badge"]);
    badge.set_valign(gtk::Align::Center);
    title_line.append(&title);
    title_line.append(&badge);
    let subtitle = label("", &["app-subtitle"]);
    subtitle.set_ellipsize(pango::EllipsizeMode::End);
    text.append(&title_line);
    text.append(&subtitle);
    line.append(&text);

    let meter = level_bar();
    meter.set_hexpand(false);
    meter.set_halign(gtk::Align::End);
    meter.set_width_request(56);
    meter.add_css_class("row-meter");
    meter.set_tooltip_text(Some("This app's share of your total memory."));
    line.append(&meter);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    actions.add_css_class("row-actions");
    let btn_pin = icon_button("view-pin-symbolic", "Pin (I): keep this app in fast memory");
    let btn_cap = icon_button(
        "power-profile-power-saver-symbolic",
        "Cap (C): have it give back memory it isn't using, without pausing it",
    );
    let btn_pause = icon_button("media-playback-pause-symbolic", "Pause (P): freeze it; Resume wakes it as it was");
    let btn_close = icon_button("window-close-symbolic", "Close (Delete): quit it; asks first");
    btn_close.add_css_class("danger");
    for (b, action) in [(&btn_pin, "pin"), (&btn_cap, "cap"), (&btn_pause, "pause"), (&btn_close, "kill")] {
        let act = act.clone();
        let id = a.id.clone();
        b.connect_clicked(move |_| act(action, &id));
        actions.append(b);
    }
    line.append(&actions);

    let root = gtk::ListBoxRow::new();
    root.set_widget_name(&a.id);
    root.set_child(Some(&line));

    let row = Row { root, icon, title, subtitle, meter, badge, btn_pin, btn_cap, btn_pause, btn_close };
    update_row(&row, a, look, mem_total);
    row
}

fn update_row(row: &Row, a: &App, look: &Look, mem_total: u64) {
    match &look.icon {
        Icon::File(p) => row.icon.set_from_file(Some(p)),
        Icon::Named(n) => {
            let has = gtk::gdk::Display::default()
                .map(|d| gtk::IconTheme::for_display(&d).has_icon(n))
                .unwrap_or(false);
            row.icon.set_icon_name(Some(if has { n.as_str() } else { "application-x-executable-symbolic" }));
        }
    }
    row.title.set_text(&look.title);

    let mut sub = vec![human_bytes(a.mem)];
    if a.swap > 0 {
        sub.push(format!("{} swapped", human_bytes(a.swap)));
    }
    if let Some(k) = &look.kind {
        sub.push(k.clone());
    }
    row.subtitle.set_text(&sub.join(" \u{b7} "));

    // A badge only when something is out of the ordinary.
    let (badge, cls, help) = if a.frozen {
        ("Paused", "paused", "Paused: frozen and not running. Resume wakes it exactly where it was.")
    } else if a.protected {
        ("Pinned", "pinned", "Pinned: kept in fast memory so it stays quick.")
    } else if !a.freezable {
        ("System", "system", "A protected system app: rtux won't slow, pause or close it.")
    } else if a.hog {
        ("Biggest", "top", "The biggest app rtux could pause: the first it would pause under pressure.")
    } else {
        ("", "", "")
    };
    row.badge.set_text(badge);
    row.badge.set_tooltip_text(Some(help));
    row.badge.set_visible(!badge.is_empty());
    for c in ["paused", "pinned", "system", "top"] {
        row.badge.remove_css_class(c);
    }
    if !cls.is_empty() {
        row.badge.add_css_class(cls);
    }
    if a.frozen {
        row.root.add_css_class("frozen");
    } else {
        row.root.remove_css_class("frozen");
    }

    let share = a.mem as f64 / mem_total as f64;
    row.meter.set_fraction(share.clamp(0.0, 1.0));
    set_level(&row.meter, share_level(share));

    // Actions follow the app's state.
    row.btn_pause.set_icon_name(if a.frozen { "media-playback-start-symbolic" } else { "media-playback-pause-symbolic" });
    row.btn_pause.set_tooltip_text(Some(if a.frozen {
        "Resume (P): wake it exactly where it was"
    } else {
        "Pause (P): freeze it; Resume wakes it as it was"
    }));
    row.btn_pin.set_tooltip_text(Some(if a.protected {
        "Unpin (I): stop keeping it in fast memory"
    } else {
        "Pin (I): keep this app in fast memory"
    }));
    if a.protected {
        row.btn_pin.add_css_class("accent");
    } else {
        row.btn_pin.remove_css_class("accent");
    }
    row.btn_pause.set_sensitive(a.freezable || a.frozen);
    row.btn_cap.set_sensitive(a.freezable);
    row.btn_close.set_sensitive(a.freezable);

    let swap_part = if a.swap > 0 { format!(", {} in swap", human_bytes(a.swap)) } else { String::new() };
    row.root.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}{}{}",
        look.title,
        human_bytes(a.mem),
        swap_part,
        if badge.is_empty() { String::new() } else { format!(", {}", badge.to_lowercase()) }
    ))]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_map_to_desktop_ids() {
        assert_eq!(desktop_candidates("app-com.t3tools.T3Code-875826.scope")[0], "com.t3tools.T3Code");
        assert_eq!(desktop_candidates("app-vicinae-figma\\x2dlinux\\x2dnext-1663810.scope")[0], "figma-linux-next");
        assert_eq!(desktop_candidates("app-flatpak-app.zen_browser.zen-4243411638.scope")[0], "app.zen_browser.zen");
        assert_eq!(desktop_candidates("app-gnome-org.gnome.Calculator-123.scope")[0], "org.gnome.Calculator");
        assert_eq!(desktop_candidates("vicinae.service")[0], "vicinae");
        assert_eq!(desktop_candidates("snap.firefox.firefox-0b1c.scope")[0], "firefox_firefox");
    }

    #[test]
    fn sizes_read_like_gnome() {
        assert_eq!(human_bytes(4_724_464_025), "4.4 GB");
        assert_eq!(human_bytes(24_373_000_000), "23 GB");
        assert_eq!(human_bytes(700 * 1024 * 1024), "700 MB");
    }
}
