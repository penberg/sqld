mod hrana_over_http_1;
mod result_builder;
pub mod stats;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use base64::prelude::BASE64_STANDARD_NO_PAD;
use base64::Engine;
use hyper::body::to_bytes;
use hyper::server::conn::AddrIncoming;
use hyper::{Body, Method, Request, Response, StatusCode};
use serde::Serialize;
use serde_json::Number;
use tokio::sync::{mpsc, oneshot};
use tonic::codegen::http;
use tower::ServiceBuilder;
use tower_http::trace::DefaultOnResponse;
use tower_http::{compression::CompressionLayer, cors};
use tracing::{Level, Span};

use crate::auth::{Auth, Authenticated};
use crate::database::factory::DbFactory;
use crate::database::Database;
use crate::error::Error;
use crate::hrana;
use crate::http::types::HttpQuery;
use crate::query::{self, Query};
use crate::query_analysis::{predict_final_state, State, Statement};
use crate::query_result_builder::QueryResultBuilder;
use crate::stats::Stats;
use crate::utils::services::idle_shutdown::IdleShutdownLayer;
use crate::version;

use self::result_builder::JsonHttpPayloadBuilder;
use self::types::QueryObject;

impl TryFrom<query::Value> for serde_json::Value {
    type Error = Error;

    fn try_from(value: query::Value) -> Result<Self, Self::Error> {
        let value = match value {
            query::Value::Null => serde_json::Value::Null,
            query::Value::Integer(i) => serde_json::Value::Number(Number::from(i)),
            query::Value::Real(x) => {
                serde_json::Value::Number(Number::from_f64(x).ok_or_else(|| {
                    Error::DbValueError(format!(
                        "Cannot to convert database value `{x}` to a JSON number"
                    ))
                })?)
            }
            query::Value::Text(s) => serde_json::Value::String(s),
            query::Value::Blob(v) => serde_json::json!({
                "base64": BASE64_STANDARD_NO_PAD.encode(v),
            }),
        };

        Ok(value)
    }
}

/// Encodes a query response rows into json
#[derive(Debug, Serialize)]
struct RowsResponse {
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    message: String,
}

fn error(msg: &str, code: StatusCode) -> Response<Body> {
    let err = serde_json::json!({ "error": msg });
    Response::builder()
        .status(code)
        .body(Body::from(serde_json::to_vec(&err).unwrap()))
        .unwrap()
}

fn parse_queries(queries: Vec<QueryObject>) -> anyhow::Result<Vec<Query>> {
    let mut out = Vec::with_capacity(queries.len());
    for query in queries {
        let mut iter = Statement::parse(&query.q);
        let stmt = iter.next().transpose()?.unwrap_or_default();
        if iter.next().is_some() {
            anyhow::bail!(
                "found more than one command in a single statement string. It is allowed to issue only one command per string."
            );
        }
        let query = Query {
            stmt,
            params: query.params.0,
            want_rows: true,
        };

        out.push(query);
    }

    match predict_final_state(State::Init, out.iter().map(|q| &q.stmt)) {
        State::Txn => anyhow::bail!("interactive transaction not allowed in HTTP queries"),
        State::Init => (),
        // maybe we should err here, but let's sqlite deal with that.
        State::Invalid => (),
    }

    Ok(out)
}

fn parse_payload(data: &[u8]) -> Result<HttpQuery, Response<Body>> {
    match serde_json::from_slice(data) {
        Ok(data) => Ok(data),
        Err(e) => Err(error(&e.to_string(), http::status::StatusCode::BAD_REQUEST)),
    }
}

async fn handle_query<D: Database>(
    mut req: Request<Body>,
    auth: Authenticated,
    namespace: String,
    db_factory: Arc<dyn DbFactory<Db = D>>,
) -> anyhow::Result<Response<Body>> {
    let bytes = to_bytes(req.body_mut()).await?;
    let req = match parse_payload(&bytes) {
        Ok(req) => req,
        Err(resp) => return Ok(resp),
    };

    let batch = match parse_queries(req.statements) {
        Ok(queries) => queries,
        Err(e) => return Ok(error(&e.to_string(), StatusCode::BAD_REQUEST)),
    };

    let db = db_factory.create(&namespace).await?;

    let builder = JsonHttpPayloadBuilder::new();
    match db.execute_batch_or_rollback(batch, auth, builder).await {
        Ok((builder, _)) => Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(Body::from(builder.into_ret()))?),
        Err(e) => Ok(error(
            &format!("internal error: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

async fn show_console() -> anyhow::Result<Response<Body>> {
    Ok(Response::new(Body::from(std::include_str!("console.html"))))
}

fn handle_health() -> Response<Body> {
    // return empty OK
    Response::new(Body::empty())
}

async fn handle_upgrade(
    upgrade_tx: &mpsc::Sender<hrana::ws::Upgrade>,
    req: Request<Body>,
) -> Response<Body> {
    let (response_tx, response_rx) = oneshot::channel();
    let _: Result<_, _> = upgrade_tx
        .send(hrana::ws::Upgrade {
            request: req,
            response_tx,
        })
        .await;

    match response_rx.await {
        Ok(response) => response,
        Err(_) => Response::builder()
            .status(hyper::StatusCode::SERVICE_UNAVAILABLE)
            .body("sqld was not able to process the HTTP upgrade".into())
            .unwrap(),
    }
}

fn split_url_path(path: &str) -> (&str, &str) {
    let idx = path[1..].find('/').unwrap();
    let first = &path[0..idx+1];
    (first, path.trim_start_matches(first))
}

async fn handle_request<D: Database>(
    auth: Arc<Auth>,
    req: Request<Body>,
    upgrade_tx: mpsc::Sender<hrana::ws::Upgrade>,
    hrana_http_srv: Arc<hrana::http::Server<D>>,
    db_factory: Arc<dyn DbFactory<Db = D>>,
    enable_console: bool,
    stats: Stats,
) -> anyhow::Result<Response<Body>> {
    let (namespace, path) = split_url_path(req.uri().path());
    let namespace = namespace.to_string();
    if hyper_tungstenite::is_upgrade_request(&req) {
        return Ok(handle_upgrade(&upgrade_tx, req).await);
    }

    if req.method() == Method::GET && path == "/health" {
        return Ok(handle_health());
    }
    let auth_header = req.headers().get(hyper::header::AUTHORIZATION);
    let auth = match auth.authenticate_http(auth_header) {
        Ok(auth) => auth,
        Err(err) => {
            return Ok(Response::builder()
                .status(hyper::StatusCode::UNAUTHORIZED)
                .body(err.to_string().into())
                .unwrap());
        }
    };

    match (req.method(), path) {
        (&Method::POST, "/") => handle_query(req, auth, namespace, db_factory.clone()).await,
        (&Method::GET, "/version") => Ok(handle_version()),
        (&Method::GET, "/console") if enable_console => show_console().await,
        (&Method::GET, "/v1/stats") => Ok(stats::handle_stats(&stats)),

        (&Method::GET, "/v1") => hrana_over_http_1::handle_index(req).await,
        (&Method::POST, "/v1/execute") => {
            hrana_over_http_1::handle_execute(db_factory, auth, req).await
        }
        (&Method::POST, "/v1/batch") => {
            hrana_over_http_1::handle_batch(db_factory, auth, req).await
        }

        (&Method::GET, "/v2") => {
            hrana_http_srv
                .handle(auth, hrana::http::Route::GetIndex, req)
                .await
        }
        (&Method::POST, "/v2/pipeline") => {
            hrana_http_srv
                .handle(auth, hrana::http::Route::PostPipeline, req)
                .await
        }

        _ => Ok(Response::builder().status(404).body(Body::empty()).unwrap()),
    }
}

fn handle_version() -> Response<Body> {
    let version = version::version();
    Response::new(Body::from(version))
}

// TODO: refactor
#[allow(clippy::too_many_arguments)]
pub async fn run_http<D: Database>(
    addr: SocketAddr,
    auth: Arc<Auth>,
    db_factory: Arc<dyn DbFactory<Db = D>>,
    upgrade_tx: mpsc::Sender<hrana::ws::Upgrade>,
    hrana_http_srv: Arc<hrana::http::Server<D>>,
    enable_console: bool,
    idle_shutdown_layer: Option<IdleShutdownLayer>,
    stats: Stats,
) -> anyhow::Result<()> {
    tracing::info!("listening for HTTP requests on {addr}");

    fn trace_request<B>(req: &Request<B>, _span: &Span) {
        tracing::debug!("got request: {} {}", req.method(), req.uri());
    }
    let service = ServiceBuilder::new()
        .option_layer(idle_shutdown_layer)
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .on_request(trace_request)
                .on_response(
                    DefaultOnResponse::new()
                        .level(Level::DEBUG)
                        .latency_unit(tower_http::LatencyUnit::Micros),
                ),
        )
        .layer(CompressionLayer::new())
        .layer(
            cors::CorsLayer::new()
                .allow_methods(cors::AllowMethods::any())
                .allow_headers(cors::Any)
                .allow_origin(cors::Any),
        )
        .service_fn(move |req| {
            handle_request(
                auth.clone(),
                req,
                upgrade_tx.clone(),
                hrana_http_srv.clone(),
                db_factory.clone(),
                enable_console,
                stats.clone(),
            )
        });

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let server = hyper::server::Server::builder(AddrIncoming::from_listener(listener)?)
        .tcp_nodelay(true)
        .serve(tower::make::Shared::new(service));

    server.await.context("Http server exited with an error")?;

    Ok(())
}
