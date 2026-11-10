//! The HTTP API (DESIGN.md §6, the M1 part). JSON in; JSON or NDJSON
//! event streams out. Every route but `/v1/health` needs the bearer token.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Json, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

use crate::events::{Event, Log, follow};
use crate::ids;
use crate::machines::{self, CreateRequest, ExecArgs, Manager};

/// Largest request body: an exec's stdin travels inside it.
const MAX_BODY: usize = 16 << 20;
/// Largest `metadata` a machine may carry, as JSON text.
const MAX_METADATA: usize = 4096;

pub struct App {
    pub mgr: Arc<Manager>,
    pub log: Arc<Log>,
    pub token: String,
}

pub fn router(app: Arc<App>) -> Router {
    let v1 = Router::new()
        .route("/v1/machines", post(create))
        .route("/v1/machines/{id}", get(info).delete(destroy))
        .route("/v1/machines/{id}/exec", post(exec))
        .route("/v1/events", get(events))
        .layer(middleware::from_fn_with_state(Arc::clone(&app), auth))
        .layer(DefaultBodyLimit::max(MAX_BODY));
    Router::new()
        .route("/v1/health", get(async || Json(json!({"ok": true}))))
        .merge(v1)
        .with_state(app)
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<machines::Error> for ApiError {
    fn from(e: machines::Error) -> Self {
        Self(
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            e.message,
        )
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

async fn auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(given.as_bytes(), app.token.as_bytes()) {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        )
        .into_response();
    }
    next.run(req).await
}

/// Compare without a timing difference that depends on where they differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn machine_id(id: &str) -> Result<(), ApiError> {
    if ids::is_id(id, "m") {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no machine {id:?}"),
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateBody {
    template: String,
    ttl: Option<Ttl>,
    metadata: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Ttl {
    idle_ms: Option<u64>,
    max_ms: Option<u64>,
}

async fn create(
    State(app): State<Arc<App>>,
    Json(body): Json<CreateBody>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(m) = &body.metadata
        && m.to_string().len() > MAX_METADATA
    {
        return Err(bad(format!("metadata is over {MAX_METADATA} bytes")));
    }
    let ttl = body.ttl.unwrap_or(Ttl {
        idle_ms: None,
        max_ms: None,
    });
    let req = CreateRequest {
        template: body.template,
        idle: ttl.idle_ms.map(Duration::from_millis),
        max: ttl.max_ms.map(Duration::from_millis),
        metadata: body.metadata,
    };
    let mgr = Arc::clone(&app.mgr);
    let info = blocking(move || Ok(mgr.create(req)?)).await?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn info(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    machine_id(&id)?;
    let mgr = Arc::clone(&app.mgr);
    Ok(Json(blocking(move || Ok(mgr.info(&id)?)).await?))
}

async fn destroy(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    machine_id(&id)?;
    let mgr = Arc::clone(&app.mgr);
    blocking(move || Ok(mgr.destroy(&id)?)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecBody {
    cmd: Vec<String>,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
    /// UTF-8 text for the command's stdin.
    stdin: Option<String>,
    /// Bytes for the command's stdin, base64. Not with `stdin`.
    stdin_base64: Option<String>,
    timeout_ms: Option<u64>,
}

fn exec_args(body: ExecBody) -> Result<ExecArgs, ApiError> {
    let stdin = match (body.stdin, body.stdin_base64) {
        (Some(_), Some(_)) => return Err(bad("give stdin or stdinBase64, not both")),
        (Some(s), None) => s.into_bytes(),
        (None, Some(b)) => B64
            .decode(b)
            .map_err(|e| bad(format!("stdinBase64: {e}")))?,
        (None, None) => Vec::new(),
    };
    let env = body.env.unwrap_or_default();
    for (k, v) in &env {
        if k.is_empty() || k.contains(['=', '\0']) || v.contains('\0') {
            return Err(bad(format!("bad environment variable {k:?}")));
        }
    }
    if body.cmd.iter().any(|a| a.contains('\0'))
        || body.cwd.as_ref().is_some_and(|c| c.contains('\0'))
    {
        return Err(bad("cmd and cwd must not contain NUL"));
    }
    Ok(ExecArgs {
        cmd: body.cmd,
        env: env.into_iter().collect(),
        cwd: body.cwd,
        stdin,
        timeout: body.timeout_ms.map(Duration::from_millis),
    })
}

fn ndjson(
    rx: tokio::sync::mpsc::Receiver<Result<axum::body::Bytes, std::convert::Infallible>>,
) -> Response {
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

/// Run a command. The response streams the exec's events as NDJSON and
/// ends with `exec.exited`, `exec.timedout` or `exec.failed`. If the client
/// goes, the exec goes on; `/v1/events` replays it.
async fn exec(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<ExecBody>,
) -> Result<Response, ApiError> {
    machine_id(&id)?;
    let args = exec_args(body)?;
    let mgr = Arc::clone(&app.mgr);
    let id2 = id.clone();
    let (exec, seq, args) = blocking(move || {
        let (exec, seq) = mgr.begin_exec(&id2, &args)?;
        Ok((exec, seq, args))
    })
    .await?;
    let (mgr, id2, exec2) = (Arc::clone(&app.mgr), id.clone(), exec.clone());
    tokio::task::spawn_blocking(move || mgr.run_exec(&id2, &exec2, args));
    let mine = move |e: &Event| e.str("exec") == Some(exec.as_str());
    let mine2 = mine.clone();
    // The machine's end goes out too, so a client can tell it from a
    // dropped connection.
    let keep = move |e: &Event| mine(e) || e.kind == "machine.destroyed";
    Ok(ndjson(follow(
        Arc::clone(&app.log),
        id,
        seq,
        keep,
        move |e| {
            e.kind == "machine.destroyed"
                || (mine2(e)
                    && matches!(
                        e.kind.as_str(),
                        "exec.exited" | "exec.timedout" | "exec.failed"
                    ))
        },
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsQuery {
    machine: String,
    from: Option<u64>,
}

/// A machine's events from `from` on, then live, ending after
/// `machine.destroyed`.
async fn events(
    State(app): State<Arc<App>>,
    Query(q): Query<EventsQuery>,
) -> Result<Response, ApiError> {
    machine_id(&q.machine)?;
    let (log, m) = (Arc::clone(&app.log), q.machine.clone());
    let known = blocking(move || Ok(!log.read(&m, 1, 1)?.is_empty())).await?;
    if !known {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no machine {}", q.machine),
        ));
    }
    Ok(ndjson(follow(
        Arc::clone(&app.log),
        q.machine,
        q.from.unwrap_or(1),
        |_| true,
        |e| e.kind == "machine.destroyed",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    fn body(v: Value) -> ExecBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn exec_args_are_checked() {
        let a = exec_args(body(json!({"cmd": ["cat"], "stdinBase64": "aGk="}))).unwrap();
        assert_eq!(a.stdin, b"hi");
        assert!(
            exec_args(body(
                json!({"cmd": ["cat"], "stdin": "a", "stdinBase64": "YQ=="})
            ))
            .is_err()
        );
        assert!(exec_args(body(json!({"cmd": ["env"], "env": {"A=B": "c"}}))).is_err());
        assert!(exec_args(body(json!({"cmd": ["env"], "env": {"": "c"}}))).is_err());
        assert!(exec_args(body(json!({"cmd": ["a\u{0}b"]}))).is_err());
        assert!(serde_json::from_value::<ExecBody>(json!({"cmd": [], "shell": true})).is_err());
    }
}
