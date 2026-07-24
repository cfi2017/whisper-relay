use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
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
use futures_util::StreamExt;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, DecodingKey, Validation};
use reqwest::multipart;
use serde::Deserialize;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use whisper_relay_protocol::{
    ClientHello, ClientMessage, DiarizationPreference, DiarizationStatus, ErrorMessage,
    ServerMessage, SessionReady, TranscriptEvent, WarningMessage, PROTOCOL_VERSION,
};

#[derive(Debug, Parser)]
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

    #[arg(long, env = "WHISPER_RELAY_LANGUAGE")]
    language: Option<String>,

    #[arg(long, env = "WHISPER_RELAY_CHUNK_SECONDS", default_value_t = 15)]
    chunk_seconds: u64,

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
        },
        diarization: args
            .diarization_base_url
            .as_ref()
            .map(|base_url| TranscriptionClient {
                http: reqwest::Client::new(),
                base_url: base_url.trim_end_matches('/').to_string(),
                api_key: args.diarization_api_key.clone(),
                model: args
                    .diarization_model
                    .clone()
                    .unwrap_or_else(|| "whisper-diarized".into()),
                language: args.language.clone(),
            }),
        chunk_seconds: args.chunk_seconds,
        backend_diarization: args.backend_diarization,
    });

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/sessions/ws", get(ws_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(args.bind).await?;
    info!(addr = %args.bind, "listening");
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

    ws.on_upgrade(move |socket| async move {
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
    while let Some(message) = socket.next().await {
        match message? {
            Message::Binary(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                sequence += 1;
                debug!(session_id = %session_id, sequence, bytes = bytes.len(), "transcribing chunk");
                let transcription = if state.backend_diarization {
                    state.diarization.as_ref().unwrap_or(&state.transcription)
                } else {
                    &state.transcription
                };
                match transcription
                    .transcribe(bytes.to_vec(), state.backend_diarization)
                    .await
                {
                    Ok(events) => {
                        for event in events {
                            send_json(
                                &mut socket,
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
                            &mut socket,
                            &ServerMessage::Error(ErrorMessage {
                                code: "transcription_failed".into(),
                                message: err.to_string(),
                            }),
                        )
                        .await?;
                    }
                }
            }
            Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text)? {
                ClientMessage::AudioEnd => break,
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

async fn validate_hello(hello: &ClientHello, socket: &mut WebSocket) -> Result<()> {
    if hello.protocol_version != PROTOCOL_VERSION {
        send_error(
            socket,
            "protocol_version",
            &format!(
                "unsupported protocol version {}; server supports {}",
                hello.protocol_version, PROTOCOL_VERSION
            ),
        )
        .await?;
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

impl TranscriptionClient {
    async fn transcribe(
        &self,
        bytes: Vec<u8>,
        diarization: bool,
    ) -> Result<Vec<NormalizedTranscript>> {
        let url = format!("{}/v1/audio/transcriptions", self.base_url);
        let part = multipart::Part::bytes(bytes)
            .file_name("chunk.ogg")
            .mime_str("audio/ogg")?;
        let mut form = multipart::Form::new()
            .text("model", self.model.clone())
            .part("file", part);
        if let Some(language) = &self.language {
            form = form.text("language", language.clone());
        }
        if diarization {
            form = form
                .text("response_format", "diarized_json")
                .text("chunking_strategy", "auto");
        } else {
            form = form.text("response_format", "json");
        }

        let mut request = self.http.post(url).multipart(form);
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }

        let response = request.timeout(Duration::from_secs(120)).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            anyhow::bail!("transcription backend returned {status}: {body}");
        }

        normalize_transcription_response(&body)
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
}
