use std::{collections::BTreeSet, io, path::PathBuf, process::Stdio, time::Duration};

use anyhow::{bail, Context, Result};
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
use serde::Deserialize;
use tokio::{fs::OpenOptions, io::AsyncWriteExt, process::Command, time::sleep};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use tracing::{info, warn};
use whisper_relay_protocol::{
    AudioCodec, AudioContainer, AudioFormat, ClientHello, ClientMessage, DiarizationPreference,
    ServerMessage, TranscriptEvent, PROTOCOL_VERSION,
};

#[derive(Debug, Parser)]
struct CliArgs {
    #[arg(long, env = "WHISPER_RELAY_CONFIG")]
    config: Option<PathBuf>,

    #[arg(long, env = "WHISPER_RELAY_SERVER_URL")]
    server_url: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_OUTPUT")]
    output: Option<PathBuf>,

    #[arg(long, env = "WHISPER_RELAY_OIDC_ISSUER")]
    oidc_issuer: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_TOKEN")]
    token: Option<String>,

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

    #[arg(long)]
    source: Vec<String>,

    #[arg(long, default_value_t = false)]
    list_sources: bool,

    #[arg(long)]
    chunk_seconds: Option<u64>,
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
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    token: Option<String>,
    insecure_no_auth: Option<bool>,
    diarization: Option<DiarizationArg>,
    audio_file: Option<PathBuf>,
    #[serde(default)]
    source: Vec<String>,
    chunk_seconds: Option<u64>,
}

#[derive(Debug, Clone)]
struct ClientConfig {
    server_url: String,
    output: PathBuf,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    token: Option<String>,
    insecure_no_auth: bool,
    diarization: DiarizationArg,
    audio_file: Option<PathBuf>,
    source: Vec<String>,
    chunk_seconds: u64,
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
    error: Option<String>,
    error_description: Option<String>,
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
                "{}\t{}\t{}\t{}",
                source.id, source.media_class, source.name, source.description
            );
        }
        return Ok(());
    }
    let config = ClientConfig::load(args)?;

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
    let hello = ClientMessage::Hello(ClientHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: hostname(),
        diarization: config.diarization.clone().into(),
        audio: AudioFormat {
            codec: AudioCodec::Opus,
            container: AudioContainer::Ogg,
            sample_rate_hz: 16_000,
            channels: 1,
        },
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

    let mut audio = AudioInput::open(&config).await?;

    loop {
        tokio::select! {
            chunk = audio.next_chunk() => {
                let Some(chunk) = chunk? else {
                    ws.send(Message::Text(serde_json::to_string(&ClientMessage::AudioEnd)?.into())).await?;
                    break;
                };
                ws.send(Message::Binary(chunk.into())).await?;
            }
            message = ws.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => handle_server_message(&mut output, &text).await?,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err.into()),
                }
            }
        }
    }

    while let Some(message) = ws.next().await {
        match message? {
            Message::Text(text) => handle_server_message(&mut output, &text).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}

impl ClientConfig {
    fn load(args: CliArgs) -> Result<Self> {
        let file = load_file_config(args.config.as_ref())?;
        Ok(Self {
            server_url: args
                .server_url
                .or(file.server_url)
                .unwrap_or_else(|| "ws://127.0.0.1:8080/v1/sessions/ws".into()),
            output: expand_home(
                args.output
                    .or(file.output)
                    .unwrap_or_else(|| PathBuf::from("transcript.md")),
            ),
            oidc_issuer: args.oidc_issuer.or(file.oidc_issuer),
            oidc_client_id: args.oidc_client_id.or(file.oidc_client_id),
            token: args.token.or(file.token),
            insecure_no_auth: args
                .insecure_no_auth
                .or(file.insecure_no_auth)
                .unwrap_or(false),
            diarization: args
                .diarization
                .or(file.diarization)
                .unwrap_or(DiarizationArg::Prefer),
            audio_file: args.audio_file.or(file.audio_file).map(expand_home),
            source: if args.source.is_empty() {
                file.source
            } else {
                args.source
            },
            chunk_seconds: args.chunk_seconds.or(file.chunk_seconds).unwrap_or(15),
        })
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
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let discovery: OpenIdConfiguration = reqwest::get(&discovery_url)
        .await?
        .error_for_status()?
        .json()
        .await?;

    let http = reqwest::Client::new();
    let device: DeviceAuthorizationResponse = http
        .post(&discovery.device_authorization_endpoint)
        .form(&[
            ("client_id", client_id.as_str()),
            ("scope", "openid profile email"),
        ])
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
                ("client_id", client_id.as_str()),
            ])
            .send()
            .await?
            .json()
            .await?;
        if let Some(token) = response.access_token {
            return Ok(Some(token));
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

async fn handle_server_message(output: &mut tokio::fs::File, text: &str) -> Result<()> {
    match serde_json::from_str::<ServerMessage>(text)? {
        ServerMessage::SessionReady(ready) => {
            info!(session_id = %ready.session_id, chunk_seconds = ready.chunk_seconds, "session ready");
        }
        ServerMessage::TranscriptFinal(event) => append_transcript(output, &event).await?,
        ServerMessage::TranscriptPartial(_) => {}
        ServerMessage::Warning(warning) => warn!(code = warning.code, message = warning.message),
        ServerMessage::Error(error) => bail!("server error {}: {}", error.code, error.message),
        ServerMessage::Pong { .. } => {}
    }
    Ok(())
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
    File(Option<PathBuf>),
    ProcessChunks {
        _dir: tempfile::TempDir,
        location_pattern: String,
        next_index: u64,
        _child: tokio::process::Child,
    },
}

impl AudioInput {
    async fn open(config: &ClientConfig) -> Result<Self> {
        if let Some(path) = &config.audio_file {
            return Ok(Self::File(Some(path.clone())));
        }

        let sources = if config.source.is_empty() {
            select_sources_tui(discover_sources().await?)?
        } else {
            config.source.clone()
        };
        if sources.is_empty() {
            bail!("no PipeWire sources selected");
        }

        let dir = tempfile::tempdir()?;
        let location_pattern = dir.path().join("chunk-%05d.ogg").display().to_string();
        let child = Command::new("gst-launch-1.0")
            .args(gstreamer_args(
                &sources,
                &location_pattern,
                config.chunk_seconds,
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting gst-launch-1.0; install gstreamer and pipewire plugins")?;
        Ok(Self::ProcessChunks {
            _dir: dir,
            location_pattern,
            next_index: 0,
            _child: child,
        })
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::File(path) => {
                let Some(path) = path.take() else {
                    return Ok(None);
                };
                Ok(Some(tokio::fs::read(path).await?))
            }
            Self::ProcessChunks {
                location_pattern,
                next_index,
                ..
            } => {
                let path = chunk_path(location_pattern, *next_index);
                wait_until_complete(&path).await?;
                let bytes = tokio::fs::read(&path).await?;
                let _ = tokio::fs::remove_file(&path).await;
                *next_index += 1;
                Ok(Some(bytes))
            }
        }
    }
}

async fn wait_until_complete(path: &PathBuf) -> Result<()> {
    let mut last_size = None;
    let mut stable_ticks = 0_u8;
    loop {
        if let Ok(metadata) = tokio::fs::metadata(path).await {
            let size = metadata.len();
            if size > 0 && Some(size) == last_size {
                stable_ticks += 1;
                if stable_ticks >= 2 {
                    return Ok(());
                }
            } else {
                stable_ticks = 0;
                last_size = Some(size);
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
}

fn chunk_path(pattern: &str, index: u64) -> PathBuf {
    PathBuf::from(pattern.replace("%05d", &format!("{index:05}")))
}

fn select_sources_tui(sources: Vec<AudioSource>) -> Result<Vec<String>> {
    if sources.is_empty() {
        bail!("pw-dump returned no usable audio sources");
    }

    let mut terminal = TuiSession::enter()?;
    let mut selected = BTreeSet::new();
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    loop {
        terminal.draw(&sources, &selected, &mut list_state)?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => bail!("source selection cancelled"),
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut list_state, sources.len(), 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut list_state, sources.len(), -1),
            KeyCode::Char(' ') => {
                let idx = list_state.selected().unwrap_or(0);
                if !selected.insert(idx) {
                    selected.remove(&idx);
                }
            }
            KeyCode::Enter => {
                if selected.is_empty() {
                    let idx = list_state.selected().unwrap_or(0);
                    selected.insert(idx);
                }
                return Ok(selected
                    .into_iter()
                    .filter_map(|idx| sources.get(idx).map(|source| source.id.clone()))
                    .collect());
            }
            _ => {}
        }
    }
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
        sources: &[AudioSource],
        selected: &BTreeSet<usize>,
        list_state: &mut ListState,
    ) -> Result<()> {
        self.terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(8),
                    Constraint::Length(3),
                ])
                .split(frame.area());

            let header = Paragraph::new("Whisper Relay")
                .block(Block::default().borders(Borders::ALL).title("Client"));
            frame.render_widget(header, chunks[0]);

            let items = sources.iter().enumerate().map(|(idx, source)| {
                let mark = if selected.contains(&idx) {
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
                    Span::raw(format!("  {}  {}", source.media_class, source.name)),
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
            frame.render_stateful_widget(list, chunks[1], list_state);

            let help =
                Paragraph::new("Up/Down or j/k move  Space toggles  Enter starts  Esc/q cancels")
                    .wrap(Wrap { trim: true })
                    .block(Block::default().borders(Borders::ALL).title("Keys"));
            frame.render_widget(help, chunks[2]);
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
            let description = prop(&node.info.props, "node.description")
                .or_else(|| prop(&node.info.props, "application.name"))
                .unwrap_or_else(|| name.clone());
            Some(AudioSource {
                id: node.id.to_string(),
                name,
                description,
                media_class,
            })
        })
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| a.description.cmp(&b.description));
    Ok(sources)
}

fn prop(props: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    props.get(key)?.as_str().map(ToOwned::to_owned)
}

fn gstreamer_args(sources: &[String], location_pattern: &str, chunk_seconds: u64) -> Vec<String> {
    let mut args = vec![
        "-q".into(),
        "audiomixer".into(),
        "name=mixer".into(),
        "!".into(),
        "audioconvert".into(),
        "!".into(),
        "audioresample".into(),
        "!".into(),
        "audio/x-raw,rate=16000,channels=1".into(),
        "!".into(),
        "opusenc".into(),
        "audio-type=voice".into(),
        "!".into(),
        "splitmuxsink".into(),
        "muxer-factory=oggmux".into(),
        format!("location={location_pattern}"),
        format!("max-size-time={}", chunk_seconds * 1_000_000_000),
    ];

    for source in sources {
        args.extend([
            "pipewiresrc".into(),
            format!("target-object={source}"),
            "!".into(),
            "queue".into(),
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
    fn builds_gstreamer_pipeline_for_two_sources() {
        let args = gstreamer_args(&["10".into(), "11".into()], "/tmp/chunk-%05d.ogg", 15);
        assert!(args.contains(&"audiomixer".to_string()));
        assert!(args.contains(&"target-object=10".to_string()));
        assert!(args.contains(&"target-object=11".to_string()));
        assert!(args.contains(&"muxer-factory=oggmux".to_string()));
    }

    #[test]
    fn config_defaults_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let config = ClientConfig::load(CliArgs {
            config: Some(dir.path().join("missing.toml")),
            server_url: None,
            output: None,
            oidc_issuer: None,
            oidc_client_id: None,
            token: None,
            insecure_no_auth: None,
            diarization: None,
            audio_file: None,
            source: Vec::new(),
            list_sources: false,
            chunk_seconds: None,
        })
        .unwrap();
        assert_eq!(config.server_url, "ws://127.0.0.1:8080/v1/sessions/ws");
        assert_eq!(config.output, PathBuf::from("transcript.md"));
        assert_eq!(config.chunk_seconds, 15);
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
"#,
        )
        .unwrap();
        let config = ClientConfig::load(CliArgs {
            config: Some(path),
            server_url: None,
            output: None,
            oidc_issuer: None,
            oidc_client_id: None,
            token: None,
            insecure_no_auth: None,
            diarization: None,
            audio_file: None,
            source: Vec::new(),
            list_sources: false,
            chunk_seconds: None,
        })
        .unwrap();
        assert_eq!(config.server_url, "wss://example.test/v1/sessions/ws");
        assert_eq!(config.output, PathBuf::from("notes.md"));
        assert!(config.insecure_no_auth);
        assert_eq!(config.source, vec!["42"]);
        assert_eq!(config.chunk_seconds, 30);
    }
}
