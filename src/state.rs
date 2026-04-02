use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};


/// Media type of playback session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Episode,
    Movie,
    Anime,
    AnimeMovie,
}

/// Identifiers for a show/movie/anime.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaIds {
    pub simkl: Option<u64>,
    pub slug: Option<String>,
    pub tmdb: Option<u64>,
    pub imdb: Option<String>,
    pub mal: Option<String>,
}

/// A single playback session returned by /sync/playback, or a live "NOW WATCHING"
/// session scraped from the Simkl dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackSession {
    pub id: u64,
    pub progress: f64,
    pub paused_at: DateTime<Utc>,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<u32>,
    pub ids: MediaIds,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_title: Option<String>,
    /// For anime: "tv", "movie", etc.
    pub anime_type: Option<String>,
    /// Unix timestamp (seconds) when playback actually started.
    /// `Some(ts)` means this is a live "NOW WATCHING" session.
    /// `None` means it is a paused/stopped session from /sync/playback.
    pub live_start: Option<i64>,
    /// Total runtime in minutes (set for live sessions).
    pub runtime_mins: Option<u64>,
}

impl PlaybackSession {
    /// Returns true if this is a live "NOW WATCHING" session.
    pub fn is_live(&self) -> bool {
        self.live_start.is_some()
    }

    /// Human-readable label for tray tooltip.
    pub fn display_label(&self) -> String {
        let prefix = if self.is_live() { "▶ " } else { "" };
        match self.media_type {
            MediaType::Episode | MediaType::Anime => {
                if let (Some(s), Some(e)) = (self.season, self.episode) {
                    format!("{}{} S{:02}E{:02}", prefix, self.title, s, e)
                } else {
                    format!("{}{}", prefix, self.title)
                }
            }
            MediaType::Movie | MediaType::AnimeMovie => {
                if let Some(y) = self.year {
                    format!("{}{} ({})", prefix, self.title, y)
                } else {
                    format!("{}{}", prefix, self.title)
                }
            }
        }
    }

    /// Simkl page URL.
    ///
    /// For episodes/anime with known season+episode, links directly to that
    /// episode page (`/tv/{id}/{slug}/season-{s}/episode-{e}/`).
    /// Falls back to the show root page otherwise.
    pub fn simkl_url(&self) -> Option<String> {
        let slug = self.ids.slug.as_deref()?;
        let id = self.ids.simkl?;
        let url = match self.media_type {
            MediaType::Episode => {
                match (self.season, self.episode) {
                    (Some(s), Some(e)) => format!(
                        "https://simkl.com/tv/{}/{}/season-{}/episode-{}/",
                        id, slug, s, e
                    ),
                    _ => format!("https://simkl.com/tv/{}/{}/", id, slug),
                }
            }
            MediaType::Movie => format!("https://simkl.com/movies/{}/{}/", id, slug),
            MediaType::Anime => {
                match (self.season, self.episode) {
                    (Some(s), Some(e)) => format!(
                        "https://simkl.com/anime/{}/{}/season-{}/episode-{}/",
                        id, slug, s, e
                    ),
                    _ => format!("https://simkl.com/anime/{}/{}/", id, slug),
                }
            }
            MediaType::AnimeMovie => format!("https://simkl.com/anime/{}/{}/", id, slug),
        };
        Some(url)
    }

    /// IMDb URL.
    pub fn imdb_url(&self) -> Option<String> {
        let imdb = self.ids.imdb.as_deref()?;
        Some(format!("https://www.imdb.com/title/{}/", imdb))
    }

    /// MyAnimeList URL (anime only).
    pub fn mal_url(&self) -> Option<String> {
        let mal = self.ids.mal.as_deref()?;
        Some(format!("https://myanimelist.net/anime/{}", mal))
    }

    /// Estimated runtime in seconds (for timestamp calculation).
    pub fn estimated_runtime_secs(&self) -> i64 {
        match self.media_type {
            MediaType::Episode => 45 * 60,
            MediaType::Anime => 25 * 60,
            MediaType::Movie | MediaType::AnimeMovie => 110 * 60,
        }
    }
}

/// Shared application state, protected by a read-write lock.
#[derive(Debug, Default)]
pub struct AppState {
    pub current_session: Option<PlaybackSession>,
    pub is_paused: bool,
    pub discord_connected: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Text shown as the tray tooltip.
    pub fn status_text(&self) -> String {
        if self.is_paused {
            return "dissimkl — Paused".to_string();
        }
        match &self.current_session {
            Some(session) => format!("dissimkl — {}", session.display_label()),
            None => "dissimkl — Nothing playing".to_string(),
        }
    }
}

/// Thread-safe handle to AppState.
pub type SharedState = Arc<RwLock<AppState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(RwLock::new(AppState::new()))
}
