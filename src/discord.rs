use anyhow::{Context, Result};
use discord_rich_presence::{
    activity::{Activity, ActivityType, Assets, Button, Timestamps},
    DiscordIpc, DiscordIpcClient,
};
use tracing::{debug, info, warn};

use crate::state::{MediaType, PlaybackSession};

/// Manages a single Discord IPC connection and Rich Presence state.
pub struct DiscordState {
    /// The Discord application ID currently connected (if any).
    current_app_id: Option<String>,
    /// The IPC client (present when connected).
    client: Option<DiscordIpcClient>,
}

impl DiscordState {
    pub fn new() -> Self {
        Self {
            current_app_id: None,
            client: None,
        }
    }

    /// Returns true if there is an active, connected client.
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    // -----------------------------------------------------------------------
    // Connection management
    // -----------------------------------------------------------------------

    /// Connect (or reconnect) to Discord using the given app_id.
    /// If already connected with the same id, this is a no-op.
    fn ensure_connected(&mut self, app_id: &str) -> Result<()> {
        // Already connected with the right app_id — nothing to do.
        if self.client.is_some() && self.current_app_id.as_deref() == Some(app_id) {
            return Ok(());
        }

        // Disconnect from a different app_id first.
        if self.client.is_some() {
            self.disconnect_inner();
        }

        info!(app_id, "Connecting to Discord IPC");
        // DiscordIpcClient::new() returns Self (not a Result).
        let mut client = DiscordIpcClient::new(app_id);
        client
            .connect()
            .map_err(|e| anyhow::anyhow!("Failed to connect to Discord IPC: {}", e))
            .context("ensure_connected")?;

        self.client = Some(client);
        self.current_app_id = Some(app_id.to_string());
        info!(app_id, "Connected to Discord IPC");
        Ok(())
    }

    fn disconnect_inner(&mut self) {
        if let Some(mut client) = self.client.take() {
            let _ = client.close();
        }
        self.current_app_id = None;
    }

    /// Disconnect from Discord, suppressing errors.
    pub fn disconnect(&mut self) {
        if self.client.is_some() {
            debug!("Disconnecting from Discord IPC");
            self.disconnect_inner();
        }
    }

    // -----------------------------------------------------------------------
    // Rich Presence helpers
    // -----------------------------------------------------------------------

    /// Set the Rich Presence for an active playback session.
    ///
    /// * `session`    — current playback session
    /// * `poster_url` — resolved poster URL (or `None` to use the app's default art)
    /// * `app_id`     — Discord application ID to use
    pub fn set_watching(
        &mut self,
        session: &PlaybackSession,
        poster_url: Option<&str>,
        app_id: &str,
    ) -> Result<()> {
        self.ensure_connected(app_id)
            .context("Connecting to Discord before set_watching")?;

        // Build strings we need as owned values first so they outlive the activity builder.
        let details = build_details(session);
        let state_str = build_state(session);
        let large_image: String = poster_url.unwrap_or("simkl").to_string();
        let large_text: String = session.title.clone();

        // Compute Discord timestamps.
        // • Live session: use the actual start time from the scrobble bar, plus
        //   an end time so Discord shows a countdown bar.
        // • Paused session: fake the start by subtracting elapsed watch-time from
        //   the paused_at timestamp.
        let timestamps = if let Some(live_start) = session.live_start {
            let mut ts = Timestamps::new().start(live_start);
            if let Some(runtime) = session.runtime_mins {
                let end = live_start + runtime as i64 * 60;
                ts = ts.end(end);
            }
            ts
        } else {
            let runtime_secs = session.estimated_runtime_secs();
            let watched_secs = (session.progress / 100.0 * runtime_secs as f64) as i64;
            let start_ts = session.paused_at.timestamp() - watched_secs;
            Timestamps::new().start(start_ts)
        };

        // Build up to 2 buttons (owned Strings coerce into Cow<str>).
        let mut button_data: Vec<(String, String)> = Vec::new();
        if let Some(url) = session.simkl_url() {
            button_data.push(("View on Simkl".to_string(), url));
        }
        match session.media_type {
            MediaType::Anime | MediaType::AnimeMovie => {
                if let Some(url) = session.mal_url() {
                    button_data.push(("View on MyAnimeList".to_string(), url));
                } else if let Some(url) = session.imdb_url() {
                    button_data.push(("View on IMDb".to_string(), url));
                }
            }
            _ => {
                if let Some(url) = session.imdb_url() {
                    button_data.push(("View on IMDb".to_string(), url));
                }
            }
        }
        // Limit to 2 buttons (Discord cap).
        button_data.truncate(2);

        // Build buttons from the owned data.
        let buttons: Vec<Button<'_>> = button_data
            .iter()
            .map(|(label, url)| Button::new(label.as_str(), url.as_str()))
            .collect();

        let assets = Assets::new()
            .large_image(large_image.as_str())
            .large_text(large_text.as_str())
            .small_image("simkl")
            .small_text("Simkl");

        let mut activity = Activity::new()
            .activity_type(ActivityType::Watching)
            .details(details.as_str())
            .state(state_str.as_str())
            .assets(assets)
            .timestamps(timestamps);

        if !buttons.is_empty() {
            activity = activity.buttons(buttons);
        }

        let client = self.client.as_mut().unwrap();
        match client.set_activity(activity) {
            Ok(_) => {
                debug!(
                    "Discord Rich Presence updated: {} — {}",
                    details, state_str
                );
                Ok(())
            }
            Err(e) => {
                warn!(
                    "Failed to set Discord activity, will reconnect next poll: {}",
                    e
                );
                // Drop the client so we reconnect next time.
                self.disconnect_inner();
                Err(anyhow::anyhow!("set_activity failed: {}", e))
            }
        }
    }

    /// Clear the Rich Presence (nothing is playing).
    pub fn clear(&mut self) -> Result<()> {
        if let Some(client) = self.client.as_mut() {
            match client.clear_activity() {
                Ok(_) => {
                    debug!("Discord Rich Presence cleared");
                    Ok(())
                }
                Err(e) => {
                    warn!("Failed to clear Discord activity: {}", e);
                    self.disconnect_inner();
                    Err(anyhow::anyhow!("clear_activity failed: {}", e))
                }
            }
        } else {
            // Not connected — nothing to clear.
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Activity string construction
// ---------------------------------------------------------------------------

/// Returns the `details` line: media category label + title (+ year if known).
///
/// Discord shows this as the bold first line under the app name.
/// e.g. "TV Show • Star Trek: The Next Generation (1987)"
fn build_details(session: &PlaybackSession) -> String {
    let category = match session.media_type {
        MediaType::Episode   => "TV Show",
        MediaType::Movie     => "Movie",
        MediaType::Anime     => "Anime",
        MediaType::AnimeMovie => "Anime Movie",
    };
    match session.year {
        Some(y) => format!("{} \u{2022} {} ({})", category, session.title, y),
        None    => format!("{} \u{2022} {}", category, session.title),
    }
}

/// Returns the `state` line: episode code + title (or progress for movies).
///
/// Discord shows this as the smaller second line.
/// e.g. "S01E16 • Too Short a Season"
fn build_state(session: &PlaybackSession) -> String {
    match &session.media_type {
        MediaType::Episode | MediaType::Anime => {
            build_episode_state(session.season, session.episode, session.episode_title.as_deref())
        }
        MediaType::Movie | MediaType::AnimeMovie => {
            if session.is_live() {
                "Watching now".to_string()
            } else {
                format!("{:.0}% watched", session.progress)
            }
        }
    }
}

fn build_episode_state(
    season: Option<u32>,
    episode: Option<u32>,
    ep_title: Option<&str>,
) -> String {
    match (season, episode) {
        (Some(s), Some(e)) => {
            let code = format!("S{:02}E{:02}", s, e);
            match ep_title {
                Some(t) if !t.is_empty() => format!("{} \u{2022} {}", code, t),
                _ => code,
            }
        }
        _ => "Episode".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Determine which Discord app_id key to use for a session.
// ---------------------------------------------------------------------------

/// Returns the config key ("shows" | "movies" | "anime") for a session.
pub fn app_id_key_for(session: &PlaybackSession) -> &'static str {
    match session.media_type {
        MediaType::Episode => "shows",
        MediaType::Movie => "movies",
        MediaType::Anime | MediaType::AnimeMovie => "anime",
    }
}
