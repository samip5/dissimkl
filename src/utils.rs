use std::io::{self, Write as IoWrite};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use configparser::ini::Ini;
use serde::Deserialize;
use tracing::{info, warn};

/// All application configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub simkl_client_id: String,
    pub simkl_access_token: Option<String>,
    pub discord_app_id_shows: Option<String>,
    pub discord_app_id_movies: Option<String>,
    pub discord_app_id_anime: Option<String>,
    pub poll_interval_secs: u64,
    pub session_window_hours: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            simkl_client_id: String::new(),
            simkl_access_token: None,
            discord_app_id_shows: None,
            discord_app_id_movies: None,
            discord_app_id_anime: None,
            poll_interval_secs: 15,
            session_window_hours: 24,
        }
    }
}

impl Config {
    /// Returns true if all required fields are filled.
    pub fn is_complete(&self) -> bool {
        !self.simkl_client_id.is_empty() && self.simkl_access_token.is_some()
    }

    /// Return the Discord app_id for the given media kind ("shows", "movies", "anime").
    pub fn discord_app_id(&self, kind: &str) -> Option<&str> {
        match kind {
            "shows" => self.discord_app_id_shows.as_deref(),
            "movies" => self.discord_app_id_movies.as_deref(),
            "anime" => self.discord_app_id_anime.as_deref(),
            _ => None,
        }
    }
}

/// Returns the platform-specific config directory for dissimkl.
pub fn get_config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("Cannot find config directory"))?;
    Ok(base.join("dissimkl"))
}

/// Returns the path to credentials.ini.
pub fn credentials_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("credentials.ini"))
}

/// Loads configuration from credentials.ini.
/// Returns `Err` if the file does not exist or is malformed.
/// Missing optional keys are left as `None`.
pub fn load_config() -> Result<Config> {
    let path = credentials_path()?;
    if !path.exists() {
        bail!("Config file not found at {}", path.display());
    }

    let mut ini = Ini::new();
    ini.load(path.to_str().unwrap())
        .map_err(|e| anyhow!("Failed to parse credentials.ini: {}", e))?;

    let get = |section: &str, key: &str| -> Option<String> {
        ini.get(section, key)
            .filter(|v| !v.is_empty())
    };

    let client_id = get("simkl", "client_id")
        .ok_or_else(|| anyhow!("Missing [simkl] client_id in credentials.ini"))?;

    let poll_interval_secs = get("settings", "poll_interval_secs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(15u64);

    let session_window_hours = get("settings", "session_window_hours")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4u64);

    Ok(Config {
        simkl_client_id: client_id,
        simkl_access_token: get("simkl", "access_token"),
        discord_app_id_shows: get("discord", "app_id_shows"),
        discord_app_id_movies: get("discord", "app_id_movies"),
        discord_app_id_anime: get("discord", "app_id_anime"),
        poll_interval_secs,
        session_window_hours,
    })
}

/// Saves (or updates) credentials.ini with the provided config.
pub fn save_config(config: &Config) -> Result<()> {
    let dir = get_config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("credentials.ini");

    let access_token = config.simkl_access_token.as_deref().unwrap_or("");
    let app_id_shows = config.discord_app_id_shows.as_deref().unwrap_or("");
    let app_id_movies = config.discord_app_id_movies.as_deref().unwrap_or("");
    let app_id_anime = config.discord_app_id_anime.as_deref().unwrap_or("");

    let content = format!(
        "[simkl]\n\
         client_id = {client_id}\n\
         access_token = {access_token}\n\
         \n\
         [discord]\n\
         app_id_shows = {app_id_shows}\n\
         app_id_movies = {app_id_movies}\n\
         app_id_anime = {app_id_anime}\n\
         \n\
         [settings]\n\
         poll_interval_secs = {poll_interval}\n\
         session_window_hours = {window}\n",
        client_id = config.simkl_client_id,
        poll_interval = config.poll_interval_secs,
        window = config.session_window_hours,
    );

    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    info!("Config saved to {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Simkl PIN auth responses
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PinResponse {
    #[allow(dead_code)]
    device_code: String,
    user_code: String,
    verification_url: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct PinPollResponse {
    result: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// Runs the Simkl PIN authorization flow interactively on the terminal.
/// Blocks until the user authorizes or the code expires.
/// Returns the `access_token` on success.
pub fn run_pin_auth(client_id: &str) -> Result<String> {
    let pin_url = format!(
        "https://api.simkl.com/oauth/pin?client_id={}&redirect=urn:ietf:wg:oauth:2.0:oob",
        client_id
    );

    let agent = ureq::agent();
    let resp: PinResponse = agent
        .get(&pin_url)
        .call()
        .context("Failed to request PIN from Simkl")?
        .into_json()
        .context("Failed to parse PIN response")?;

    println!();
    println!("=== Simkl Authorization ===");
    println!("Your PIN code: {}", resp.user_code);
    println!("Visit:         {}", resp.verification_url);
    println!("(Opening browser…)");
    println!();

    // Try to open the browser — ignore errors (headless env).
    if let Err(e) = open::that(&resp.verification_url) {
        warn!("Could not open browser automatically: {}", e);
        println!("Please open the URL above manually.");
    }

    print!("Waiting for authorization");
    io::stdout().flush().ok();

    let poll_url = format!(
        "https://api.simkl.com/oauth/pin/{}?client_id={}",
        resp.user_code, client_id
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(resp.expires_in);
    let mut interval = Duration::from_secs(resp.interval.max(5));

    loop {
        thread::sleep(interval);
        print!(".");
        io::stdout().flush().ok();

        if std::time::Instant::now() > deadline {
            println!();
            bail!("PIN expired. Please restart the application and try again.");
        }

        let poll_resp = match agent.get(&poll_url).call() {
            Ok(r) => r,
            Err(e) => {
                warn!("Poll request failed: {}", e);
                continue;
            }
        };

        let parsed: PinPollResponse = match poll_resp.into_json() {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to parse poll response: {}", e);
                continue;
            }
        };

        match parsed.result.as_str() {
            "OK" => {
                println!();
                println!("Authorization successful!");
                return parsed
                    .access_token
                    .ok_or_else(|| anyhow!("Server returned OK but no access_token"));
            }
            "KO" => {
                let msg = parsed.message.as_deref().unwrap_or("");
                if msg.contains("Slow down") {
                    interval += Duration::from_secs(5);
                    warn!("Rate-limited during PIN poll, backing off to {:?}", interval);
                }
                // "Authorization pending" — keep polling
            }
            other => {
                warn!("Unknown poll result: {}", other);
            }
        }
    }
}

/// Interactive first-run setup: prompts for client_id (and optionally Discord app IDs),
/// runs PIN auth, and saves the resulting config.
pub fn interactive_setup() -> Result<Config> {
    println!();
    println!("=== dissimkl — First-run Setup ===");
    println!("You need a Simkl API client_id. Get one at: https://simkl.com/settings/developer/");
    println!();

    let client_id = prompt("Enter your Simkl client_id: ")?;
    if client_id.is_empty() {
        bail!("client_id cannot be empty");
    }

    println!();
    println!(
        "For Discord Rich Presence you need separate Discord application IDs for shows, movies, and anime."
    );
    println!("You can leave these blank and add them to credentials.ini later.");
    println!();

    let app_id_shows = prompt_optional("Discord app_id for TV shows (or leave blank): ")?;
    let app_id_movies = prompt_optional("Discord app_id for movies (or leave blank): ")?;
    let app_id_anime = prompt_optional("Discord app_id for anime (or leave blank): ")?;

    println!();
    let access_token = run_pin_auth(&client_id)?;

    let config = Config {
        simkl_client_id: client_id,
        simkl_access_token: Some(access_token),
        discord_app_id_shows: app_id_shows,
        discord_app_id_movies: app_id_movies,
        discord_app_id_anime: app_id_anime,
        poll_interval_secs: 15,
        session_window_hours: 4,
    };

    save_config(&config)?;
    println!(
        "Config saved to {}",
        credentials_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    Ok(config)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn prompt(label: &str) -> Result<String> {
    print!("{}", label);
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn prompt_optional(label: &str) -> Result<Option<String>> {
    let s = prompt(label)?;
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(s))
    }
}
