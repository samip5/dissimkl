mod discord;
mod retry;
mod simkl;
mod state;
mod utils;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

use crate::discord::{app_id_key_for, DiscordState};
use crate::simkl::SimklClient;
use crate::state::{new_shared_state, SharedState};
use crate::utils::{interactive_setup, load_config, Config};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // Initialise tracing (RUST_LOG controls verbosity, defaults to "info").
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("dissimkl=info,warn"));
    fmt().with_env_filter(filter).with_target(false).init();

    info!("dissimkl starting");

    // Load or create config.
    let config = match load_config() {
        Ok(cfg) if cfg.is_complete() => cfg,
        Ok(cfg) => {
            // Config file exists but is missing the access token — run PIN auth.
            warn!("Config is incomplete — running PIN auth");
            let token = utils::run_pin_auth(&cfg.simkl_client_id)?;
            let mut cfg = cfg;
            cfg.simkl_access_token = Some(token);
            utils::save_config(&cfg)?;
            cfg
        }
        Err(_) => {
            info!("No config found — running first-time setup");
            interactive_setup()?
        }
    };

    let state = new_shared_state();
    let quit_flag = Arc::new(AtomicBool::new(false));

    // Spawn the background polling thread.
    let poll_state = Arc::clone(&state);
    let poll_quit = Arc::clone(&quit_flag);
    let poll_config = config.clone();
    let _polling_handle = thread::Builder::new()
        .name("simkl-poll".to_string())
        .spawn(move || polling_loop(poll_config, poll_state, poll_quit))?;

    // Run the system tray (blocks until quit).
    run_tray(config, Arc::clone(&state), Arc::clone(&quit_flag))?;

    info!("Quit requested — shutting down");
    quit_flag.store(true, Ordering::Relaxed);
    Ok(())
}

// ---------------------------------------------------------------------------
// Polling loop (background thread)
// ---------------------------------------------------------------------------

/// Local state tracked across poll cycles — not shared with the tray.
struct PollState {
    /// Last-seen `playback` timestamps from `/sync/activities` (one per category).
    /// These advance only on `/scrobble/pause` or `/scrobble/stop` (<80%).
    /// `/scrobble/start` does NOT advance them; active "NOW WATCHING" sessions
    /// are internal to Simkl and have no public GET endpoint.
    last_tv_playback: Option<chrono::DateTime<chrono::Utc>>,
    last_anime_playback: Option<chrono::DateTime<chrono::Utc>>,
    last_movie_playback: Option<chrono::DateTime<chrono::Utc>>,
    /// When we last fetched `/sync/playback`.
    last_playback_fetch: Option<std::time::Instant>,
    /// Cached sessions from the last `/sync/playback` fetch.
    cached_sessions: Vec<crate::state::PlaybackSession>,
}

impl PollState {
    fn new() -> Self {
        Self {
            last_tv_playback: None,
            last_anime_playback: None,
            last_movie_playback: None,
            last_playback_fetch: None,
            cached_sessions: Vec::new(),
        }
    }
}

/// How often to force-refresh `/sync/playback` even if activities haven't changed.
/// Catches completions (≥80% → session silently removed) and manual deletions,
/// neither of which advances `activities.playback`.
const FORCE_REFRESH_SECS: u64 = 120;

fn polling_loop(config: Config, state: SharedState, quit_flag: Arc<AtomicBool>) {
    let access_token = match &config.simkl_access_token {
        Some(t) => t.clone(),
        None => {
            error!("No access token — polling loop cannot start");
            return;
        }
    };

    let mut simkl =
        SimklClient::new(config.simkl_client_id.clone(), access_token);
    let mut discord = DiscordState::new();
    let mut poll_state = PollState::new();
    let window_hours = config.session_window_hours;
    let interval = Duration::from_secs(config.poll_interval_secs);

    loop {
        if quit_flag.load(Ordering::Relaxed) {
            break;
        }

        // Skip polling if the user has paused the app via the tray.
        let is_paused = state.read().map(|s| s.is_paused).unwrap_or(false);
        if !is_paused {
            if let Err(e) = run_poll_cycle(
                &mut simkl,
                &mut discord,
                &mut poll_state,
                &state,
                window_hours,
                &config,
            ) {
                warn!("Poll cycle error: {:#}", e);
            }
        }

        thread::sleep(interval);
    }

    // Clean up Discord on exit.
    let _ = discord.clear();
    discord.disconnect();
    info!("Polling loop exited");
}

/// One iteration of the poll cycle.
fn run_poll_cycle(
    simkl: &mut SimklClient,
    discord: &mut DiscordState,
    poll_state: &mut PollState,
    state: &SharedState,
    window_hours: u64,
    config: &Config,
) -> Result<()> {
    // Step 0: Check for a live "NOW WATCHING" session via dashboard scraping.
    //
    // The Simkl public API only exposes paused/stopped sessions (<80%).
    // An active scrobble (started but not yet paused) is internal state with no
    // public GET endpoint.  We work around this by scraping the user's dashboard
    // page which renders the scrobble bar with real-time data attributes.
    //
    // If a live session is found, we show it in Discord immediately and skip the
    // activities + /sync/playback flow for this cycle.
    if let Some(live_session) = simkl.get_now_watching() {
        let kind = crate::discord::app_id_key_for(&live_session);
        let app_id = config.discord_app_id(kind).map(|s| s.to_string());

        // Prefer the poster URL from the cached API data; the dashboard img is
        // available as a fallback via get_poster_url which checks the LRU cache.
        let poster_url = live_session
            .ids
            .simkl
            .and_then(|id| simkl.get_poster_url(id, &live_session.media_type));

        {
            let mut s = state.write().unwrap();
            s.current_session = Some(live_session.clone());
            s.discord_connected = discord.is_connected();
        }

        if let Some(app_id) = app_id {
            if let Err(e) = discord.set_watching(&live_session, poster_url.as_deref(), &app_id) {
                warn!("Failed to update Discord presence (live): {:#}", e);
            } else {
                state.write().unwrap().discord_connected = true;
            }
        } else {
            warn!(
                "No Discord app_id configured for kind='{}' — skipping Rich Presence",
                kind
            );
        }
        return Ok(());
    }

    // Step 1: fetch activities — a tiny JSON response, cheap every 15 s.
    // This is the recommended pattern from the Simkl API docs.
    let activities = simkl.get_activities()?;

    let playback_changed =
        activities.tv_playback()    != poll_state.last_tv_playback
        || activities.anime_playback() != poll_state.last_anime_playback
        || activities.movie_playback() != poll_state.last_movie_playback;

    let force_refresh = poll_state
        .last_playback_fetch
        .map(|t| t.elapsed().as_secs() >= FORCE_REFRESH_SECS)
        .unwrap_or(true); // always fetch on first run

    // Step 2: fetch /sync/playback only when needed.
    //
    // Trigger conditions:
    //  • `activities.playback` changed  → a pause/stop (<80%) event happened
    //  • force-refresh timer fired      → catches silent session removals:
    //      - completion (≥80%) removes the session but does NOT update activities.playback
    //      - manual session deletion likewise goes undetected by activities alone
    //
    // What we CANNOT detect: an active `/scrobble/start` session ("NOW WATCHING"
    // in the Simkl web UI).  That internal state has no public GET endpoint.
    // Discord status will update the moment the user pauses their player.
    if playback_changed || force_refresh {
        if playback_changed {
            debug!("activities.playback changed — fetching fresh sessions");
        } else {
            debug!("{}s force-refresh — re-fetching sessions", FORCE_REFRESH_SECS);
        }

        let sessions = simkl.get_playback()?;

        // Log the age of the most recent session to aid window tuning.
        if let Some(newest) = sessions.first() {
            let age = chrono::Utc::now().signed_duration_since(newest.paused_at);
            let mins = age.num_minutes();
            if mins < 60 {
                debug!("Most recent session: '{}' — {}m ago", newest.display_label(), mins);
            } else {
                debug!(
                    "Most recent session: '{}' — {}h {}m ago",
                    newest.display_label(), mins / 60, mins % 60
                );
            }
        }

        poll_state.last_tv_playback    = activities.tv_playback();
        poll_state.last_anime_playback = activities.anime_playback();
        poll_state.last_movie_playback = activities.movie_playback();
        poll_state.last_playback_fetch = Some(std::time::Instant::now());
        poll_state.cached_sessions     = sessions;
    }

    // Step 3: pick the most recent session within the window from the cache.
    let active = poll_state
        .cached_sessions
        .iter()
        .find(|s| SimklClient::should_show_session(s, window_hours))
        .cloned();

    match active {
        Some(session) => {
            info!(
                "Showing: {} ({:?})",
                session.display_label(),
                session.media_type
            );

            let kind = app_id_key_for(&session);
            let app_id = config.discord_app_id(kind).map(|s| s.to_string());

            let poster_url = session
                .ids
                .simkl
                .and_then(|id| simkl.get_poster_url(id, &session.media_type));

            {
                let mut s = state.write().unwrap();
                s.current_session = Some(session.clone());
                s.discord_connected = discord.is_connected();
            }

            if let Some(app_id) = app_id {
                if let Err(e) =
                    discord.set_watching(&session, poster_url.as_deref(), &app_id)
                {
                    warn!("Failed to update Discord presence: {:#}", e);
                } else {
                    state.write().unwrap().discord_connected = true;
                }
            } else {
                warn!(
                    "No Discord app_id configured for kind='{}' — skipping Rich Presence",
                    kind
                );
            }
        }
        None => {
            if poll_state.cached_sessions.is_empty() {
                debug!("No playback sessions — clearing presence");
            } else {
                debug!(
                    "All {} session(s) outside {}h window — clearing presence",
                    poll_state.cached_sessions.len(),
                    window_hours
                );
            }
            {
                let mut s = state.write().unwrap();
                s.current_session = None;
            }
            if let Err(e) = discord.clear() {
                warn!("Failed to clear Discord presence: {:#}", e);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Icon generation — 32x32 RGBA teal circle on transparent background
// ---------------------------------------------------------------------------

/// Generates a 32x32 RGBA image: a teal (#1CC8D2) filled circle.
fn generate_icon_rgba() -> Vec<u8> {
    const SIZE: usize = 32;
    const R: u8 = 28;
    const G: u8 = 200;
    const B: u8 = 210;

    let center = SIZE as f32 / 2.0;
    let radius = (SIZE as f32 / 2.0) - 1.5;

    let mut pixels = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center + 0.5;
            let dy = y as f32 - center + 0.5;
            let dist = (dx * dx + dy * dy).sqrt();

            let idx = (y * SIZE + x) * 4;
            if dist <= radius {
                // Smooth edge using anti-aliasing.
                let alpha = if dist > radius - 1.0 {
                    ((radius - dist) * 255.0) as u8
                } else {
                    255
                };
                pixels[idx] = R;
                pixels[idx + 1] = G;
                pixels[idx + 2] = B;
                pixels[idx + 3] = alpha;
            }
            // else transparent (already 0)
        }
    }
    pixels
}

// ---------------------------------------------------------------------------
// System tray — platform-specific implementations
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod tray_impl {
    use super::*;
    use std::sync::Mutex;

    struct DissimklTray {
        state: SharedState,
        quit_flag: Arc<AtomicBool>,
        /// Mirrors the is_paused field in AppState, managed within ksni's thread.
        is_paused: Arc<Mutex<bool>>,
    }

    impl ksni::Tray for DissimklTray {
        fn icon_name(&self) -> String {
            "media-playback-start".to_string()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let pixels = generate_icon_rgba();
            const SIZE: i32 = 32;
            // ksni expects ARGB32 big-endian.
            let argb: Vec<u8> = pixels
                .chunks_exact(4)
                .flat_map(|rgba| [rgba[3], rgba[0], rgba[1], rgba[2]])
                .collect();
            vec![ksni::Icon {
                width: SIZE,
                height: SIZE,
                data: argb,
            }]
        }

        fn title(&self) -> String {
            self.state
                .read()
                .map(|s| s.status_text())
                .unwrap_or_else(|_| "dissimkl".to_string())
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let paused = *self.is_paused.lock().unwrap();
            vec![
                ksni::MenuItem::Standard(ksni::menu::StandardItem {
                    label: if paused {
                        "Resume".to_string()
                    } else {
                        "Pause".to_string()
                    },
                    activate: Box::new(|this: &mut Self| {
                        let mut p = this.is_paused.lock().unwrap();
                        *p = !*p;
                        if let Ok(mut s) = this.state.write() {
                            s.is_paused = *p;
                        }
                    }),
                    ..Default::default()
                }),
                ksni::MenuItem::Separator,
                ksni::MenuItem::Standard(ksni::menu::StandardItem {
                    label: "Quit".to_string(),
                    activate: Box::new(|this: &mut Self| {
                        this.quit_flag.store(true, Ordering::Relaxed);
                        std::process::exit(0);
                    }),
                    ..Default::default()
                }),
            ]
        }
    }

    pub fn run(
        _config: Config,
        state: SharedState,
        quit_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        let is_paused = Arc::new(Mutex::new(false));
        let tray = DissimklTray {
            state,
            quit_flag: Arc::clone(&quit_flag),
            is_paused,
        };
        let service = ksni::TrayService::new(tray);
        let _handle = service.spawn();
        // Block until quit is requested.
        loop {
            thread::sleep(Duration::from_millis(200));
            if quit_flag.load(Ordering::Relaxed) {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod tray_impl {
    use super::*;

    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
        TrayIcon, TrayIconBuilder,
    };
    use winit::{
        application::ApplicationHandler,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
        window::WindowId,
    };

    // Custom user events sent via the proxy.
    #[derive(Debug, Clone)]
    enum UserEvent {
        Quit,
        TogglePause,
    }

    struct App {
        state: SharedState,
        quit_flag: Arc<AtomicBool>,
        proxy: EventLoopProxy<UserEvent>,
        _tray: TrayIcon,
        pause_item: MenuItem,
    }

    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _window_id: WindowId,
            _event: winit::event::WindowEvent,
        ) {
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            match event {
                UserEvent::Quit => {
                    self.quit_flag.store(true, Ordering::Relaxed);
                    event_loop.exit();
                }
                UserEvent::TogglePause => {
                    let paused = {
                        let mut s = self.state.write().unwrap();
                        s.is_paused = !s.is_paused;
                        s.is_paused
                    };
                    let label = if paused { "Resume" } else { "Pause" };
                    let _ = self.pause_item.set_text(label);
                }
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            // Drain tray menu events.
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                let id_str = ev.id.0.as_str();
                match id_str {
                    "pause" => {
                        let _ = self.proxy.send_event(UserEvent::TogglePause);
                    }
                    "quit" => {
                        let _ = self.proxy.send_event(UserEvent::Quit);
                    }
                    _ => {}
                }
            }

            // Update tooltip with current status.
            let tooltip = self
                .state
                .read()
                .map(|s| s.status_text())
                .unwrap_or_else(|_| "dissimkl".to_string());
            let _ = self._tray.set_tooltip(Some(&tooltip));

            if self.quit_flag.load(Ordering::Relaxed) {
                event_loop.exit();
            }

            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + Duration::from_millis(500),
            ));
        }

        fn new_events(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _cause: winit::event::StartCause,
        ) {
        }
    }

    pub fn run(
        _config: Config,
        state: SharedState,
        quit_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event().build()?;
        let proxy = event_loop.create_proxy();

        // Build the tray icon from our generated RGBA data.
        let rgba = generate_icon_rgba();
        let icon = tray_icon::Icon::from_rgba(rgba, 32, 32)
            .map_err(|e| anyhow::anyhow!("Failed to create tray icon: {}", e))?;

        // Build context menu.
        let pause_item = MenuItem::with_id("pause", "Pause", true, None);
        let quit_item = MenuItem::with_id("quit", "Quit", true, None);
        let sep = PredefinedMenuItem::separator();

        let menu = Menu::new();
        menu.append(&pause_item)
            .map_err(|e| anyhow::anyhow!("Menu error: {}", e))?;
        menu.append(&sep)
            .map_err(|e| anyhow::anyhow!("Menu error: {}", e))?;
        menu.append(&quit_item)
            .map_err(|e| anyhow::anyhow!("Menu error: {}", e))?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("dissimkl — Starting…")
            .with_icon(icon)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build tray icon: {}", e))?;

        let mut app = App {
            state,
            quit_flag,
            proxy,
            _tray: tray,
            pause_item,
        };

        // Run the event loop (blocks until exit() is called).
        event_loop.run_app(&mut app)?;
        Ok(())
    }
}

fn run_tray(
    config: Config,
    state: SharedState,
    quit_flag: Arc<AtomicBool>,
) -> Result<()> {
    tray_impl::run(config, state, quit_flag)
}
