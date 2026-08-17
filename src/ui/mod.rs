///
/// # Neolink UI
///
/// This module serves a small web page for adjusting the camera while it is
/// streaming. It exists so that zoom and the IR illuminator can be changed on
/// the fly without standing up an MQTT broker.
///
/// MQTT is only ever a remote control surface: neolink is an MQTT *client* that
/// translates messages into `BcCamera` calls. This module makes exactly the same
/// calls, through the same `NeoInstance::run_task` wrapper, so it inherits the
/// reconnect handling and use permits that come with it.
///
/// The server listens on localhost only. The controls are unauthenticated, so
/// they are deliberately not reachable from the network.
///
/// # Usage
///
/// ```bash
/// neolink ui --config=config.toml
/// neolink syphon --ui CameraName
/// ```
///
use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use neolink_core::bc_protocol::LightState;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};

mod cmdline;

use crate::common::NeoReactor;
use crate::AnyResult;
pub(crate) use cmdline::Opt;

/// The page itself. Kept as a real file but compiled in so the binary stays
/// self contained
const INDEX: &str = include_str!("index.html");

/// How many seconds the camera keeps the floodlight on for before turning it
/// off by itself. The unit is documented in `dissector/messages.md` under
/// message 288, and 180 is what the MQTT handler passes
const FLOODLIGHT_DURATION: u16 = 180;

/// Entry point for the ui subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    serve(opt.port, reactor).await
}

/// Serve the UI until the process is stopped
///
/// The syphon subcommand also calls this so that video and controls can run in
/// one process.
pub(crate) async fn serve(port: u16, reactor: NeoReactor) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/cameras", get(cameras))
        .route("/api/state", get(state))
        .route("/api/zoom", post(set_zoom))
        .route("/api/ir", post(set_ir))
        .route("/api/floodlight", post(set_floodlight))
        .route("/api/statuslight", post(set_statuslight))
        .route("/api/snapshot", get(snapshot))
        .with_state(reactor);

    // Localhost only. These controls have no authentication, so binding any
    // other interface would hand camera control to the whole network
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("Could not bind the UI to {addr}"))?;

    log::info!("UI available at http://{}", addr);

    axum::serve(listener, app)
        .await
        .context("UI server stopped")?;
    Ok(())
}

/// An error that can be reported back to the page
struct UiError(anyhow::Error);

impl IntoResponse for UiError {
    fn into_response(self) -> axum::response::Response {
        // Camera calls fail for ordinary reasons: the model may not support the
        // ability, or it may be mid reconnect. Send the reason back so the page
        // can show it rather than silently doing nothing
        log::warn!("UI request failed: {:?}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for UiError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

/// Which camera a request is about
#[derive(Deserialize)]
struct CameraQuery {
    camera: String,
}

async fn cameras(State(reactor): State<NeoReactor>) -> Result<Json<Vec<String>>, UiError> {
    let config = reactor.config().await?;
    let names = config
        .borrow()
        .cameras
        .iter()
        .map(|camera| camera.name.clone())
        .collect();
    Ok(Json(names))
}

/// Current zoom bounds, IR state and status light state
#[derive(Serialize)]
struct CameraState {
    zoom: ZoomState,
    ir: String,
    status_light: String,
}

#[derive(Serialize)]
struct ZoomState {
    min: u32,
    max: u32,
    cur: u32,
}

async fn state(
    State(reactor): State<NeoReactor>,
    Query(query): Query<CameraQuery>,
) -> Result<Json<CameraState>, UiError> {
    let camera = reactor.get(&query.camera).await?;

    let zoom = camera
        .run_task(|cam| Box::pin(async move { Ok(cam.get_zoom().await?) }))
        .await?;
    // One call covers both lights: `state` is the IR illuminator and
    // `light_state` is the blue status LED
    let leds = camera
        .run_task(|cam| Box::pin(async move { Ok(cam.get_ledstate().await?) }))
        .await?;

    Ok(Json(CameraState {
        zoom: ZoomState {
            min: zoom.zoom.min_pos,
            max: zoom.zoom.max_pos,
            cur: zoom.zoom.cur_pos,
        },
        ir: leds.state,
        status_light: leds.light_state,
    }))
}

#[derive(Deserialize)]
struct ZoomRequest {
    camera: String,
    /// Absolute position in the camera's own units, as reported by /api/state
    pos: u32,
}

async fn set_zoom(
    State(reactor): State<NeoReactor>,
    Json(request): Json<ZoomRequest>,
) -> Result<Json<serde_json::Value>, UiError> {
    let camera = reactor.get(&request.camera).await?;
    let pos = request.pos;

    // zoom_to clamps to the camera's own limits for us
    camera
        .run_task(move |cam| {
            Box::pin(async move {
                cam.zoom_to(pos).await?;
                AnyResult::Ok(())
            })
        })
        .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct IrRequest {
    camera: String,
    /// "on", "off" or "auto"
    state: String,
}

/// A copyable stand in for `LightState`
///
/// `run_task` takes an `Fn`, so it may run the closure again if the camera
/// reconnects. `LightState` is neither `Clone` nor `Copy`, so it cannot be
/// captured and handed over; this is copied in and converted on each run.
#[derive(Clone, Copy)]
enum Ir {
    On,
    Off,
    Auto,
}

impl Ir {
    fn light_state(self) -> LightState {
        match self {
            Ir::On => LightState::On,
            Ir::Off => LightState::Off,
            Ir::Auto => LightState::Auto,
        }
    }
}

async fn set_ir(
    State(reactor): State<NeoReactor>,
    Json(request): Json<IrRequest>,
) -> Result<Json<serde_json::Value>, UiError> {
    let ir = match request.state.as_str() {
        "on" => Ir::On,
        "off" => Ir::Off,
        "auto" => Ir::Auto,
        other => {
            return Err(UiError(anyhow::anyhow!(
                "Unknown ir state {other:?}, expected on, off or auto"
            )))
        }
    };

    let camera = reactor.get(&request.camera).await?;
    camera
        .run_task(move |cam| {
            Box::pin(async move {
                cam.irled_light_set(ir.light_state()).await?;
                AnyResult::Ok(())
            })
        })
        .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Turning a light on or off
#[derive(Deserialize)]
struct LightRequest {
    camera: String,
    on: bool,
}

/// Turning the floodlight on or off, optionally overriding how long for
#[derive(Deserialize)]
struct FloodlightRequest {
    camera: String,
    on: bool,
    #[serde(default)]
    duration: Option<u16>,
}

async fn set_floodlight(
    State(reactor): State<NeoReactor>,
    Json(request): Json<FloodlightRequest>,
) -> Result<Json<serde_json::Value>, UiError> {
    let camera = reactor.get(&request.camera).await?;
    let on = request.on;
    let duration = request.duration.unwrap_or(FLOODLIGHT_DURATION);

    camera
        .run_task(move |cam| {
            Box::pin(async move {
                cam.set_floodlight_manual(on, duration).await?;
                AnyResult::Ok(())
            })
        })
        .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn set_statuslight(
    State(reactor): State<NeoReactor>,
    Json(request): Json<LightRequest>,
) -> Result<Json<serde_json::Value>, UiError> {
    let camera = reactor.get(&request.camera).await?;
    let on = request.on;

    camera
        .run_task(move |cam| {
            Box::pin(async move {
                cam.led_light_set(on).await?;
                AnyResult::Ok(())
            })
        })
        .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn snapshot(
    State(reactor): State<NeoReactor>,
    Query(query): Query<CameraQuery>,
) -> Result<impl IntoResponse, UiError> {
    let camera = reactor.get(&query.camera).await?;

    // Not every camera answers the snap command. Those that do not will surface
    // the error rather than hanging
    let jpeg = camera
        .run_task(|cam| Box::pin(async move { Ok(cam.get_snapshot().await?) }))
        .await?;

    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        jpeg,
    ))
}
