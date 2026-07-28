use std::{
    collections::{BTreeSet, VecDeque},
    io,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use clap::{ArgAction, Parser, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use reqwest::header;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::RwLock,
    task::JoinHandle,
    time::sleep,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, error::ProtocolError, Error as WsError, Message},
};
use whisper_relay_protocol::{
    AudioCodec, AudioContainer, AudioFormat, ClientHello, ClientMessage, DiarizationPreference,
    ServerMessage, TranscriptEvent, PROTOCOL_VERSION,
};

#[derive(Debug, Parser)]
#[command(version)]
struct CliArgs {
    #[arg(long, env = "WHISPER_RELAY_CONFIG")]
    config: Option<PathBuf>,

    #[arg(long, env = "WHISPER_RELAY_SERVER_URL")]
    server_url: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_OUTPUT")]
    output: Option<PathBuf>,

    #[arg(long, env = "WHISPER_RELAY_RECORDING_OUTPUT")]
    recording_output: Option<PathBuf>,

    #[arg(long, env = "WHISPER_RELAY_EVENTS_OUTPUT")]
    events_output: Option<PathBuf>,

    #[arg(long, env = "WHISPER_RELAY_OIDC_ISSUER")]
    oidc_issuer: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_TOKEN")]
    token: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_TOKEN_CACHE")]
    token_cache: Option<PathBuf>,

    #[arg(
        long,
        env = "WHISPER_RELAY_DISABLE_TOKEN_CACHE",
        action = ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    disable_token_cache: Option<bool>,

    #[arg(
        long,
        env = "WHISPER_RELAY_INSECURE_NO_AUTH",
        action = ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    insecure_no_auth: Option<bool>,

    #[arg(long, value_enum)]
    diarization: Option<DiarizationArg>,

    #[arg(long)]
    audio_file: Option<PathBuf>,

    #[arg(long, env = "WHISPER_RELAY_LANGUAGE")]
    language: Option<String>,

    #[arg(long)]
    source: Vec<String>,

    #[arg(long, default_value_t = false)]
    list_sources: bool,

    #[arg(long)]
    chunk_seconds: Option<u64>,

    #[arg(long, value_enum)]
    capture_mode: Option<CaptureMode>,

    #[arg(
        long,
        env = "WHISPER_RELAY_AUTO_ENABLE_NEW_STREAMS",
        action = ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    auto_enable_new_streams: Option<bool>,

    #[arg(long, env = "WHISPER_RELAY_AUDIO_RESCAN_SECONDS")]
    audio_rescan_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum DiarizationArg {
    Prefer,
    Require,
    Disable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    server_url: Option<String>,
    output: Option<PathBuf>,
    recording_output: Option<PathBuf>,
    events_output: Option<PathBuf>,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    token: Option<String>,
    token_cache: Option<PathBuf>,
    disable_token_cache: Option<bool>,
    insecure_no_auth: Option<bool>,
    diarization: Option<DiarizationArg>,
    audio_file: Option<PathBuf>,
    language: Option<String>,
    #[serde(default)]
    source: Vec<String>,
    chunk_seconds: Option<u64>,
    capture_mode: Option<CaptureMode>,
    auto_enable_new_streams: Option<bool>,
    audio_rescan_seconds: Option<u64>,
}

#[derive(Debug, Clone)]
struct ClientConfig {
    server_url: String,
    output: PathBuf,
    recording_output: Option<PathBuf>,
    events_output: PathBuf,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    token: Option<String>,
    token_cache: Option<PathBuf>,
    disable_token_cache: bool,
    insecure_no_auth: bool,
    diarization: DiarizationArg,
    audio_file: Option<PathBuf>,
    language: Option<String>,
    source: Vec<String>,
    chunk_seconds: u64,
    capture_mode: CaptureMode,
    auto_enable_new_streams: bool,
    audio_rescan_seconds: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CaptureMode {
    Meeting,
    Live,
}

impl From<DiarizationArg> for DiarizationPreference {
    fn from(value: DiarizationArg) -> Self {
        match value {
            DiarizationArg::Prefer => Self::Prefer,
            DiarizationArg::Require => Self::Require,
            DiarizationArg::Disable => Self::Disable,
        }
    }
}

#[derive(Debug, Deserialize)]
struct OpenIdConfiguration {
    device_authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_poll_interval")]
    interval: u64,
    message: Option<String>,
}

fn default_poll_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenCache {
    issuer: String,
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
}

#[derive(Debug)]
struct AcquiredToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    exp: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct PwNode {
    id: u64,
    #[serde(default)]
    info: PwInfo,
}

#[derive(Debug, Default, Deserialize)]
struct PwInfo {
    #[serde(default)]
    props: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
struct AudioSource {
    id: String,
    name: String,
    description: String,
    media_class: String,
    application_name: Option<String>,
    binary_name: Option<String>,
}

type SharedAudioState = Arc<RwLock<AudioState>>;
type SharedLogs = Arc<RwLock<LogBuffer>>;

#[derive(Debug, Clone)]
struct AudioState {
    sources: Vec<AudioSource>,
    selected_keys: BTreeSet<String>,
    active_ids: Vec<String>,
    auto_enable_new_streams: bool,
    capture_status: String,
    capture_mode: CaptureMode,
    recording_requested: bool,
    recording_started_at: Option<Instant>,
    quit_requested: bool,
}

impl AudioState {
    fn is_selected(&self, source: &AudioSource) -> bool {
        self.selected_keys
            .iter()
            .any(|selected| source.matches_configured(selected))
    }
}

#[derive(Debug)]
struct LogBuffer {
    lines: VecDeque<String>,
    capacity: usize,
}

impl LogBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, line: impl Into<String>) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines
            .push_back(format!("{} {}", Utc::now().format("%H:%M:%S"), line.into()));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "whisper_relay_client=info".into()),
        )
        .init();

    let args = CliArgs::parse();
    if args.list_sources {
        for source in discover_sources().await? {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                source.id,
                source.identity_key(),
                source.media_class,
                source.capture_role(),
                source.recommendation(),
                source.name,
                source.description
            );
        }
        return Ok(());
    }
    let config = ClientConfig::load(args)?;
    let logs = Arc::new(RwLock::new(LogBuffer::new(200)));

    let token = acquire_token(&config).await?;
    let mut request = config.server_url.clone().into_client_request()?;
    if let Some(token) = token {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .context("invalid bearer token")?,
        );
    }

    let (mut ws, _) = connect_async(request).await?;
    push_log(&logs, "connected to server").await;
    let buffered_upload =
        config.audio_file.is_some() || config.capture_mode == CaptureMode::Meeting;
    let hello = ClientMessage::Hello(ClientHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: hostname(),
        diarization: config.diarization.clone().into(),
        audio: config.audio_format(),
        language: config.language.clone(),
        buffer_audio_until_end: buffered_upload,
    });
    ws.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;

    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.output)
        .await
        .with_context(|| format!("opening {}", config.output.display()))?;
    write_session_header(&mut output).await?;
    let mut events_output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.events_output)
        .await
        .with_context(|| format!("opening {}", config.events_output.display()))?;

    let audio_state = build_audio_state(&config, &logs).await?;
    let tui = if config.audio_file.is_none() {
        Some(spawn_tui(audio_state.clone(), logs.clone()))
    } else {
        None
    };
    let mut audio = AudioInput::open(&config, audio_state.clone(), logs.clone()).await?;
    let headless = config.audio_file.is_some();
    let mut transcript_count = 0_u64;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            chunk = audio.next_chunk() => {
                let Some(chunk) = chunk? else {
                    let audio_end = serde_json::to_string(&ClientMessage::AudioEnd)?;
                    if let Err(err) = ws.send(Message::Text(audio_end.into())).await {
                        if !is_expected_shutdown_ws_error(&err) {
                            return Err(err.into());
                        }
                    }
                    break;
                };
                if buffered_upload {
                    const FRAME_BYTES: usize = 64 * 1024;
                    const PROGRESS_BYTES: usize = 8 * 1024 * 1024;
                    for (index, frame) in chunk.chunks(FRAME_BYTES).enumerate() {
                        let offset = index * FRAME_BYTES;
                        ws.send(Message::Binary(frame.to_vec().into()))
                            .await
                            .with_context(|| {
                                format!("uploading meeting audio at byte offset {offset}")
                            })?;
                        if headless && (offset + frame.len()) / PROGRESS_BYTES > offset / PROGRESS_BYTES {
                            eprintln!(
                                "uploaded {} of {} MiB",
                                (offset + frame.len()) / (1024 * 1024),
                                chunk.len() / (1024 * 1024)
                            );
                        }
                    }
                    if headless {
                        eprintln!("meeting audio upload complete; waiting for transcription");
                    }
                } else {
                    ws.send(Message::Binary(chunk.into())).await?;
                }
            }
            message = ws.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        transcript_count += u64::from(
                            handle_server_message(
                                &mut output,
                                &mut events_output,
                                &logs,
                                &text,
                                headless,
                            )
                            .await?,
                        );
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err.into()),
                }
            }
            _ = heartbeat.tick() => {
                let ping = ClientMessage::Ping {
                    nonce: Utc::now().timestamp_millis().to_string(),
                };
                ws.send(Message::Text(serde_json::to_string(&ping)?.into())).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                request_quit(&audio_state).await;
                push_log(&logs, "stopping capture cleanly").await;
            }
        }
    }

    while let Some(message) = ws.next().await {
        match message {
            Ok(Message::Text(text)) => {
                transcript_count += u64::from(
                    handle_server_message(&mut output, &mut events_output, &logs, &text, headless)
                        .await?,
                );
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(err) if !headless && is_expected_shutdown_ws_error(&err) => break,
            Err(err) => return Err(err.into()),
        }
    }

    if let Some(tui) = tui {
        request_quit(&audio_state).await;
        let _ = tui.await;
    }

    if headless && transcript_count == 0 {
        bail!("server returned no transcript events; see the server messages above");
    }

    Ok(())
}

impl ClientConfig {
    fn load(args: CliArgs) -> Result<Self> {
        let file = load_file_config(args.config.as_ref())?;
        let output = expand_home(
            args.output
                .or(file.output)
                .unwrap_or_else(|| PathBuf::from("transcript.md")),
        );
        let events_output = args
            .events_output
            .or(file.events_output)
            .map(expand_home)
            .unwrap_or_else(|| output.with_extension("events.jsonl"));
        Ok(Self {
            server_url: args
                .server_url
                .or(file.server_url)
                .unwrap_or_else(|| "ws://127.0.0.1:8080/v1/sessions/ws".into()),
            output,
            recording_output: args
                .recording_output
                .or(file.recording_output)
                .map(expand_home),
            events_output,
            oidc_issuer: args.oidc_issuer.or(file.oidc_issuer),
            oidc_client_id: args.oidc_client_id.or(file.oidc_client_id),
            token: args.token.or(file.token),
            token_cache: args
                .token_cache
                .or(file.token_cache)
                .map(expand_home)
                .or_else(default_token_cache_path),
            disable_token_cache: args
                .disable_token_cache
                .or(file.disable_token_cache)
                .unwrap_or(false),
            insecure_no_auth: args
                .insecure_no_auth
                .or(file.insecure_no_auth)
                .unwrap_or(false),
            diarization: args
                .diarization
                .or(file.diarization)
                .unwrap_or(DiarizationArg::Prefer),
            audio_file: args.audio_file.or(file.audio_file).map(expand_home),
            language: args.language.or(file.language),
            source: if args.source.is_empty() {
                file.source
            } else {
                args.source
            },
            chunk_seconds: args.chunk_seconds.or(file.chunk_seconds).unwrap_or(15),
            capture_mode: args
                .capture_mode
                .or(file.capture_mode)
                .unwrap_or(CaptureMode::Meeting),
            auto_enable_new_streams: args
                .auto_enable_new_streams
                .or(file.auto_enable_new_streams)
                .unwrap_or(false),
            audio_rescan_seconds: args
                .audio_rescan_seconds
                .or(file.audio_rescan_seconds)
                .unwrap_or(2),
        })
    }

    fn audio_format(&self) -> AudioFormat {
        if self
            .audio_file
            .as_ref()
            .and_then(|path| path.extension())
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ogg"))
        {
            return AudioFormat {
                codec: AudioCodec::Opus,
                container: AudioContainer::Ogg,
                sample_rate_hz: 48_000,
                channels: 1,
            };
        }

        AudioFormat {
            codec: AudioCodec::WavPcm16,
            container: AudioContainer::Wav,
            sample_rate_hz: 16_000,
            channels: 1,
        }
    }
}

fn load_file_config(path: Option<&PathBuf>) -> Result<FileConfig> {
    let Some(path) = path.cloned().map(expand_home).or_else(default_config_path) else {
        return Ok(FileConfig::default());
    };
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))
}

fn expand_home(path: PathBuf) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path;
    };
    if value == "~" {
        return home_dir().unwrap_or(path);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir().map(|home| home.join(rest)).unwrap_or(path);
    }
    path
}

fn default_config_path() -> Option<PathBuf> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config_home).join("whisper-relay/client.toml"));
    }
    home_dir().map(|home| home.join(".config/whisper-relay/client.toml"))
}

fn default_token_cache_path() -> Option<PathBuf> {
    if let Some(cache_home) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(cache_home).join("whisper-relay/oidc-token.json"));
    }
    home_dir().map(|home| home.join(".cache/whisper-relay/oidc-token.json"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

async fn acquire_token(config: &ClientConfig) -> Result<Option<String>> {
    if config.insecure_no_auth {
        return Ok(None);
    }
    if let Some(token) = &config.token {
        return Ok(Some(token.clone()));
    }

    let issuer = config
        .oidc_issuer
        .as_ref()
        .context("--oidc-issuer or WHISPER_RELAY_OIDC_ISSUER is required unless --token or --insecure-no-auth is used")?;
    let client_id = config
        .oidc_client_id
        .as_ref()
        .context("--oidc-client-id or WHISPER_RELAY_OIDC_CLIENT_ID is required for device login")?;
    let cache_path = config
        .token_cache
        .as_ref()
        .filter(|_| !config.disable_token_cache);

    if let Some(path) = cache_path {
        if let Some(cache) = load_token_cache(path).await? {
            if cache.issuer == *issuer && cache.client_id == *client_id {
                if cache.expires_at > Utc::now().timestamp() + 60 {
                    return Ok(Some(cache.access_token));
                }
                if let Some(refresh_token) = cache.refresh_token {
                    let discovery = discover_oidc(issuer).await?;
                    if let Ok(token) =
                        refresh_access_token(&discovery, client_id, &refresh_token).await
                    {
                        let access_token = token.access_token.clone();
                        save_token_cache(path, issuer, client_id, token).await?;
                        return Ok(Some(access_token));
                    }
                }
            }
        }
    }

    let discovery = discover_oidc(issuer).await?;
    let token = device_login(&discovery, client_id).await?;
    let access_token = token.access_token.clone();
    if let Some(path) = cache_path {
        save_token_cache(path, issuer, client_id, token).await?;
    }

    Ok(Some(access_token))
}

async fn discover_oidc(issuer: &str) -> Result<OpenIdConfiguration> {
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    reqwest::get(&discovery_url)
        .await?
        .error_for_status()?
        .json()
        .await
        .with_context(|| format!("loading OIDC discovery document from {discovery_url}"))
}

async fn device_login(discovery: &OpenIdConfiguration, client_id: &str) -> Result<AcquiredToken> {
    let http = reqwest::Client::new();
    let device: DeviceAuthorizationResponse = http
        .post(&discovery.device_authorization_endpoint)
        .form(&[("client_id", client_id), ("scope", "openid profile email")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if let Some(message) = &device.message {
        println!("{message}");
    } else if let Some(uri) = &device.verification_uri_complete {
        println!("Open {uri}");
    } else if let Some(uri) = &device.verification_uri {
        println!("Open {uri} and enter code {}", device.user_code);
    }

    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(device.expires_in) {
        sleep(Duration::from_secs(device.interval)).await;
        let response: TokenResponse = http
            .post(&discovery.token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await?
            .json()
            .await?;
        if response.access_token.is_some() {
            return token_response_to_acquired(response);
        }
        match response.error.as_deref() {
            Some("authorization_pending") | Some("slow_down") => continue,
            Some(error) => {
                bail!(
                    "device authorization failed: {} ({})",
                    error,
                    response.error_description.unwrap_or_default()
                );
            }
            None => {}
        }
    }

    bail!("device authorization expired")
}

async fn refresh_access_token(
    discovery: &OpenIdConfiguration,
    client_id: &str,
    refresh_token: &str,
) -> Result<AcquiredToken> {
    let response: TokenResponse = reqwest::Client::new()
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await?
        .json()
        .await?;
    token_response_to_acquired(response)
}

fn token_response_to_acquired(response: TokenResponse) -> Result<AcquiredToken> {
    let access_token = response
        .access_token
        .context("token response omitted access_token")?;
    let expires_at = response
        .expires_in
        .and_then(|expires_in| i64::try_from(expires_in).ok())
        .map(|expires_in| Utc::now().timestamp() + expires_in)
        .or_else(|| jwt_exp(&access_token));
    Ok(AcquiredToken {
        access_token,
        refresh_token: response.refresh_token,
        expires_at,
    })
}

async fn load_token_cache(path: &PathBuf) -> Result<Option<TokenCache>> {
    let contents = match fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&contents)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

async fn save_token_cache(
    path: &PathBuf,
    issuer: &str,
    client_id: &str,
    token: AcquiredToken,
) -> Result<()> {
    let Some(expires_at) = token.expires_at else {
        return Ok(());
    };
    let cache = TokenCache {
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let contents = serde_json::to_vec_pretty(&cache)?;
    fs::write(path, contents)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        let permissions =
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600);
        fs::set_permissions(path, permissions)
            .await
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<JwtClaims>(&bytes).ok()?.exp
}

async fn build_audio_state(
    config: &ClientConfig,
    logs: &SharedLogs,
) -> Result<Option<SharedAudioState>> {
    if config.audio_file.is_some() {
        return Ok(None);
    }
    let sources = discover_sources().await?;
    let selected_keys = if config.source.is_empty() {
        BTreeSet::new()
    } else {
        resolve_configured_sources_from(&sources, &config.source, logs).await
    };
    let state = AudioState {
        sources,
        selected_keys,
        active_ids: Vec::new(),
        auto_enable_new_streams: config.auto_enable_new_streams,
        capture_status: "starting".into(),
        capture_mode: config.capture_mode,
        recording_requested: config.capture_mode == CaptureMode::Live,
        recording_started_at: None,
        quit_requested: false,
    };
    Ok(Some(Arc::new(RwLock::new(state))))
}

async fn request_quit(state: &Option<SharedAudioState>) {
    if let Some(state) = state {
        state.write().await.quit_requested = true;
    }
}

async fn push_log(logs: &SharedLogs, line: impl Into<String>) {
    logs.write().await.push(line);
}

async fn handle_server_message(
    output: &mut tokio::fs::File,
    events_output: &mut tokio::fs::File,
    logs: &SharedLogs,
    text: &str,
    headless: bool,
) -> Result<bool> {
    match serde_json::from_str::<ServerMessage>(text)? {
        ServerMessage::SessionReady(ready) => {
            let message = format!("session ready {}", ready.session_id);
            push_log(logs, &message).await;
            if headless {
                eprintln!("{message}");
            }
        }
        ServerMessage::TranscriptFinal(event) => {
            push_log(
                logs,
                format!("transcript {}", truncate_for_log(&event.text)),
            )
            .await;
            append_transcript(output, &event).await?;
            events_output
                .write_all(format!("{}\n", serde_json::to_string(&event)?).as_bytes())
                .await?;
            events_output.flush().await?;
            if headless {
                eprintln!("received transcript segment {}", event.sequence);
            }
            return Ok(true);
        }
        ServerMessage::TranscriptPartial(_) => {}
        ServerMessage::Warning(warning) => {
            let message = format!("warning {}: {}", warning.code, warning.message);
            push_log(logs, &message).await;
            if headless {
                eprintln!("{message}");
            }
        }
        ServerMessage::Error(error) => bail!("server error {}: {}", error.code, error.message),
        ServerMessage::Pong { .. } => {}
    }
    Ok(false)
}

fn truncate_for_log(text: &str) -> String {
    const LIMIT: usize = 96;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let mut truncated = text.chars().take(LIMIT).collect::<String>();
    truncated.push_str("...");
    truncated
}

async fn append_transcript(output: &mut tokio::fs::File, event: &TranscriptEvent) -> Result<()> {
    let time = match (event.start_ms, event.end_ms) {
        (Some(start), Some(end)) => format!("[{}-{}]", format_ms(start), format_ms(end)),
        _ => format!("[{}]", event.received_at.format("%H:%M:%S")),
    };
    let speaker = event.speaker.as_deref().unwrap_or("Unknown");
    output
        .write_all(format!("{time} **{speaker}:** {}\n\n", event.text).as_bytes())
        .await?;
    output.flush().await?;
    Ok(())
}

async fn write_session_header(output: &mut tokio::fs::File) -> Result<()> {
    output
        .write_all(format!("\n\n## Session {}\n\n", Utc::now().to_rfc3339()).as_bytes())
        .await?;
    Ok(())
}

fn is_expected_shutdown_ws_error(err: &WsError) -> bool {
    matches!(
        err,
        WsError::ConnectionClosed
            | WsError::AlreadyClosed
            | WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake)
    )
}

fn format_ms(ms: u64) -> String {
    let total_seconds = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60
    )
}

enum AudioInput {
    File(Option<Vec<u8>>),
    Live(Box<LiveCapture>),
    Meeting(Box<MeetingCapture>),
}

struct MeetingCapture {
    active_ids: Vec<String>,
    child: Option<tokio::process::Child>,
    dir: tempfile::TempDir,
    segment_paths: Vec<PathBuf>,
    recording_output: PathBuf,
    audio_rescan_interval: Duration,
    last_rescan: Instant,
    state: SharedAudioState,
    logs: SharedLogs,
    finished: bool,
}

struct LiveCapture {
    active_ids: Vec<String>,
    location_pattern: String,
    next_index: u64,
    child: Option<tokio::process::Child>,
    dir: tempfile::TempDir,
    chunk_seconds: u64,
    audio_rescan_interval: Duration,
    last_rescan: Instant,
    restart_after: Option<Instant>,
    restart_attempts: u32,
    state: SharedAudioState,
    logs: SharedLogs,
}

impl AudioInput {
    async fn open(
        config: &ClientConfig,
        audio_state: Option<SharedAudioState>,
        logs: SharedLogs,
    ) -> Result<Self> {
        if let Some(path) = &config.audio_file {
            let bytes = fs::read(path)
                .await
                .with_context(|| format!("reading {}", path.display()))?;
            return Ok(Self::File(Some(bytes)));
        }
        let state = audio_state.context("live capture requires audio state")?;

        if config.capture_mode == CaptureMode::Meeting {
            return Ok(Self::Meeting(Box::new(MeetingCapture {
                active_ids: Vec::new(),
                child: None,
                dir: tempfile::tempdir()?,
                segment_paths: Vec::new(),
                recording_output: meeting_audio_path(config),
                audio_rescan_interval: Duration::from_secs(config.audio_rescan_seconds.max(1)),
                last_rescan: Instant::now()
                    .checked_sub(Duration::from_secs(config.audio_rescan_seconds.max(1)))
                    .unwrap_or_else(Instant::now),
                state,
                logs,
                finished: false,
            })));
        }

        let dir = tempfile::tempdir()?;
        let location_pattern = dir.path().join("chunk-%05d.wav").display().to_string();
        let mut capture = LiveCapture {
            active_ids: Vec::new(),
            dir,
            location_pattern,
            next_index: 0,
            child: None,
            chunk_seconds: config.chunk_seconds,
            audio_rescan_interval: Duration::from_secs(config.audio_rescan_seconds.max(1)),
            last_rescan: Instant::now()
                .checked_sub(Duration::from_secs(config.audio_rescan_seconds.max(1)))
                .unwrap_or_else(Instant::now),
            restart_after: None,
            restart_attempts: 0,
            state,
            logs,
        };
        capture.refresh_pipeline().await?;
        Ok(Self::Live(Box::new(capture)))
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::File(bytes) => Ok(bytes.take()),
            Self::Live(capture) => capture.next_chunk().await,
            Self::Meeting(capture) => capture.next_audio().await,
        }
    }
}

impl MeetingCapture {
    async fn next_audio(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if self.finished {
                return Ok(None);
            }
            if self.last_rescan.elapsed() >= self.audio_rescan_interval {
                self.refresh_sources().await?;
            }

            let (recording_requested, quit_requested) = {
                let state = self.state.read().await;
                (state.recording_requested, state.quit_requested)
            };
            if !recording_requested && !self.segment_paths.is_empty() {
                return self.finish().await.map(Some);
            }
            if quit_requested {
                if self.child.is_some() || !self.segment_paths.is_empty() {
                    return self.finish().await.map(Some);
                }
                self.finished = true;
                return Ok(None);
            }

            if let Some(reason) = self.child_exit_reason().await? {
                self.log(format!("meeting recorder exited unexpectedly ({reason})"))
                    .await;
                self.start_segment().await?;
            }
            sleep(Duration::from_millis(200)).await;
        }
    }

    async fn refresh_sources(&mut self) -> Result<()> {
        self.last_rescan = Instant::now();
        let sources = discover_sources().await?;
        let (selected_keys, recording_requested) = {
            let mut state = self.state.write().await;
            state.sources = sources;
            if state.auto_enable_new_streams {
                let keys = state
                    .sources
                    .iter()
                    .map(AudioSource::identity_key)
                    .collect::<Vec<_>>();
                state.selected_keys.extend(keys);
            }
            (state.selected_keys.clone(), state.recording_requested)
        };
        let sources = self.state.read().await.sources.clone();
        let active_ids = sources
            .iter()
            .filter(|source| {
                selected_keys
                    .iter()
                    .any(|key| source.matches_configured(key))
            })
            .map(|source| source.id.clone())
            .collect::<Vec<_>>();

        if active_ids != self.active_ids {
            if self.child.is_some() {
                self.finalize_segment().await?;
            }
            self.active_ids = active_ids;
            if recording_requested {
                self.start_segment().await?;
            }
        } else if recording_requested && self.child.is_none() {
            self.start_segment().await?;
        }

        let mut state = self.state.write().await;
        state.active_ids = self.active_ids.clone();
        if !recording_requested {
            state.capture_status = "ready; press r to record".into();
        } else if self.active_ids.is_empty() {
            state.capture_status = "recording; waiting for selected streams".into();
        } else {
            state.capture_status = format!("recording {} stream(s)", self.active_ids.len());
        }
        Ok(())
    }

    async fn start_segment(&mut self) -> Result<()> {
        if self.child.is_some() || self.active_ids.is_empty() {
            return Ok(());
        }
        let path = self
            .dir
            .path()
            .join(format!("meeting-{:05}.wav", self.segment_paths.len()));
        self.child = Some(spawn_meeting_gstreamer(
            &self.active_ids,
            &path.display().to_string(),
        )?);
        self.segment_paths.push(path);
        self.log(format!(
            "recording meeting from {} stream(s)",
            self.active_ids.len()
        ))
        .await;
        Ok(())
    }

    async fn finalize_segment(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Some(id) = child.id() {
            let status = Command::new("kill")
                .args(["-INT", &id.to_string()])
                .status()
                .await?;
            if !status.success() {
                child.start_kill()?;
            }
        }
        let status = child.wait().await?;
        if !status.success() {
            self.log(format!("meeting recorder finalized with {status}"))
                .await;
        }
        Ok(())
    }

    async fn child_exit_reason(&mut self) -> Result<Option<String>> {
        let Some(child) = &mut self.child else {
            return Ok(None);
        };
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        self.child = None;
        Ok(Some(status.to_string()))
    }

    async fn finish(&mut self) -> Result<Vec<u8>> {
        self.finalize_segment().await?;
        {
            let mut state = self.state.write().await;
            state.capture_status = "preparing full meeting upload".into();
            state.active_ids.clear();
        }
        let bytes = merge_wav_segments(&self.segment_paths).await?;
        if let Some(parent) = self
            .recording_output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.recording_output, &bytes)
            .await
            .with_context(|| format!("writing {}", self.recording_output.display()))?;
        self.log(format!(
            "saved recording to {}",
            self.recording_output.display()
        ))
        .await;
        self.log(format!(
            "uploading full meeting ({} MiB)",
            bytes.len() / 1_048_576
        ))
        .await;
        self.finished = true;
        Ok(bytes)
    }

    async fn log(&self, line: impl Into<String>) {
        push_log(&self.logs, line).await;
    }
}

impl Drop for MeetingCapture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

impl LiveCapture {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if self.last_rescan.elapsed() >= self.audio_rescan_interval {
                self.refresh_pipeline().await?;
            }
            if self.state.read().await.quit_requested {
                return Ok(None);
            }

            let path = chunk_path(&self.location_pattern, self.next_index);
            if let Some(reason) = self.child_exit_reason().await? {
                if capture_completed(&reason) {
                    let bytes = tokio::fs::read(&path).await.with_context(|| {
                        format!("reading finalized audio chunk {}", path.display())
                    })?;
                    let _ = tokio::fs::remove_file(&path).await;
                    self.next_index += 1;
                    self.log(format!(
                        "forwarding audio chunk {} ({} bytes)",
                        self.next_index,
                        bytes.len()
                    ))
                    .await;
                    return Ok(Some(bytes));
                }
                self.schedule_restart(reason).await;
                self.refresh_pipeline().await?;
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    async fn refresh_pipeline(&mut self) -> Result<()> {
        self.last_rescan = Instant::now();
        let sources = discover_sources().await?;
        let (selected_keys, auto_enabled, quit_requested) = {
            let mut state = self.state.write().await;
            state.sources = sources;
            let mut auto_enabled = Vec::new();
            if state.auto_enable_new_streams {
                let keys = state
                    .sources
                    .iter()
                    .map(AudioSource::identity_key)
                    .collect::<Vec<_>>();
                for key in keys {
                    if state.selected_keys.insert(key.clone()) {
                        auto_enabled.push(key);
                    }
                }
            }
            (
                state.selected_keys.clone(),
                auto_enabled,
                state.quit_requested,
            )
        };
        if quit_requested {
            self.stop_child();
            return Ok(());
        }

        let sources = self.state.read().await.sources.clone();
        for key in auto_enabled {
            self.log(format!("auto-enabled stream {key}")).await;
        }

        let active_ids = sources
            .iter()
            .filter(|source| {
                selected_keys
                    .iter()
                    .any(|selected| source.matches_configured(selected))
            })
            .map(|source| source.id.clone())
            .collect::<Vec<_>>();
        let ids_changed = active_ids != self.active_ids;
        if let Some(reason) = self.child_exit_reason().await? {
            self.schedule_restart(reason).await;
        }
        let child_running = self.child.is_some();
        if !ids_changed && child_running {
            return Ok(());
        }

        if ids_changed {
            self.stop_child();
            self.active_ids = active_ids;
            self.restart_after = None;
            self.restart_attempts = 0;
            self.next_index = 0;
            self.clean_chunk_files().await?;
        }

        if self.active_ids.is_empty() {
            {
                let mut state = self.state.write().await;
                state.active_ids = self.active_ids.clone();
                state.capture_status = "waiting for selected streams".into();
            }
            if ids_changed {
                self.log("no selected PipeWire streams are currently available")
                    .await;
            }
            return Ok(());
        }

        if let Some(restart_after) = self.restart_after {
            let now = Instant::now();
            if now < restart_after {
                let remaining = restart_after
                    .saturating_duration_since(now)
                    .as_secs()
                    .max(1);
                let mut state = self.state.write().await;
                state.active_ids = self.active_ids.clone();
                state.capture_status = format!("capture crashed; retrying in {remaining}s");
                return Ok(());
            }
        }

        {
            let mut state = self.state.write().await;
            state.active_ids = self.active_ids.clone();
            state.capture_status = format!("capturing {} stream(s)", self.active_ids.len());
        }
        let path = chunk_path(&self.location_pattern, self.next_index);
        let _ = tokio::fs::remove_file(&path).await;
        self.child = Some(spawn_gstreamer(
            &self.active_ids,
            &path.display().to_string(),
            self.chunk_seconds,
        )?);
        self.restart_after = None;
        self.log(format!(
            "started capture for {} stream(s)",
            self.active_ids.len()
        ))
        .await;
        Ok(())
    }

    async fn child_exit_reason(&mut self) -> Result<Option<String>> {
        let Some(child) = &mut self.child else {
            return Ok(None);
        };
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        let stderr = child.stderr.take();
        let mut reason = format!("status {status}");
        if let Some(mut stderr) = stderr {
            let mut text = String::new();
            stderr.read_to_string(&mut text).await?;
            if let Some(text) = compact_stderr(&text) {
                reason.push_str(": ");
                reason.push_str(&text);
            }
        }
        self.child = None;
        Ok(Some(reason))
    }

    async fn schedule_restart(&mut self, reason: String) {
        let delay = restart_delay(self.restart_attempts);
        self.restart_attempts = self.restart_attempts.saturating_add(1);
        self.restart_after = Some(Instant::now() + delay);
        {
            let mut state = self.state.write().await;
            state.capture_status = format!("capture crashed; retrying in {}s", delay.as_secs());
        }
        self.log(format!(
            "audio capture exited ({reason}); retrying in {}s",
            delay.as_secs()
        ))
        .await;
    }

    fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }

    async fn clean_chunk_files(&self) -> Result<()> {
        let mut entries = tokio::fs::read_dir(self.dir.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("chunk-") && (name.ends_with(".ogg") || name.ends_with(".wav")) {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
        Ok(())
    }

    async fn log(&self, line: impl Into<String>) {
        push_log(&self.logs, line).await;
    }
}

impl Drop for LiveCapture {
    fn drop(&mut self) {
        self.stop_child();
    }
}

fn restart_delay(attempts: u32) -> Duration {
    Duration::from_secs(2_u64.saturating_pow(attempts.min(4)))
}

fn compact_stderr(text: &str) -> Option<String> {
    let compact = text
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())?
        .chars()
        .take(160)
        .collect::<String>();
    Some(compact)
}

fn capture_completed(reason: &str) -> bool {
    reason.contains("exit status: 0") || reason.contains("exit status: 124")
}

fn chunk_path(pattern: &str, index: u64) -> PathBuf {
    PathBuf::from(pattern.replace("%05d", &format!("{index:05}")))
}

async fn resolve_configured_sources_from(
    sources: &[AudioSource],
    configured_sources: &[String],
    logs: &SharedLogs,
) -> BTreeSet<String> {
    let mut selected = BTreeSet::new();
    for configured in configured_sources {
        let matches = sources
            .iter()
            .filter(|source| source.matches_configured(configured))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            push_log(
                logs,
                format!("configured source not currently available: {configured}"),
            )
            .await;
            selected.insert(configured.clone());
        } else {
            selected.extend(matches.into_iter().map(AudioSource::identity_key));
        }
    }
    selected
}

fn spawn_tui(state: Option<SharedAudioState>, logs: SharedLogs) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let state = state.context("tui requires audio state")?;
        let mut terminal = TuiSession::enter()?;
        let mut list_state = ListState::default();
        list_state.select(Some(0));

        loop {
            let snapshot = state.read().await.clone();
            let log_lines = logs.read().await.lines.iter().cloned().collect::<Vec<_>>();
            if snapshot.quit_requested {
                break;
            }
            if !snapshot.sources.is_empty()
                && list_state.selected().unwrap_or(0) >= snapshot.sources.len()
            {
                list_state.select(Some(snapshot.sources.len() - 1));
            }

            terminal.draw(&snapshot, &log_lines, &mut list_state)?;
            if event::poll(Duration::from_millis(150))? {
                let Event::Key(key) = event::read()? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        state.write().await.quit_requested = true;
                        break;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        move_selection(&mut list_state, snapshot.sources.len(), 1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        move_selection(&mut list_state, snapshot.sources.len(), -1)
                    }
                    KeyCode::Char(' ') => {
                        let idx = list_state.selected().unwrap_or(0);
                        if let Some(source) = snapshot.sources.get(idx) {
                            let key = source.identity_key();
                            let mut state = state.write().await;
                            if !state.selected_keys.insert(key.clone()) {
                                state.selected_keys.remove(&key);
                                push_log(&logs, format!("disabled {}", source.description)).await;
                            } else {
                                push_log(&logs, format!("enabled {}", source.description)).await;
                            }
                        }
                    }
                    KeyCode::Char('a') => {
                        let mut state = state.write().await;
                        state.auto_enable_new_streams = !state.auto_enable_new_streams;
                        push_log(
                            &logs,
                            format!("auto-enable new streams: {}", state.auto_enable_new_streams),
                        )
                        .await;
                    }
                    KeyCode::Char('r') if snapshot.capture_mode == CaptureMode::Meeting => {
                        let mut state = state.write().await;
                        if state.recording_requested {
                            state.recording_requested = false;
                            state.recording_started_at = None;
                            state.capture_status = "finalizing meeting".into();
                            push_log(&logs, "stopping recording; full transcription will start")
                                .await;
                        } else if !state.selected_keys.is_empty() {
                            state.recording_requested = true;
                            state.recording_started_at = Some(Instant::now());
                            push_log(&logs, "meeting recording started").await;
                        } else {
                            push_log(&logs, "select at least one audio stream before recording")
                                .await;
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    })
}

struct TuiSession {
    terminal: Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
}

impl TuiSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn draw(
        &mut self,
        state: &AudioState,
        logs: &[String],
        list_state: &mut ListState,
    ) -> Result<()> {
        self.terminal.draw(|frame| {
            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(8),
                    Constraint::Length(3),
                ])
                .split(frame.area());

            let header = Paragraph::new(format!(
                "Whisper Relay | {} | active {} | auto-enable {}{}",
                state.capture_status,
                state.active_ids.len(),
                state.auto_enable_new_streams,
                state.recording_started_at.map(|started| format!(" | {}", format_duration(started.elapsed()))).unwrap_or_default()
            ))
            .block(Block::default().borders(Borders::ALL).title("Client"));
            frame.render_widget(header, outer[0]);

            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                .split(outer[1]);

            let items = state.sources.iter().map(|source| {
                let selected = state.is_selected(source);
                let active = state.active_ids.contains(&source.id);
                let mark = if selected && active {
                    "[*]"
                } else if selected {
                    "[x]"
                } else {
                    "[ ]"
                };
                let line = Line::from(vec![
                    Span::raw(format!("{mark} ")),
                    Span::styled(
                        source.description.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "  {}  {}  id={}",
                        source.capture_role(),
                        source.detail_label(),
                        source.id
                    )),
                ]);
                ListItem::new(line)
            });
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Audio Streams"),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> ");
            frame.render_stateful_widget(list, columns[0], list_state);

            let log_items = logs
                .iter()
                .rev()
                .take(columns[1].height.saturating_sub(2) as usize)
                .rev()
                .map(|line| Line::from(line.clone()))
                .collect::<Vec<_>>();
            let log_panel = Paragraph::new(log_items)
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title("Log"));
            frame.render_widget(log_panel, columns[1]);

            let help_text = if state.capture_mode == CaptureMode::Meeting {
                "r starts/stops meeting  Space toggles stream  a toggles auto-enable  q/Esc stops and transcribes  App playback = people you hear  Microphone = you"
            } else {
                "Space toggles stream  a toggles auto-enable  q/Esc quits  App playback = people you hear  Microphone = you"
            };
            let help = Paragraph::new(help_text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Keys"));
            frame.render_widget(help, outer[2]);
        })?;
        Ok(())
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn move_selection(state: &mut ListState, len: usize, delta: isize) {
    let current = state.selected().unwrap_or(0);
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(len.saturating_sub(1))
    };
    state.select(Some(next));
}

async fn discover_sources() -> Result<Vec<AudioSource>> {
    let output = Command::new("pw-dump")
        .output()
        .await
        .context("running pw-dump; install pipewire tools")?;
    if !output.status.success() {
        bail!("pw-dump failed with status {}", output.status);
    }
    let nodes: Vec<PwNode> = serde_json::from_slice(&output.stdout)?;
    let mut sources = nodes
        .into_iter()
        .filter_map(|node| {
            let media_class = prop(&node.info.props, "media.class")?;
            if !matches!(
                media_class.as_str(),
                "Audio/Source" | "Audio/Sink" | "Stream/Output/Audio" | "Stream/Input/Audio"
            ) {
                return None;
            }
            let name = prop(&node.info.props, "node.name").unwrap_or_else(|| node.id.to_string());
            let application_name = prop(&node.info.props, "application.name");
            let binary_name = prop(&node.info.props, "application.process.binary");
            let description = prop(&node.info.props, "node.description")
                .or_else(|| application_name.clone())
                .unwrap_or_else(|| name.clone());
            Some(AudioSource {
                id: node.id.to_string(),
                name,
                description,
                media_class,
                application_name,
                binary_name,
            })
        })
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| {
        a.sort_rank()
            .cmp(&b.sort_rank())
            .then_with(|| a.description.cmp(&b.description))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(sources)
}

impl AudioSource {
    fn identity_key(&self) -> String {
        format!("{}:{}", self.media_class, self.name)
    }

    fn matches_configured(&self, configured: &str) -> bool {
        configured == self.id
            || configured == self.name
            || configured == self.description
            || configured == self.identity_key()
    }

    fn capture_role(&self) -> &'static str {
        match self.media_class.as_str() {
            "Stream/Output/Audio" => "App playback",
            "Audio/Source" => "Microphone",
            "Audio/Sink" => "Speaker output",
            "Stream/Input/Audio" => "App mic input",
            _ => "Audio",
        }
    }

    fn recommendation(&self) -> &'static str {
        match self.media_class.as_str() {
            "Stream/Output/Audio" => "capture this for other people speaking in this app",
            "Audio/Source" => "capture this for your microphone",
            "Audio/Sink" => "capture this for everything routed to this output device",
            "Stream/Input/Audio" => {
                "usually not needed; this is what the app captures from your mic"
            }
            _ => "audio stream",
        }
    }

    fn detail_label(&self) -> String {
        let mut parts = vec![self.media_class.clone(), self.name.clone()];
        if let Some(application_name) = &self.application_name {
            if application_name != &self.description {
                parts.push(format!("app={application_name}"));
            }
        }
        if let Some(binary_name) = &self.binary_name {
            parts.push(format!("bin={binary_name}"));
        }
        parts.join("  ")
    }

    fn sort_rank(&self) -> u8 {
        match self.media_class.as_str() {
            "Stream/Output/Audio" => 0,
            "Audio/Source" => 1,
            "Audio/Sink" => 2,
            "Stream/Input/Audio" => 3,
            _ => 4,
        }
    }
}

fn spawn_gstreamer(
    sources: &[String],
    location: &str,
    chunk_seconds: u64,
) -> Result<tokio::process::Child> {
    Command::new("timeout")
        .args(gstreamer_args(sources, location, chunk_seconds))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context(
            "starting timeout/gst-launch-1.0; install coreutils, gstreamer, and pipewire plugins",
        )
}

fn prop(props: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    props.get(key)?.as_str().map(ToOwned::to_owned)
}

fn gstreamer_args(sources: &[String], location: &str, chunk_seconds: u64) -> Vec<String> {
    let mut args = vec![
        "-s".into(),
        "INT".into(),
        chunk_seconds.to_string(),
        "gst-launch-1.0".into(),
    ];
    args.extend(gstreamer_pipeline_args(sources, location));
    args
}

fn spawn_meeting_gstreamer(sources: &[String], location: &str) -> Result<tokio::process::Child> {
    Command::new("gst-launch-1.0")
        .args(gstreamer_pipeline_args(sources, location))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting gst-launch-1.0 meeting recorder")
}

fn gstreamer_pipeline_args(sources: &[String], location: &str) -> Vec<String> {
    let mut args = vec![
        "-e".into(),
        "-q".into(),
        "audiomixer".into(),
        "name=mixer".into(),
        "!".into(),
        "audioconvert".into(),
        "!".into(),
        "audioresample".into(),
        "!".into(),
        "audio/x-raw,format=S16LE,rate=16000,channels=1".into(),
        "!".into(),
        "wavenc".into(),
        "!".into(),
        "filesink".into(),
        format!("location={location}"),
    ];
    for source in sources {
        args.extend([
            "pipewiresrc".into(),
            format!("target-object={source}"),
            "!".into(),
            "audioconvert".into(),
            "!".into(),
            "audioresample".into(),
            "!".into(),
            "mixer.".into(),
        ]);
    }
    args
}

async fn merge_wav_segments(paths: &[PathBuf]) -> Result<Vec<u8>> {
    let mut pcm = Vec::new();
    for path in paths {
        let wav = fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        pcm.extend_from_slice(wav_pcm_data(&wav)?);
    }
    if pcm.is_empty() {
        bail!("meeting recording contained no audio");
    }
    Ok(wav_with_pcm(&pcm))
}

fn wav_pcm_data(wav: &[u8]) -> Result<&[u8]> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        bail!("recorder produced an invalid WAV file");
    }
    let mut offset = 12;
    while offset + 8 <= wav.len() {
        let size = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let start = offset + 8;
        let end = start.saturating_add(size);
        if end > wav.len() {
            break;
        }
        if &wav[offset..offset + 4] == b"data" {
            return Ok(&wav[start..end]);
        }
        offset = end + (size % 2);
    }
    bail!("recorder WAV file has no audio data")
}

fn wav_with_pcm(pcm: &[u8]) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&16_000_u32.to_le_bytes());
    wav.extend_from_slice(&32_000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&pcm[..data_len as usize]);
    wav
}

fn format_duration(duration: Duration) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        duration.as_secs() / 3600,
        (duration.as_secs() % 3600) / 60,
        duration.as_secs() % 60
    )
}

fn meeting_audio_path(config: &ClientConfig) -> PathBuf {
    if let Some(path) = &config.recording_output {
        return path.clone();
    }
    let stem = config
        .output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("meeting");
    let name = format!("{stem}-{}.wav", Utc::now().format("%Y%m%d-%H%M%S"));
    config.output.with_file_name(name)
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "whisper-relay-client".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_offsets() {
        assert_eq!(format_ms(3_723_000), "01:02:03");
    }

    #[test]
    fn merges_pcm_from_finalized_wav_segments() {
        let first = wav_with_pcm(&[1, 2, 3, 4]);
        let second = wav_with_pcm(&[5, 6, 7, 8]);
        let mut combined = Vec::new();
        combined.extend_from_slice(wav_pcm_data(&first).unwrap());
        combined.extend_from_slice(wav_pcm_data(&second).unwrap());
        let merged = wav_with_pcm(&combined);

        assert_eq!(wav_pcm_data(&merged).unwrap(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(u32::from_le_bytes(merged[40..44].try_into().unwrap()), 8);
    }

    #[test]
    fn default_meeting_audio_path_is_timestamped_beside_transcript() {
        let config = test_config();
        let path = meeting_audio_path(&config);
        assert_eq!(path.parent(), config.output.parent());
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("transcript-"));
        assert_eq!(path.extension().unwrap(), "wav");
    }

    #[test]
    fn builds_gstreamer_pipeline_for_two_sources() {
        let args = gstreamer_args(&["10".into(), "11".into()], "/tmp/chunk-00000.wav", 15);
        assert_eq!(args[0], "-s");
        assert_eq!(args[1], "INT");
        assert_eq!(args[2], "15");
        assert_eq!(args[3], "gst-launch-1.0");
        assert!(args.contains(&"-e".to_string()));
        assert!(args.contains(&"audiomixer".to_string()));
        assert!(args.contains(&"target-object=10".to_string()));
        assert!(args.contains(&"target-object=11".to_string()));
        assert!(args.contains(&"audio/x-raw,format=S16LE,rate=16000,channels=1".to_string()));
        assert!(args.contains(&"wavenc".to_string()));
        assert!(args.contains(&"filesink".to_string()));
        assert!(args.contains(&"location=/tmp/chunk-00000.wav".to_string()));
    }

    #[test]
    fn treats_reset_without_close_as_expected_during_shutdown() {
        assert!(is_expected_shutdown_ws_error(&WsError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
        )));
    }

    #[test]
    fn source_matches_current_id_or_identity_key() {
        let source = AudioSource {
            id: "42".into(),
            name: "firefox.output".into(),
            description: "Firefox".into(),
            media_class: "Stream/Output/Audio".into(),
            application_name: Some("Firefox".into()),
            binary_name: Some("firefox".into()),
        };
        assert!(source.matches_configured("42"));
        assert!(source.matches_configured("Stream/Output/Audio:firefox.output"));
        assert!(source.matches_configured("Firefox"));
        assert!(!source.matches_configured("99"));
    }

    #[test]
    fn labels_pipewire_sources_by_capture_purpose() {
        let playback = AudioSource {
            id: "42".into(),
            name: "Playback".into(),
            description: "Mumble".into(),
            media_class: "Stream/Output/Audio".into(),
            application_name: Some("Mumble".into()),
            binary_name: None,
        };
        let capture = AudioSource {
            media_class: "Stream/Input/Audio".into(),
            ..playback.clone()
        };
        let mic = AudioSource {
            media_class: "Audio/Source".into(),
            description: "Jabra Link 380 Mono".into(),
            ..playback.clone()
        };
        let sink = AudioSource {
            media_class: "Audio/Sink".into(),
            description: "Jabra Link 380 Analog Stereo".into(),
            ..playback.clone()
        };

        assert_eq!(playback.capture_role(), "App playback");
        assert!(playback.recommendation().contains("other people"));
        assert_eq!(capture.capture_role(), "App mic input");
        assert!(capture.recommendation().contains("usually not needed"));
        assert_eq!(mic.capture_role(), "Microphone");
        assert_eq!(sink.capture_role(), "Speaker output");
        assert!(playback.sort_rank() < mic.sort_rank());
        assert!(sink.sort_rank() < capture.sort_rank());
    }

    #[test]
    fn live_capture_uses_wav_and_ogg_files_keep_ogg_format() {
        let mut config = test_config();
        assert_eq!(config.audio_format().codec, AudioCodec::WavPcm16);
        assert_eq!(config.audio_format().container, AudioContainer::Wav);
        assert_eq!(config.audio_format().sample_rate_hz, 16_000);

        config.audio_file = Some(PathBuf::from("sample.ogg"));
        assert_eq!(config.audio_format().codec, AudioCodec::Opus);
        assert_eq!(config.audio_format().container, AudioContainer::Ogg);
        assert_eq!(config.audio_format().sample_rate_hz, 48_000);
    }

    #[tokio::test]
    async fn file_input_is_preloaded_before_the_select_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        std::fs::write(&path, b"meeting audio").unwrap();
        let mut config = test_config();
        config.audio_file = Some(path.clone());
        let logs = Arc::new(RwLock::new(LogBuffer::new(10)));

        let mut input = AudioInput::open(&config, None, logs).await.unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(
            input.next_chunk().await.unwrap(),
            Some(b"meeting audio".to_vec())
        );
        assert_eq!(input.next_chunk().await.unwrap(), None);
    }

    #[test]
    fn config_defaults_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let config = ClientConfig::load(CliArgs {
            config: Some(dir.path().join("missing.toml")),
            server_url: None,
            output: None,
            recording_output: None,
            events_output: None,
            oidc_issuer: None,
            oidc_client_id: None,
            token: None,
            token_cache: None,
            disable_token_cache: None,
            insecure_no_auth: None,
            diarization: None,
            audio_file: None,
            language: None,
            source: Vec::new(),
            list_sources: false,
            chunk_seconds: None,
            capture_mode: None,
            auto_enable_new_streams: None,
            audio_rescan_seconds: None,
        })
        .unwrap();
        assert_eq!(config.server_url, "ws://127.0.0.1:8080/v1/sessions/ws");
        assert_eq!(config.output, PathBuf::from("transcript.md"));
        assert_eq!(
            config.events_output,
            PathBuf::from("transcript.events.jsonl")
        );
        assert_eq!(config.chunk_seconds, 15);
        assert_eq!(config.capture_mode, CaptureMode::Meeting);
        assert!(!config.auto_enable_new_streams);
        assert_eq!(config.audio_rescan_seconds, 2);
    }

    #[test]
    fn config_file_values_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");
        std::fs::write(
            &path,
            r#"
server_url = "wss://example.test/v1/sessions/ws"
output = "notes.md"
insecure_no_auth = true
diarization = "disable"
source = ["42"]
chunk_seconds = 30
auto_enable_new_streams = true
audio_rescan_seconds = 5
language = "de"
"#,
        )
        .unwrap();
        let config = ClientConfig::load(CliArgs {
            config: Some(path),
            server_url: None,
            output: None,
            recording_output: None,
            events_output: None,
            oidc_issuer: None,
            oidc_client_id: None,
            token: None,
            token_cache: None,
            disable_token_cache: None,
            insecure_no_auth: None,
            diarization: None,
            audio_file: None,
            language: None,
            source: Vec::new(),
            list_sources: false,
            chunk_seconds: None,
            capture_mode: None,
            auto_enable_new_streams: None,
            audio_rescan_seconds: None,
        })
        .unwrap();
        assert_eq!(config.server_url, "wss://example.test/v1/sessions/ws");
        assert_eq!(config.output, PathBuf::from("notes.md"));
        assert!(config.insecure_no_auth);
        assert_eq!(config.source, vec!["42"]);
        assert_eq!(config.chunk_seconds, 30);
        assert!(config.auto_enable_new_streams);
        assert_eq!(config.audio_rescan_seconds, 5);
        assert_eq!(config.language.as_deref(), Some("de"));
    }

    #[test]
    fn decodes_jwt_exp_claim() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"exp":12345}"#);
        assert_eq!(jwt_exp(&format!("header.{payload}.signature")), Some(12345));
    }

    fn test_config() -> ClientConfig {
        ClientConfig {
            server_url: "ws://127.0.0.1:8080/v1/sessions/ws".into(),
            output: PathBuf::from("transcript.md"),
            recording_output: None,
            events_output: PathBuf::from("transcript.events.jsonl"),
            oidc_issuer: None,
            oidc_client_id: None,
            token: None,
            token_cache: None,
            disable_token_cache: false,
            insecure_no_auth: true,
            diarization: DiarizationArg::Prefer,
            audio_file: None,
            language: None,
            source: Vec::new(),
            chunk_seconds: 15,
            capture_mode: CaptureMode::Meeting,
            auto_enable_new_streams: false,
            audio_rescan_seconds: 2,
        }
    }
}
