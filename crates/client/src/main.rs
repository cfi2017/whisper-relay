use std::{
    io::{self, Write},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, ValueEnum};
use futures_util::{SinkExt, StreamExt};
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
struct Args {
    #[arg(
        long,
        env = "WHISPER_RELAY_SERVER_URL",
        default_value = "ws://127.0.0.1:8080/v1/sessions/ws"
    )]
    server_url: String,

    #[arg(long, env = "WHISPER_RELAY_OUTPUT", default_value = "transcript.md")]
    output: PathBuf,

    #[arg(long, env = "WHISPER_RELAY_OIDC_ISSUER")]
    oidc_issuer: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_TOKEN")]
    token: Option<String>,

    #[arg(long, default_value_t = false)]
    insecure_no_auth: bool,

    #[arg(long, value_enum, default_value_t = DiarizationArg::Prefer)]
    diarization: DiarizationArg,

    #[arg(long)]
    audio_file: Option<PathBuf>,

    #[arg(long)]
    source: Vec<String>,

    #[arg(long, default_value_t = false)]
    list_sources: bool,

    #[arg(long, default_value_t = 15)]
    chunk_seconds: u64,
}

#[derive(Debug, Clone, ValueEnum)]
enum DiarizationArg {
    Prefer,
    Require,
    Disable,
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

    let args = Args::parse();
    if args.list_sources {
        for source in discover_sources().await? {
            println!(
                "{}\t{}\t{}\t{}",
                source.id, source.media_class, source.name, source.description
            );
        }
        return Ok(());
    }

    let token = acquire_token(&args).await?;
    let mut request = args.server_url.clone().into_client_request()?;
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
        diarization: args.diarization.clone().into(),
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
        .open(&args.output)
        .await
        .with_context(|| format!("opening {}", args.output.display()))?;
    write_session_header(&mut output).await?;

    let mut audio = AudioInput::open(&args).await?;

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

async fn acquire_token(args: &Args) -> Result<Option<String>> {
    if args.insecure_no_auth {
        return Ok(None);
    }
    if let Some(token) = &args.token {
        return Ok(Some(token.clone()));
    }

    let issuer = args
        .oidc_issuer
        .as_ref()
        .context("--oidc-issuer or WHISPER_RELAY_OIDC_ISSUER is required unless --token or --insecure-no-auth is used")?;
    let client_id = args
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
    async fn open(args: &Args) -> Result<Self> {
        if let Some(path) = &args.audio_file {
            return Ok(Self::File(Some(path.clone())));
        }

        let sources = if args.source.is_empty() {
            prompt_sources().await?
        } else {
            args.source.clone()
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
                args.chunk_seconds,
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

async fn prompt_sources() -> Result<Vec<String>> {
    let sources = discover_sources().await?;
    if sources.is_empty() {
        bail!("pw-dump returned no usable audio sources");
    }
    for (idx, source) in sources.iter().enumerate() {
        println!(
            "{idx}: {} [{}] {}",
            source.description, source.media_class, source.name
        );
    }
    print!("Select source numbers separated by comma: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    line.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            let idx: usize = value.parse()?;
            sources
                .get(idx)
                .map(|source| source.id.clone())
                .context("selected source index out of range")
        })
        .collect()
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
}
