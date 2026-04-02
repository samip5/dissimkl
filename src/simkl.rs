use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use lru::LruCache;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::retry::{execute_with_retry, wrap_ureq, ErrorKind, RetryConfig};
use crate::state::{MediaIds, MediaType, PlaybackSession};

// ---------------------------------------------------------------------------
// Flexible number deserialiser
//
// Simkl inconsistently returns some numeric IDs (e.g. `tmdb`, `simkl`) as
// JSON strings in certain endpoints.  This module accepts both forms.
// ---------------------------------------------------------------------------

mod de_flexible_u64 {
    use serde::{de, Deserialize, Deserializer};

    pub fn deserialize<'de, D>(d: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Flex {
            Num(u64),
            Str(String),
        }

        match Option::<Flex>::deserialize(d)? {
            None => Ok(None),
            Some(Flex::Num(n)) => Ok(Some(n)),
            Some(Flex::Str(s)) if s.is_empty() => Ok(None),
            Some(Flex::Str(s)) => s.parse::<u64>().map(Some).map_err(de::Error::custom),
        }
    }
}

// ---------------------------------------------------------------------------
// /sync/activities response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ActivitiesCategory {
    #[serde(default)]
    playback: Option<DateTime<Utc>>,
}

/// Timestamps returned by GET /sync/activities.
/// We only care about the `playback` field per category — it advances whenever
/// a `/scrobble/pause` or `/scrobble/stop` (<80%) event occurs.
///
/// Note: `/scrobble/start` does NOT advance these timestamps.  An active
/// start-session ("NOW WATCHING" in the Simkl web UI) is internal state that
/// has no public GET endpoint in the Simkl API.
#[derive(Debug, Deserialize)]
pub struct ActivitiesResponse {
    #[serde(rename = "tv_shows", default)]
    tv_shows: Option<ActivitiesCategory>,
    #[serde(default)]
    anime: Option<ActivitiesCategory>,
    #[serde(default)]
    movies: Option<ActivitiesCategory>,
}

impl ActivitiesResponse {
    pub fn tv_playback(&self) -> Option<DateTime<Utc>> {
        self.tv_shows.as_ref()?.playback
    }
    pub fn anime_playback(&self) -> Option<DateTime<Utc>> {
        self.anime.as_ref()?.playback
    }
    pub fn movie_playback(&self) -> Option<DateTime<Utc>> {
        self.movies.as_ref()?.playback
    }
}

// ---------------------------------------------------------------------------
// Raw /sync/playback entries
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawIds {
    // Both `simkl` and `tmdb` may arrive as a JSON string or a JSON number
    // depending on the endpoint — use the flexible deserialiser for both.
    #[serde(default, deserialize_with = "de_flexible_u64::deserialize")]
    simkl: Option<u64>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default, deserialize_with = "de_flexible_u64::deserialize")]
    tmdb: Option<u64>,
    #[serde(default)]
    imdb: Option<String>,
    #[serde(default)]
    mal: Option<String>,
}

impl From<RawIds> for MediaIds {
    fn from(r: RawIds) -> Self {
        MediaIds {
            simkl: r.simkl,
            slug: r.slug,
            tmdb: r.tmdb,
            imdb: r.imdb,
            mal: r.mal,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawShow {
    title: String,
    #[serde(default)]
    year: Option<u32>,
    ids: RawIds,
}

#[derive(Debug, Deserialize)]
struct RawMovie {
    title: String,
    #[serde(default)]
    year: Option<u32>,
    ids: RawIds,
}

#[derive(Debug, Deserialize)]
struct RawAnime {
    title: String,
    #[serde(default)]
    year: Option<u32>,
    #[serde(default)]
    anime_type: Option<String>,
    ids: RawIds,
}

/// Episode info inside a /sync/playback entry.
///
/// The API docs example showed the key as `"episode"`, but real responses use
/// `"number"`.  We accept both so we are robust against either form.
#[derive(Debug, Deserialize)]
struct RawEpisode {
    #[serde(default)]
    season: Option<u32>,
    /// Actual field name in live responses.
    #[serde(default)]
    number: Option<u32>,
    /// Documented field name — present in some response examples; kept as
    /// a fallback in case some endpoints still use it.
    #[serde(default, rename = "episode")]
    episode_fallback: Option<u32>,
    #[serde(default)]
    title: Option<String>,
}

impl RawEpisode {
    /// Returns whichever episode-number field is populated, preferring `number`.
    fn episode_number(&self) -> Option<u32> {
        self.number.or(self.episode_fallback)
    }
}

#[derive(Debug, Deserialize)]
struct RawPlaybackEntry {
    id: u64,
    progress: f64,
    paused_at: DateTime<Utc>,
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    show: Option<RawShow>,
    #[serde(default)]
    movie: Option<RawMovie>,
    #[serde(default)]
    anime: Option<RawAnime>,
    #[serde(default)]
    episode: Option<RawEpisode>,
}

impl RawPlaybackEntry {
    /// Convert to the canonical `PlaybackSession`.
    fn into_session(self) -> Option<PlaybackSession> {
        match self.entry_type.as_str() {
            "episode" => {
                // Could be a TV show or anime episode.
                if let Some(show) = self.show {
                    let ep = self.episode.as_ref();
                    Some(PlaybackSession {
                        id: self.id,
                        progress: self.progress,
                        paused_at: self.paused_at,
                        media_type: MediaType::Episode,
                        title: show.title,
                        year: show.year,
                        ids: show.ids.into(),
                        season: ep.and_then(|e| e.season),
                        episode: ep.and_then(|e| e.episode_number()),
                        episode_title: ep.and_then(|e| e.title.clone()),
                        anime_type: None,
                        live_start: None,
                        runtime_mins: None,
                    })
                } else if let Some(anime) = self.anime {
                    let ep = self.episode.as_ref();
                    let is_movie = anime
                        .anime_type
                        .as_deref()
                        .map(|t| t == "movie")
                        .unwrap_or(false);
                    Some(PlaybackSession {
                        id: self.id,
                        progress: self.progress,
                        paused_at: self.paused_at,
                        media_type: if is_movie {
                            MediaType::AnimeMovie
                        } else {
                            MediaType::Anime
                        },
                        title: anime.title,
                        year: anime.year,
                        ids: anime.ids.into(),
                        season: ep.and_then(|e| e.season),
                        episode: ep.and_then(|e| e.episode_number()),
                        episode_title: ep.and_then(|e| e.title.clone()),
                        anime_type: anime.anime_type,
                        live_start: None,
                        runtime_mins: None,
                    })
                } else {
                    warn!("Episode entry has neither show nor anime: id={}", self.id);
                    None
                }
            }
            "movie" => {
                let movie = self.movie?;
                Some(PlaybackSession {
                    id: self.id,
                    progress: self.progress,
                    paused_at: self.paused_at,
                    media_type: MediaType::Movie,
                    title: movie.title,
                    year: movie.year,
                    ids: movie.ids.into(),
                    season: None,
                    episode: None,
                    episode_title: None,
                    anime_type: None,
                    live_start: None,
                    runtime_mins: None,
                })
            }
            other => {
                warn!("Unknown playback entry type: {}", other);
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Extended info response (for poster hash)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ExtendedInfoResponse {
    #[serde(default)]
    poster: Option<String>,
}

// ---------------------------------------------------------------------------
// Simkl API client
// ---------------------------------------------------------------------------

/// Number of poster URLs to cache in memory.
const POSTER_CACHE_SIZE: usize = 128;

pub struct SimklClient {
    pub client_id: String,
    pub access_token: String,
    pub base_url: String,
    agent: ureq::Agent,
    poster_cache: LruCache<u64, String>,
    retry_config: RetryConfig,
    /// Numeric Simkl user ID — fetched lazily from /users/settings.
    /// Used to construct the dashboard URL for NOW WATCHING scraping.
    user_id: Option<u64>,
}

impl SimklClient {
    pub fn new(client_id: String, access_token: String) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(20))
            .build();

        Self {
            client_id,
            access_token,
            base_url: "https://api.simkl.com".to_string(),
            agent,
            poster_cache: LruCache::new(
                std::num::NonZeroUsize::new(POSTER_CACHE_SIZE).unwrap(),
            ),
            retry_config: RetryConfig::default(),
            user_id: None,
        }
    }

    // -----------------------------------------------------------------------
    // Auth headers helper
    // -----------------------------------------------------------------------

    fn get_request(&self, path: &str) -> ureq::Request {
        self.agent
            .get(&format!("{}{}", self.base_url, path))
            .set(
                "Authorization",
                &format!("Bearer {}", self.access_token),
            )
            .set("simkl-api-key", &self.client_id)
            .set("Content-Type", "application/json")
    }

    // -----------------------------------------------------------------------
    // GET /sync/playback
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // GET /sync/activities
    // -----------------------------------------------------------------------

    pub fn get_activities(&self) -> Result<ActivitiesResponse> {
        debug!("GET /sync/activities");
        execute_with_retry(&self.retry_config, |attempt| {
            debug!(attempt, "fetching /sync/activities");
            let resp = wrap_ureq(|| self.get_request("/sync/activities").call())?;
            resp.into_json::<ActivitiesResponse>().map_err(|e| {
                (ErrorKind::Fatal, anyhow!("Failed to parse activities: {}", e))
            })
        })
        .context("get_activities failed")
    }

    // -----------------------------------------------------------------------
    // GET /sync/playback
    // -----------------------------------------------------------------------

    pub fn get_playback(&self) -> Result<Vec<PlaybackSession>> {
        debug!("Fetching /sync/playback");
        let raw: Vec<RawPlaybackEntry> =
            execute_with_retry(&self.retry_config, |attempt| {
                debug!(attempt, "GET /sync/playback");
                let resp =
                    wrap_ureq(|| self.get_request("/sync/playback").call())?;

                // Simkl returns `null` (not `[]`) when there are no sessions.
                // Deserialise into a Value first so we can handle that case —
                // and so we can log the raw payload at TRACE level to answer
                // the open question: does GET /sync/playback return active
                // `start` sessions or only `pause`/`stop` ones?
                let value: serde_json::Value =
                    resp.into_json().map_err(|e| {
                        (
                            ErrorKind::Fatal,
                            anyhow!("Failed to read playback body: {}", e),
                        )
                    })?;

                // Run with RUST_LOG=dissimkl=trace to see the raw payload
                // while you have an active "NOW WATCHING" session.  If a
                // start-session appears here, the GET endpoint DOES return
                // active sessions and our activities-gating is the culprit.
                // If it doesn't, the limitation is real and server-side.
                tracing::trace!(
                    raw_playback = %value,
                    "raw /sync/playback response"
                );

                if value.is_null() {
                    debug!("Playback response was null — no active sessions");
                    return Ok(vec![]);
                }

                serde_json::from_value::<Vec<RawPlaybackEntry>>(value)
                    .map_err(|e| {
                        (
                            ErrorKind::Fatal,
                            anyhow!("Failed to deserialise playback entries: {}", e),
                        )
                    })
            })
            .context("get_playback failed")?;

        let sessions: Vec<PlaybackSession> =
            raw.into_iter().filter_map(|r| r.into_session()).collect();
        debug!("Got {} playback session(s)", sessions.len());
        Ok(sessions)
    }

    // -----------------------------------------------------------------------
    // GET /{type}/{simkl_id}?extended=full — for poster hash
    // -----------------------------------------------------------------------

    pub fn get_poster_url(
        &mut self,
        simkl_id: u64,
        media_type: &MediaType,
    ) -> Option<String> {
        // Return from cache if available.
        if let Some(url) = self.poster_cache.get(&simkl_id) {
            return Some(url.clone());
        }

        let segment = match media_type {
            MediaType::Episode => "tv",
            MediaType::Movie => "movies",
            MediaType::Anime | MediaType::AnimeMovie => "anime",
        };
        let path = format!("/{}?id={}&extended=full", segment, simkl_id);

        let result = execute_with_retry(&self.retry_config, |attempt| {
            debug!(attempt, "GET /{} for poster (id={})", segment, simkl_id);
            let resp = wrap_ureq(|| {
                self.agent
                    .get(&format!("{}/{}/{}", self.base_url, segment, simkl_id))
                    .query("extended", "full")
                    .set("simkl-api-key", &self.client_id)
                    .call()
            })?;
            resp.into_json::<ExtendedInfoResponse>().map_err(|e| {
                (
                    ErrorKind::Fatal,
                    anyhow!("Failed to parse extended info: {}", e),
                )
            })
        });

        match result {
            Ok(info) => {
                if let Some(hash) = info.poster {
                    if hash.len() >= 2 {
                        let prefix = &hash[..2];
                        let url = format!(
                            "https://wsrv.nl/?url=https://simkl.in/posters/{}/{}_m.webp",
                            prefix, hash
                        );
                        self.poster_cache.put(simkl_id, url.clone());
                        return Some(url);
                    }
                }
                None
            }
            Err(e) => {
                warn!("Could not fetch poster for simkl_id={}: {}", simkl_id, e);
                let _ = path; // silence unused warning
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helper: is a session within the configured window?
    // -----------------------------------------------------------------------

    pub fn should_show_session(
        session: &PlaybackSession,
        window_hours: u64,
    ) -> bool {
        // Live sessions are always within window.
        if session.is_live() {
            return true;
        }
        let age = Utc::now().signed_duration_since(session.paused_at);
        age.num_hours() < window_hours as i64 && age.num_seconds() >= 0
    }

    // -----------------------------------------------------------------------
    // GET /users/settings — obtain numeric user ID for dashboard scraping
    // -----------------------------------------------------------------------

    fn fetch_user_id(&self) -> Result<u64> {
        debug!("GET /users/settings");

        let value: serde_json::Value = execute_with_retry(&self.retry_config, |attempt| {
            debug!(attempt, "fetching /users/settings");
            let r = wrap_ureq(|| self.get_request("/users/settings").call())?;
            r.into_json().map_err(|e| {
                (ErrorKind::Fatal, anyhow!("Failed to parse user settings: {}", e))
            })
        }).context("fetch_user_id failed")?;

        tracing::debug!(raw_user_settings = %value, "raw /users/settings response");

        // Try several known response shapes — Simkl has changed this over time.
        //   Shape A: { "user": { "ids": { "simkl": 12345 } } }
        //   Shape B: { "account": { "id": 12345 } }
        //   Shape C: { "user": { "simkl": 12345 } }
        let id =
            value["user"]["ids"]["simkl"].as_u64()
            .or_else(|| value["account"]["id"].as_u64())
            .or_else(|| value["user"]["simkl"].as_u64())
            .or_else(|| value["user"]["id"].as_u64());

        id.ok_or_else(|| anyhow!(
            "Cannot find user ID in /users/settings response. \
             Full response logged at TRACE level. \
             Run with RUST_LOG=dissimkl=trace to inspect it."
        ))
    }

    /// Fetch (and cache) the numeric Simkl user ID.
    /// Returns `None` and logs a warning if the fetch fails.
    pub fn ensure_user_id(&mut self) -> Option<u64> {
        if let Some(id) = self.user_id {
            return Some(id);
        }
        match self.fetch_user_id() {
            Ok(id) => {
                info!("Simkl user ID: {}", id);
                self.user_id = Some(id);
                Some(id)
            }
            Err(e) => {
                warn!("Could not fetch Simkl user ID: {:#}", e);
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // NOW WATCHING — scrape the Simkl dashboard for an active start session
    //
    // The public API only exposes paused/stopped sessions via /sync/playback.
    // Active start sessions ("NOW WATCHING") live exclusively in the web UI.
    // We detect them by scraping the dashboard page and reading the scrobble
    // bar data attributes.
    // -----------------------------------------------------------------------

    /// Fetch the Simkl dashboard and extract an active "NOW WATCHING" session,
    /// if any.  Returns `None` if:
    ///   • the user ID cannot be determined
    ///   • the dashboard is unreachable
    ///   • the scrobble bar element is absent (nothing playing)
    ///   • the HTML cannot be parsed
    pub fn get_now_watching(&mut self) -> Option<PlaybackSession> {
        let user_id = self.ensure_user_id()?;
        let url = format!("https://simkl.com/{}/dashboard/", user_id);
        debug!("Scraping dashboard for NOW WATCHING: {}", url);

        let resp = self.agent
            .get(&url)
            // Try the OAuth Bearer token — Simkl's web server accepts it for
            // authenticated page requests in addition to the standard API.
            .set("Authorization", &format!("Bearer {}", self.access_token))
            // Some web endpoints also accept the token as a cookie.
            .set("Cookie", &format!("simkl_access_token={}", self.access_token))
            .set("Accept", "text/html,application/xhtml+xml")
            .call()
            .map_err(|e| { warn!("Dashboard fetch failed: {}", e); e })
            .ok()?;

        let html = resp.into_string()
            .map_err(|e| { warn!("Dashboard read failed: {}", e); e })
            .ok()?;

        let session = parse_now_watching(&html)?;

        // Pre-populate the poster cache from the dashboard img so that the
        // subsequent get_poster_url() call in main.rs is a cache hit and
        // requires no extra API round-trip.
        if let Some(simkl_id) = session.ids.simkl {
            if self.poster_cache.get(&simkl_id).is_none() {
                if let Some(url) = extract_live_poster_url(&html) {
                    debug!("Pre-caching live poster for simkl_id={}", simkl_id);
                    self.poster_cache.put(simkl_id, url);
                }
            }
        }

        Some(session)
    }
}

// ---------------------------------------------------------------------------
// Dashboard HTML parsing
// TODO: This is not a great way to do this, but other ways don't exist.
// ---------------------------------------------------------------------------

/// Parse the Simkl dashboard HTML and return the active session, if present.
fn parse_now_watching(html: &str) -> Option<PlaybackSession> {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);

    // Find the scrobble bar element.
    let bar_sel = Selector::parse("#simklScrobbleBar").ok()?;
    let bar = doc.select(&bar_sel).next()?;

    let attr = |name: &str| bar.value().attr(name);

    let started_at: i64 = attr("data-started")?.parse().ok()?;
    let runtime_mins: u64 = attr("data-runtime")?.parse().ok()?;
    let progress_at_start: f64 = attr("data-progress-at-start")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);

    // Compute current progress.
    let elapsed_secs = Utc::now().timestamp().saturating_sub(started_at).max(0);
    let elapsed_mins = elapsed_secs as f64 / 60.0;
    let progress = if runtime_mins > 0 {
        (progress_at_start + (elapsed_mins / runtime_mins as f64) * 100.0).min(100.0)
    } else {
        progress_at_start
    };

    // Find the media link inside the bar.
    // Expected href patterns:
    //   /tv/1029/star-trek-the-next-generation/season-1/episode-16/
    //   /movies/12345/inception/
    //   /anime/6789/attack-on-titan/season-1/episode-5/
    let link_sel = Selector::parse("a[href]").ok()?;
    let mut media_kind = String::new();
    let mut simkl_id: Option<u64> = None;
    let mut slug: Option<String> = None;
    let mut season: Option<u32> = None;
    let mut episode: Option<u32> = None;
    let mut link_title = String::new();

    for a in bar.select(&link_sel) {
        let href = a.value().attr("href").unwrap_or("");
        if let Some((kind, id, sl, s, e)) = parse_media_href(href) {
            media_kind = kind;
            simkl_id = Some(id);
            slug = Some(sl);
            season = s;
            episode = e;
            let text: String = a.text().collect::<Vec<_>>().join(" ");
            let text = text.trim().to_string();
            if !text.is_empty() {
                link_title = text;
            }
            break;
        }
    }

    if media_kind.is_empty() {
        debug!("NOW WATCHING: scrobble bar found but no media link — possibly logged out");
        return None;
    }

    // Try to get a better title from dedicated title elements before falling
    // back to the link text.
    let title = find_text(&bar, &[
        ".SimklScrobbleTitle",
        ".simkl-scrobble-title",
        "[class*='ScrobbleTitle']",
        "[class*='scrobble-title']",
    ]).unwrap_or(link_title);

    // Episode title from a subtitle element.
    let episode_title = find_text(&bar, &[
        ".SimklScrobbleEpTitle",
        ".SimklScrobbleEpInfo",
        ".simkl-scrobble-ep-title",
        "[class*='EpTitle']",
        "[class*='ep-title']",
    ]);

    // (Poster URL is extracted separately via extract_live_poster_url so that
    //  get_now_watching can pre-populate the poster cache from &mut self.)

    let media_type = match media_kind.as_str() {
        "tv" => MediaType::Episode,
        "movies" => MediaType::Movie,
        _ => MediaType::Anime,  // "anime"
    };

    let ids = MediaIds {
        simkl: simkl_id,
        slug,
        ..Default::default()
    };

    // Cache the poster URL if we have both a simkl_id and a URL.
    // (We can't call self.poster_cache here since we're outside the impl —
    //  the caller may add it separately.)

    info!(
        "NOW WATCHING: {} ({:?}) — started {}s ago, {}min runtime, progress {:.1}%",
        title, media_type, elapsed_secs, runtime_mins, progress
    );

    Some(PlaybackSession {
        id: 0, // no API id for live sessions
        progress,
        paused_at: Utc::now(), // "just now" — keeps it within any session window
        media_type,
        title,
        year: None,
        ids,
        season,
        episode,
        episode_title: episode_title.as_deref().map(clean_episode_title),
        anime_type: None,
        live_start: Some(started_at),
        runtime_mins: Some(runtime_mins),
    })
}

// ---------------------------------------------------------------------------
// NOW WATCHING poster URL extractor (public so main.rs can use it)
// ---------------------------------------------------------------------------

/// Extract and convert the wsrv.nl poster URL from the dashboard HTML.
/// Returns `None` if the scrobble bar or poster img is absent.
pub fn extract_live_poster_url(html: &str) -> Option<String> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let bar_sel = Selector::parse("#simklScrobbleBar").ok()?;
    let bar = doc.select(&bar_sel).next()?;
    let img_sel = Selector::parse("img").ok()?;
    bar.select(&img_sel)
        .next()
        .and_then(|img| img.value().attr("src").or_else(|| img.value().attr("data-src")))
        .and_then(simkl_poster_to_wsrv)
}

// ---------------------------------------------------------------------------
// Internal HTML parsing helpers
// ---------------------------------------------------------------------------

/// Strip the leading episode code + label that Simkl embeds in its episode
/// title elements, keeping only the bare title after the last colon.
///
/// e.g. `"S01E16   Season 1, Episode 16:  Too Short a Season"` → `"Too Short a Season"`
/// e.g. `"Too Short a Season"` → `"Too Short a Season"` (unchanged)
fn clean_episode_title(raw: &str) -> String {
    // If the text contains a colon, everything after the last ": " is the title.
    if let Some(pos) = raw.rfind(':') {
        let after = raw[pos + 1..].trim();
        if !after.is_empty() {
            return after.to_string();
        }
    }
    raw.trim().to_string()
}

/// Try each CSS selector in order; return the trimmed text of the first match.
fn find_text(
    element: &scraper::ElementRef<'_>,
    selectors: &[&str],
) -> Option<String> {
    for sel_str in selectors {
        if let Ok(sel) = scraper::Selector::parse(sel_str) {
            if let Some(el) = element.select(&sel).next() {
                let text: String = el.text().collect::<Vec<_>>().join(" ");
                let text = text.trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Parse an href like `/tv/1029/star-trek-the-next-generation/season-1/episode-16/`
/// into `(kind, simkl_id, slug, season, episode)`.
/// Returns `None` if the href does not match a known media URL pattern.
fn parse_media_href(href: &str) -> Option<(String, u64, String, Option<u32>, Option<u32>)> {
    let trimmed = href.trim_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    let kind = parts[0];
    if !matches!(kind, "tv" | "movies" | "anime") {
        return None;
    }
    let id: u64 = parts[1].parse().ok()?;
    let slug = parts[2].to_string();
    if slug.is_empty() {
        return None;
    }

    let mut season: Option<u32> = None;
    let mut episode: Option<u32> = None;
    for part in &parts[3..] {
        if let Some(s) = part.strip_prefix("season-") {
            season = s.parse().ok();
        } else if let Some(e) = part.strip_prefix("episode-") {
            episode = e.parse().ok();
        }
    }

    Some((kind.to_string(), id, slug, season, episode))
}

/// Convert a `//simkl.in/posters/…` or `https://simkl.in/posters/…` URL into
/// a wsrv.nl-proxied medium-size WebP URL (the same format used by get_poster_url).
/// Returns `None` if the URL doesn't match the expected pattern.
fn simkl_poster_to_wsrv(src: &str) -> Option<String> {
    // Strip protocol prefix so we can handle both // and https:// variants.
    let clean = src
        .trim_start_matches("https:")
        .trim_start_matches("http:")
        .trim_start_matches("//");

    let path = clean.strip_prefix("simkl.in/posters/")?;
    // path = "25/25673dccc82356e_c.webp"
    let slash_pos = path.find('/')?;
    let file = &path[slash_pos + 1..]; // "25673dccc82356e_c.webp"
    let hash = file.split('_').next()?; // "25673dccc82356e"
    if hash.len() < 2 {
        return None;
    }
    let prefix = &hash[..2];
    Some(format!(
        "https://wsrv.nl/?url=https://simkl.in/posters/{}/{}_m.webp",
        prefix, hash
    ))
}
