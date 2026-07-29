use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use chrono::Utc;
use clap::Parser;
use futures_util::{stream, StreamExt};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, DecodingKey, Validation};
use reqwest::multipart;
use serde::Deserialize;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use whisper_relay_protocol::{
    AudioCodec, AudioContainer, AudioFormat, ClientHello, ClientMessage, DiarizationPreference,
    DiarizationStatus, ErrorMessage, ServerMessage, SessionReady, TranscriptEvent, WarningMessage,
    PROTOCOL_VERSION,
};

#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    #[arg(long, env = "WHISPER_RELAY_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,

    #[arg(long, env = "WHISPER_RELAY_OIDC_ISSUER")]
    oidc_issuer: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_OIDC_AUDIENCE")]
    oidc_audience: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_INSECURE_NO_AUTH", default_value_t = false)]
    insecure_no_auth: bool,

    #[arg(long, env = "WHISPER_RELAY_TRANSCRIPTION_BASE_URL")]
    transcription_base_url: String,

    #[arg(long, env = "WHISPER_RELAY_TRANSCRIPTION_API_KEY")]
    transcription_api_key: Option<String>,

    #[arg(
        long,
        env = "WHISPER_RELAY_TRANSCRIPTION_MODEL",
        default_value = "whisper"
    )]
    transcription_model: String,

    #[arg(long, env = "WHISPER_RELAY_DIARIZATION_BASE_URL")]
    diarization_base_url: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_DIARIZATION_API_KEY")]
    diarization_api_key: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_DIARIZATION_MODEL")]
    diarization_model: Option<String>,

    #[arg(
        long,
        env = "WHISPER_RELAY_DIARIZATION_RESPONSE_FORMAT",
        default_value = "verbose_json"
    )]
    diarization_response_format: String,

    #[arg(long, env = "WHISPER_RELAY_LANGUAGE")]
    language: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_PROMPT")]
    prompt: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_CHUNK_SECONDS", default_value_t = 15)]
    chunk_seconds: u64,

    #[arg(
        long,
        env = "WHISPER_RELAY_TRANSCRIPTION_TIMEOUT_SECONDS",
        default_value_t = 3600
    )]
    transcription_timeout_seconds: u64,

    #[arg(long, env = "WHISPER_RELAY_MAX_AUDIO_MIB", default_value_t = 512)]
    max_audio_mib: usize,

    #[arg(long, env = "WHISPER_RELAY_SMART_CHUNKING", default_value_t = true)]
    smart_chunking: bool,

    #[arg(long, env = "WHISPER_RELAY_ASR_CONCURRENCY", default_value_t = 4)]
    asr_concurrency: usize,

    #[arg(long, env = "WHISPER_RELAY_TARGET_CHUNK_SECONDS", default_value_t = 25)]
    target_chunk_seconds: u64,

    #[arg(long, env = "WHISPER_RELAY_MAX_CHUNK_SECONDS", default_value_t = 28)]
    max_chunk_seconds: u64,

    #[arg(
        long,
        env = "WHISPER_RELAY_BACKEND_DIARIZATION",
        default_value_t = false
    )]
    backend_diarization: bool,
}

#[derive(Clone)]
struct AppState {
    auth: AuthConfig,
    transcription: TranscriptionClient,
    diarization: Option<TranscriptionClient>,
    chunk_seconds: u64,
    backend_diarization: bool,
    max_audio_bytes: usize,
    smart_chunking: SmartChunkConfig,
}

#[derive(Clone)]
struct SmartChunkConfig {
    enabled: bool,
    concurrency: usize,
    target_seconds: u64,
    max_seconds: u64,
}

#[derive(Clone)]
enum AuthConfig {
    Disabled,
    Oidc {
        issuer: String,
        audience: String,
        jwks: Arc<JwkSet>,
    },
}

#[derive(Clone)]
struct TranscriptionClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    language: Option<String>,
    timeout: Duration,
    prompt: Option<String>,
    diarization_response_format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenIdConfiguration {
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[allow(dead_code)]
    sub: String,
    #[allow(dead_code)]
    exp: usize,
    #[allow(dead_code)]
    iss: String,
    #[serde(default)]
    #[allow(dead_code)]
    aud: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: Option<String>,
    segments: Option<Vec<TranscriptionSegment>>,
}

#[derive(Debug, Deserialize)]
struct TranscriptionSegment {
    text: String,
    start: Option<f64>,
    end: Option<f64>,
    speaker: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "whisper_relay_server=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();
    let diarization_api_key = args
        .diarization_api_key
        .clone()
        .or_else(|| args.transcription_api_key.clone());
    let diarization_auth_inherited = args.diarization_api_key.is_none()
        && args.transcription_api_key.is_some()
        && args.diarization_base_url.is_some();
    let state = Arc::new(AppState {
        auth: build_auth(&args).await?,
        transcription: TranscriptionClient {
            http: reqwest::Client::new(),
            base_url: args
                .transcription_base_url
                .trim_end_matches('/')
                .to_string(),
            api_key: args.transcription_api_key.clone(),
            model: args.transcription_model.clone(),
            language: args.language.clone(),
            timeout: Duration::from_secs(args.transcription_timeout_seconds),
            prompt: args.prompt.clone(),
            diarization_response_format: None,
        },
        diarization: args
            .diarization_base_url
            .as_ref()
            .map(|base_url| TranscriptionClient {
                http: reqwest::Client::new(),
                base_url: base_url.trim_end_matches('/').to_string(),
                api_key: diarization_api_key.clone(),
                model: args
                    .diarization_model
                    .clone()
                    .unwrap_or_else(|| "whisper-diarized".into()),
                language: args.language.clone(),
                timeout: Duration::from_secs(args.transcription_timeout_seconds),
                prompt: args.prompt.clone(),
                diarization_response_format: Some(args.diarization_response_format.clone()),
            }),
        chunk_seconds: args.chunk_seconds,
        backend_diarization: args.backend_diarization,
        max_audio_bytes: args.max_audio_mib * 1024 * 1024,
        smart_chunking: SmartChunkConfig {
            enabled: args.smart_chunking,
            concurrency: args.asr_concurrency.max(1),
            target_seconds: args.target_chunk_seconds.max(5),
            max_seconds: args.max_chunk_seconds.max(args.target_chunk_seconds).max(5),
        },
    });

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/sessions/ws", get(ws_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(args.bind).await?;
    info!(
        addr = %args.bind,
        version = env!("CARGO_PKG_VERSION"),
        protocol_version = PROTOCOL_VERSION,
        transcription_auth = args.transcription_api_key.is_some(),
        diarization_auth = args.diarization_base_url.is_some() && diarization_api_key.is_some(),
        diarization_auth_inherited,
        "listening"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn build_auth(args: &Args) -> Result<AuthConfig> {
    if args.insecure_no_auth {
        warn!("authentication disabled by WHISPER_RELAY_INSECURE_NO_AUTH");
        return Ok(AuthConfig::Disabled);
    }

    let issuer = args
        .oidc_issuer
        .clone()
        .context("WHISPER_RELAY_OIDC_ISSUER is required unless insecure auth is enabled")?;
    let audience = args
        .oidc_audience
        .clone()
        .context("WHISPER_RELAY_OIDC_AUDIENCE is required unless insecure auth is enabled")?;
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let discovery: OpenIdConfiguration = reqwest::get(&discovery_url)
        .await
        .with_context(|| format!("fetching OIDC discovery document from {discovery_url}"))?
        .error_for_status()?
        .json()
        .await?;
    let jwks: JwkSet = reqwest::get(&discovery.jwks_uri)
        .await
        .with_context(|| format!("fetching JWKS from {}", discovery.jwks_uri))?
        .error_for_status()?
        .json()
        .await?;

    Ok(AuthConfig::Oidc {
        issuer,
        audience,
        jwks: Arc::new(jwks),
    })
}

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if let Err(err) = authorize(&state.auth, &headers) {
        warn!(%err, "websocket authorization rejected");
        return (StatusCode::UNAUTHORIZED, err).into_response();
    }

    ws.max_message_size(state.max_audio_bytes)
        .max_frame_size(state.max_audio_bytes)
        .on_upgrade(move |socket| async move {
            if let Err(err) = handle_socket(state, socket).await {
                error!(%err, "session failed");
            }
        })
        .into_response()
}

fn authorize(auth: &AuthConfig, headers: &HeaderMap) -> std::result::Result<(), String> {
    let AuthConfig::Oidc {
        issuer,
        audience,
        jwks,
    } = auth
    else {
        return Ok(());
    };

    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "missing Authorization header".to_string())?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| "Authorization must be Bearer token".to_string())?;
    let header = decode_header(token).map_err(|e| format!("invalid token header: {e}"))?;
    let kid = header
        .kid
        .ok_or_else(|| "token header missing kid".to_string())?;
    let jwk = jwks
        .find(&kid)
        .ok_or_else(|| "token kid not found in JWKS".to_string())?;
    let key = DecodingKey::from_jwk(jwk).map_err(|e| format!("invalid jwk: {e}"))?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    decode::<Claims>(token, &key, &validation).map_err(|e| format!("invalid token: {e}"))?;
    Ok(())
}

async fn handle_socket(state: Arc<AppState>, mut socket: WebSocket) -> Result<()> {
    let session_id = Uuid::new_v4();
    let Some(Ok(Message::Text(text))) = socket.next().await else {
        send_error(
            &mut socket,
            "expected_hello",
            "first message must be hello JSON",
        )
        .await?;
        return Ok(());
    };
    let hello: ClientMessage = serde_json::from_str(&text)?;
    let ClientMessage::Hello(hello) = hello else {
        send_error(&mut socket, "expected_hello", "first message must be hello").await?;
        return Ok(());
    };
    validate_hello(&hello, &mut socket).await?;
    info!(
        session_id = %session_id,
        protocol_version = hello.protocol_version,
        buffered_upload = hello.buffer_audio_until_end,
        codec = ?hello.audio.codec,
        container = ?hello.audio.container,
        "session accepted"
    );

    let diarization = if state.backend_diarization {
        DiarizationStatus::Enabled
    } else if hello.diarization == DiarizationPreference::Require {
        send_error(
            &mut socket,
            "diarization_unsupported",
            "backend diarization is not enabled on this server",
        )
        .await?;
        return Ok(());
    } else {
        DiarizationStatus::Unsupported
    };

    send_json(
        &mut socket,
        &ServerMessage::SessionReady(SessionReady {
            session_id,
            chunk_seconds: state.chunk_seconds,
            diarization: diarization.clone(),
        }),
    )
    .await?;
    if diarization == DiarizationStatus::Unsupported
        && hello.diarization == DiarizationPreference::Prefer
    {
        send_json(
            &mut socket,
            &ServerMessage::Warning(WarningMessage {
                code: "diarization_unsupported".into(),
                message:
                    "backend diarization is disabled; transcripts will not include speaker labels"
                        .into(),
            }),
        )
        .await?;
    }

    let mut sequence = 0_u64;
    let mut buffered_audio = Vec::new();
    while let Some(message) = socket.next().await {
        match message? {
            Message::Binary(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                if hello.buffer_audio_until_end {
                    if buffered_audio.len().saturating_add(bytes.len()) > state.max_audio_bytes {
                        send_error(
                            &mut socket,
                            "audio_too_large",
                            "buffered audio exceeds the configured server limit",
                        )
                        .await?;
                        break;
                    }
                    buffered_audio.extend_from_slice(&bytes);
                    debug!(session_id = %session_id, frame_bytes = bytes.len(), total_bytes = buffered_audio.len(), "buffered meeting frame");
                } else {
                    sequence += 1;
                    transcribe_and_send(
                        &state,
                        &hello,
                        &mut socket,
                        session_id,
                        sequence,
                        bytes.to_vec(),
                    )
                    .await?;
                }
            }
            Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text)? {
                ClientMessage::AudioEnd => {
                    if hello.buffer_audio_until_end && !buffered_audio.is_empty() {
                        sequence += 1;
                        info!(session_id = %session_id, bytes = buffered_audio.len(), "meeting upload complete");
                        transcribe_and_send(
                            &state,
                            &hello,
                            &mut socket,
                            session_id,
                            sequence,
                            std::mem::take(&mut buffered_audio),
                        )
                        .await?;
                    }
                    break;
                }
                ClientMessage::Ping { nonce } => {
                    send_json(&mut socket, &ServerMessage::Pong { nonce }).await?
                }
                ClientMessage::Hello(_) => {
                    send_json(
                        &mut socket,
                        &ServerMessage::Warning(WarningMessage {
                            code: "duplicate_hello".into(),
                            message: "hello was already received for this session".into(),
                        }),
                    )
                    .await?;
                }
            },
            Message::Close(_) => break,
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Pong(_) => {}
        }
    }

    Ok(())
}

async fn transcribe_and_send(
    state: &AppState,
    hello: &ClientHello,
    socket: &mut WebSocket,
    session_id: Uuid,
    sequence: u64,
    bytes: Vec<u8>,
) -> Result<()> {
    debug!(session_id = %session_id, sequence, bytes = bytes.len(), "transcribing audio");
    let transcription = if state.backend_diarization {
        state.diarization.as_ref().unwrap_or(&state.transcription)
    } else {
        &state.transcription
    };
    let transcription = transcribe_audio(
        transcription,
        bytes,
        state.backend_diarization,
        &hello.audio,
        hello.language.as_deref(),
        &state.smart_chunking,
    );
    tokio::pin!(transcription);
    let mut keepalive = tokio::time::interval(Duration::from_secs(15));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;
    let result = loop {
        tokio::select! {
            result = &mut transcription => break result,
            _ = keepalive.tick() => socket.send(Message::Ping(Vec::new().into())).await?,
        }
    };
    match result {
        Ok(events) => {
            if events.is_empty() {
                send_json(
                    socket,
                    &ServerMessage::Warning(WarningMessage {
                        code: "no_transcript".into(),
                        message: "no speech or transcript text was detected".into(),
                    }),
                )
                .await?;
            }
            for event in events {
                send_json(
                    socket,
                    &ServerMessage::TranscriptFinal(TranscriptEvent {
                        session_id,
                        sequence,
                        received_at: Utc::now(),
                        start_ms: event.start_ms,
                        end_ms: event.end_ms,
                        speaker: event.speaker,
                        text: event.text,
                    }),
                )
                .await?;
            }
        }
        Err(err) => {
            send_json(
                socket,
                &ServerMessage::Error(ErrorMessage {
                    code: "transcription_failed".into(),
                    message: err.to_string(),
                }),
            )
            .await?;
        }
    }
    Ok(())
}

async fn validate_hello(hello: &ClientHello, socket: &mut WebSocket) -> Result<()> {
    if !(1..=PROTOCOL_VERSION).contains(&hello.protocol_version) {
        let message = format!(
            "unsupported protocol version {}; server supports versions 1 through {}",
            hello.protocol_version, PROTOCOL_VERSION
        );
        send_error(socket, "protocol_version", &message).await?;
        bail!(message);
    }
    if hello.buffer_audio_until_end && hello.protocol_version < 2 {
        let message = "buffered meeting uploads require protocol version 2";
        send_error(socket, "protocol_version", message).await?;
        bail!(message);
    }
    Ok(())
}

#[derive(Debug)]
struct NormalizedTranscript {
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    speaker: Option<String>,
    text: String,
}

#[derive(Debug)]
struct AudioChunk {
    index: usize,
    start_ms: u64,
    end_ms: u64,
    overlap_before: bool,
    wav: Vec<u8>,
}

#[derive(Debug)]
struct TranscribedChunk {
    index: usize,
    overlap_before: bool,
    events: Vec<NormalizedTranscript>,
}

struct PcmWav<'a> {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    data: &'a [u8],
}

async fn transcribe_audio(
    client: &TranscriptionClient,
    bytes: Vec<u8>,
    diarization: bool,
    audio_format: &AudioFormat,
    language: Option<&str>,
    config: &SmartChunkConfig,
) -> Result<Vec<NormalizedTranscript>> {
    if !config.enabled || diarization || audio_format.container != AudioContainer::Wav {
        return client
            .transcribe(bytes, diarization, audio_format, language)
            .await;
    }

    let wav = match parse_pcm_wav(&bytes) {
        Ok(wav) if wav.bits_per_sample == 16 => wav,
        Ok(_) => {
            warn!("smart chunking supports only 16-bit PCM; forwarding whole audio");
            return client
                .transcribe(bytes, false, audio_format, language)
                .await;
        }
        Err(err) => {
            warn!(%err, "could not parse WAV for smart chunking; forwarding whole audio");
            return client
                .transcribe(bytes, false, audio_format, language)
                .await;
        }
    };
    let chunks = vad_chunks(&wav, config)?;
    if chunks.is_empty() {
        info!("VAD found no speech; skipping transcription request");
        return Ok(Vec::new());
    }
    info!(
        chunks = chunks.len(),
        concurrency = config.concurrency,
        "transcribing VAD-segmented meeting"
    );

    let format = AudioFormat {
        codec: AudioCodec::WavPcm16,
        container: AudioContainer::Wav,
        sample_rate_hz: wav.sample_rate,
        channels: wav.channels as u8,
    };
    let mut results = stream::iter(chunks.into_iter().map(|chunk| {
        let format = format.clone();
        async move {
            let mut events = client
                .transcribe(chunk.wav, false, &format, language)
                .await?;
            for event in &mut events {
                event.start_ms = Some(
                    event
                        .start_ms
                        .map(|value| value + chunk.start_ms)
                        .unwrap_or(chunk.start_ms),
                );
                event.end_ms = Some(
                    event
                        .end_ms
                        .map(|value| value + chunk.start_ms)
                        .unwrap_or(chunk.end_ms),
                );
            }
            Ok::<_, anyhow::Error>(TranscribedChunk {
                index: chunk.index,
                overlap_before: chunk.overlap_before,
                events,
            })
        }
    }))
    .buffer_unordered(config.concurrency)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    results.sort_by_key(|chunk| chunk.index);
    Ok(reconcile_chunks(results))
}

fn parse_pcm_wav(bytes: &[u8]) -> Result<PcmWav<'_>> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }
    let mut offset = 12;
    let mut format = None;
    let mut data = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        let start = offset + 8;
        let end = start.checked_add(size).context("WAV chunk size overflow")?;
        if end > bytes.len() {
            bail!("truncated WAV chunk");
        }
        if id == b"fmt " && size >= 16 {
            let codec = u16::from_le_bytes(bytes[start..start + 2].try_into()?);
            let channels = u16::from_le_bytes(bytes[start + 2..start + 4].try_into()?);
            let sample_rate = u32::from_le_bytes(bytes[start + 4..start + 8].try_into()?);
            let bits = u16::from_le_bytes(bytes[start + 14..start + 16].try_into()?);
            if codec != 1 || channels == 0 || sample_rate == 0 {
                bail!("unsupported WAV encoding");
            }
            format = Some((sample_rate, channels, bits));
        } else if id == b"data" {
            data = Some(&bytes[start..end]);
        }
        offset = end + (size % 2);
    }
    let (sample_rate, channels, bits_per_sample) = format.context("WAV has no fmt chunk")?;
    Ok(PcmWav {
        sample_rate,
        channels,
        bits_per_sample,
        data: data.context("WAV has no data chunk")?,
    })
}

fn vad_chunks(wav: &PcmWav<'_>, config: &SmartChunkConfig) -> Result<Vec<AudioChunk>> {
    let bytes_per_frame = wav.channels as usize * 2;
    if wav.data.len() < bytes_per_frame {
        return Ok(Vec::new());
    }
    let frame_samples = (wav.sample_rate as usize / 50).max(1);
    let frame_bytes = frame_samples * bytes_per_frame;
    let levels = wav
        .data
        .chunks(frame_bytes)
        .map(rms_pcm16)
        .collect::<Vec<_>>();
    let mut sorted = levels.clone();
    sorted.sort_unstable();
    let noise = sorted[sorted.len() / 5];
    let speech_level = sorted[sorted.len() * 9 / 10];
    let threshold = 120_u32.max(noise.saturating_mul(3).min(speech_level / 2));
    let speech = levels
        .iter()
        .map(|level| *level >= threshold)
        .collect::<Vec<_>>();
    let regions = speech_regions(&speech, 10, 35, 13);
    let sample_ranges = pack_regions(
        &regions,
        frame_samples,
        wav.sample_rate as usize,
        config.target_seconds as usize,
        config.max_seconds as usize,
    );

    sample_ranges
        .into_iter()
        .enumerate()
        .map(|(index, (start_sample, end_sample, overlap_before))| {
            let start_byte = start_sample * bytes_per_frame;
            let end_byte = (end_sample * bytes_per_frame).min(wav.data.len());
            Ok(AudioChunk {
                index,
                start_ms: start_sample as u64 * 1000 / wav.sample_rate as u64,
                end_ms: end_sample as u64 * 1000 / wav.sample_rate as u64,
                overlap_before,
                wav: pcm_to_wav(
                    &wav.data[start_byte..end_byte],
                    wav.sample_rate,
                    wav.channels,
                )?,
            })
        })
        .collect()
}

fn rms_pcm16(bytes: &[u8]) -> u32 {
    let mut sum = 0_u64;
    let mut count = 0_u64;
    for sample in bytes.chunks_exact(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as i64;
        sum = sum.saturating_add((value * value) as u64);
        count += 1;
    }
    sum.checked_div(count)
        .map(|mean| (mean as f64).sqrt() as u32)
        .unwrap_or(0)
}

fn speech_regions(
    speech: &[bool],
    min_speech_frames: usize,
    merge_gap_frames: usize,
    padding_frames: usize,
) -> Vec<(usize, usize)> {
    let mut raw = Vec::new();
    let mut start = None;
    for (index, active) in speech
        .iter()
        .copied()
        .chain(std::iter::once(false))
        .enumerate()
    {
        match (start, active) {
            (None, true) => start = Some(index),
            (Some(begin), false) => {
                if index - begin >= min_speech_frames {
                    raw.push((
                        begin.saturating_sub(padding_frames),
                        (index + padding_frames).min(speech.len()),
                    ));
                }
                start = None;
            }
            _ => {}
        }
    }
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for region in raw {
        if let Some(last) = merged.last_mut() {
            if region.0 <= last.1 + merge_gap_frames {
                last.1 = last.1.max(region.1);
                continue;
            }
        }
        merged.push(region);
    }
    merged
}

fn pack_regions(
    regions: &[(usize, usize)],
    frame_samples: usize,
    sample_rate: usize,
    target_seconds: usize,
    max_seconds: usize,
) -> Vec<(usize, usize, bool)> {
    let target = target_seconds * sample_rate;
    let hard_max = max_seconds * sample_rate;
    let overlap = sample_rate;
    let mut chunks = Vec::new();
    let mut pending: Option<(usize, usize, bool)> = None;
    for &(start_frame, end_frame) in regions {
        let mut start = start_frame * frame_samples;
        let end = end_frame * frame_samples;
        let mut overlap_before = false;
        if let Some((pending_start, pending_end, pending_overlap)) = pending.take() {
            if end - pending_start <= target {
                start = pending_start;
                overlap_before = pending_overlap;
            } else {
                chunks.push((pending_start, pending_end, pending_overlap));
            }
        }
        while end.saturating_sub(start) > hard_max {
            let split = start + hard_max;
            chunks.push((start, split, overlap_before));
            start = split.saturating_sub(overlap);
            overlap_before = true;
        }
        pending = Some((start, end, overlap_before));
    }
    if let Some(region) = pending {
        chunks.push(region);
    }
    chunks
}

fn pcm_to_wav(pcm: &[u8], sample_rate: u32, channels: u16) -> Result<Vec<u8>> {
    let data_len = u32::try_from(pcm.len()).context("WAV chunk exceeds RIFF size")?;
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

fn reconcile_chunks(chunks: Vec<TranscribedChunk>) -> Vec<NormalizedTranscript> {
    let mut output: Vec<NormalizedTranscript> = Vec::new();
    for mut chunk in chunks {
        if chunk.overlap_before {
            if let (Some(previous), Some(first)) = (output.last(), chunk.events.first_mut()) {
                first.text = remove_repeated_prefix(&previous.text, &first.text);
            }
        }
        output.extend(
            chunk
                .events
                .into_iter()
                .filter(|event| !event.text.trim().is_empty()),
        );
    }
    output
}

fn remove_repeated_prefix(previous: &str, current: &str) -> String {
    let previous_words = previous.split_whitespace().collect::<Vec<_>>();
    let current_words = current.split_whitespace().collect::<Vec<_>>();
    let max = previous_words.len().min(current_words.len()).min(16);
    for count in (2..=max).rev() {
        let left = &previous_words[previous_words.len() - count..];
        let right = &current_words[..count];
        if left
            .iter()
            .zip(right)
            .all(|(a, b)| normalize_word(a) == normalize_word(b))
        {
            return current_words[count..].join(" ");
        }
    }
    current.to_string()
}

fn normalize_word(word: &str) -> String {
    word.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

impl TranscriptionClient {
    async fn transcribe(
        &self,
        bytes: Vec<u8>,
        diarization: bool,
        audio_format: &AudioFormat,
        language: Option<&str>,
    ) -> Result<Vec<NormalizedTranscript>> {
        let url = format!("{}/v1/audio/transcriptions", self.base_url);
        let (file_name, mime_type) = audio_part_metadata(audio_format);
        let part = multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(mime_type)?;
        let mut form = multipart::Form::new()
            .text("model", self.model.clone())
            .part("file", part);
        if let Some(language) = language.or(self.language.as_deref()) {
            form = form.text("language", language.to_string());
        }
        if let Some(prompt) = &self.prompt {
            form = form.text("prompt", prompt.clone());
        }
        if diarization {
            let response_format = self
                .diarization_response_format
                .as_deref()
                .unwrap_or("verbose_json");
            form = form.text("response_format", response_format.to_string());
            if response_format == "diarized_json" {
                form = form.text("chunking_strategy", "auto");
            }
        } else {
            form = form.text("response_format", "json");
        }

        let mut request = self.http.post(url).multipart(form);
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }

        let response = request.timeout(self.timeout).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            anyhow::bail!("transcription backend returned {status}: {body}");
        }

        normalize_transcription_response(&body)
    }
}

fn audio_part_metadata(audio_format: &AudioFormat) -> (&'static str, &'static str) {
    match (&audio_format.codec, &audio_format.container) {
        (AudioCodec::WavPcm16, AudioContainer::Wav) => ("chunk.wav", "audio/wav"),
        (AudioCodec::Opus, AudioContainer::Ogg) => ("chunk.ogg", "audio/ogg"),
        _ => ("chunk.audio", "application/octet-stream"),
    }
}

fn normalize_transcription_response(body: &str) -> Result<Vec<NormalizedTranscript>> {
    let parsed: TranscriptionResponse = serde_json::from_str(body)
        .with_context(|| format!("transcription response was not recognized JSON: {body}"))?;
    if let Some(segments) = parsed.segments {
        let events = segments
            .into_iter()
            .filter(|segment| !segment.text.trim().is_empty())
            .map(|segment| NormalizedTranscript {
                start_ms: segment.start.map(seconds_to_ms),
                end_ms: segment.end.map(seconds_to_ms),
                speaker: segment.speaker,
                text: segment.text.trim().to_string(),
            })
            .collect();
        return Ok(events);
    }
    let text = parsed.text.unwrap_or_default().trim().to_string();
    if text.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![NormalizedTranscript {
            start_ms: None,
            end_ms: None,
            speaker: None,
            text,
        }])
    }
}

fn seconds_to_ms(value: f64) -> u64 {
    (value * 1000.0).max(0.0).round() as u64
}

async fn send_json(socket: &mut WebSocket, message: &ServerMessage) -> Result<()> {
    socket
        .send(Message::Text(serde_json::to_string(message)?.into()))
        .await?;
    Ok(())
}

async fn send_error(socket: &mut WebSocket, code: &str, message: &str) -> Result<()> {
    send_json(
        socket,
        &ServerMessage::Error(ErrorMessage {
            code: code.into(),
            message: message.into(),
        }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_vllm_diarization_response_format() {
        let args = Args::try_parse_from([
            "whisper-relay-server",
            "--transcription-base-url",
            "http://localhost:4000",
        ])
        .unwrap();
        assert_eq!(args.diarization_response_format, "verbose_json");
    }

    #[test]
    fn normalizes_plain_text_response() {
        let events = normalize_transcription_response(r#"{"text":" hello world "}"#).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "hello world");
    }

    #[test]
    fn normalizes_diarized_segments() {
        let events = normalize_transcription_response(
            r#"{"segments":[{"text":" hi ","speaker":"speaker_0","start":1.2,"end":2.4}]}"#,
        )
        .unwrap();
        assert_eq!(events[0].speaker.as_deref(), Some("speaker_0"));
        assert_eq!(events[0].start_ms, Some(1200));
        assert_eq!(events[0].end_ms, Some(2400));
        assert_eq!(events[0].text, "hi");
    }

    #[test]
    fn maps_audio_formats_to_multipart_metadata() {
        assert_eq!(
            audio_part_metadata(&AudioFormat {
                codec: AudioCodec::WavPcm16,
                container: AudioContainer::Wav,
                sample_rate_hz: 16_000,
                channels: 1,
            }),
            ("chunk.wav", "audio/wav")
        );
        assert_eq!(
            audio_part_metadata(&AudioFormat {
                codec: AudioCodec::Opus,
                container: AudioContainer::Ogg,
                sample_rate_hz: 48_000,
                channels: 1,
            }),
            ("chunk.ogg", "audio/ogg")
        );
    }

    #[test]
    fn vad_skips_silence_and_splits_long_speech_with_overlap() {
        let sample_rate = 16_000_u32;
        let mut pcm = vec![0_u8; sample_rate as usize * 2];
        for _ in 0..(sample_rate as usize * 40) {
            pcm.extend_from_slice(&2_000_i16.to_le_bytes());
        }
        pcm.resize(pcm.len() + sample_rate as usize * 2, 0);
        let bytes = pcm_to_wav(&pcm, sample_rate, 1).unwrap();
        let wav = parse_pcm_wav(&bytes).unwrap();
        let chunks = vad_chunks(
            &wav,
            &SmartChunkConfig {
                enabled: true,
                concurrency: 2,
                target_seconds: 25,
                max_seconds: 28,
            },
        )
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert!(!chunks[0].overlap_before);
        assert!(chunks[1].overlap_before);
        assert!(chunks[0].start_ms < 1_000);
        assert!(chunks[1].start_ms < chunks[0].end_ms);
        assert!(chunks[1].end_ms > 40_000);
        assert!(chunks.iter().all(|chunk| parse_pcm_wav(&chunk.wav).is_ok()));
    }

    #[test]
    fn vad_does_not_submit_silence() {
        let bytes = pcm_to_wav(&vec![0; 16_000 * 2 * 10], 16_000, 1).unwrap();
        let wav = parse_pcm_wav(&bytes).unwrap();
        let chunks = vad_chunks(
            &wav,
            &SmartChunkConfig {
                enabled: true,
                concurrency: 1,
                target_seconds: 25,
                max_seconds: 28,
            },
        )
        .unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn removes_only_repeated_overlap_prefix() {
        assert_eq!(
            remove_repeated_prefix(
                "we should deploy this next week",
                "this next week after the review"
            ),
            "after the review"
        );
        assert_eq!(
            remove_repeated_prefix("the first topic", "the second topic"),
            "the second topic"
        );
    }
}
