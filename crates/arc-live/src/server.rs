use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::thread;

use anyhow::{Context, Result};
use arc_live_core::state::AppState;
use arc_live_storage::Storage;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

#[derive(Clone)]
struct ServerState {
    app: Arc<RwLock<AppState>>,
    storage: Storage,
    events: broadcast::Sender<String>,
}

pub struct ServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
    events: broadcast::Sender<String>,
    port: u16,
}

impl ServerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn notify(&self, state: &AppState) {
        let state = public_state(state);
        if let Ok(payload) = serde_json::to_string(&json!({"type":"state.updated","data":state})) {
            let _ = self.events.send(payload);
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start(app: Arc<RwLock<AppState>>, storage: Storage, port: u16) -> Result<ServerHandle> {
    let (events, _) = broadcast::channel(64);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let worker_events = events.clone();
    let worker = thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("creating server runtime");
        runtime.block_on(async move {
            let state = ServerState {
                app,
                storage,
                events: worker_events,
            };
            let router = Router::new()
                .route("/api/v1/health", get(health))
                .route("/api/v1/snapshot", get(snapshot))
                .route("/api/v1/overlay", get(overlay_snapshot))
                .route("/api/v1/observations", get(observations))
                .route("/ws", get(websocket))
                .route("/overlay/discovery", get(overlay))
                .route("/overlay/live", get(live_overlay))
                .with_state(state);
            match bind_local_listener(port).await {
                Ok((listener, actual_port)) => {
                    let _ = ready_tx.send(Ok(actual_port));
                    let _ = axum::serve(listener, router)
                        .with_graceful_shutdown(async {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            }
        });
    });
    let actual_port = ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .context("local server did not report readiness")?
        .map_err(anyhow::Error::msg)?;
    Ok(ServerHandle {
        shutdown: Some(shutdown_tx),
        worker: Some(worker),
        events,
        port: actual_port,
    })
}

async fn bind_local_listener(preferred: u16) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let mut candidates = Vec::with_capacity(22);
    candidates.push(preferred);
    for offset in 1..=20 {
        if let Some(port) = preferred.checked_add(offset) {
            candidates.push(port);
        }
    }
    candidates.push(0);

    let mut last_error = None;
    for port in candidates {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => {
                let actual_port = listener.local_addr()?.port();
                return Ok((listener, actual_port));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no local port available",
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::bind_local_listener;

    #[test]
    fn selects_an_available_port_when_preferred_is_busy() {
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let preferred = occupied.local_addr().unwrap().port();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (listener, selected) = runtime.block_on(bind_local_listener(preferred)).unwrap();
        assert_ne!(selected, preferred);
        assert_eq!(listener.local_addr().unwrap().ip().to_string(), "127.0.0.1");
    }
}

async fn health(State(state): State<ServerState>) -> Json<Value> {
    let app = state.app.read().expect("state poisoned").clone();
    Json(
        json!({"status":"ok", "version":app.version, "phase":app.phase, "database":app.database_ready}),
    )
}

async fn snapshot(State(state): State<ServerState>) -> Json<AppState> {
    Json(public_state(&state.app.read().expect("state poisoned")))
}

async fn overlay_snapshot(State(state): State<ServerState>) -> Json<Value> {
    let app = state.app.read().expect("state poisoned");
    Json(serde_json::to_value(app.overlay_snapshot()).expect("serializing overlay snapshot"))
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

async fn observations(
    State(state): State<ServerState>,
    Query(query): Query<Limit>,
) -> impl IntoResponse {
    match state
        .storage
        .recent_observations(query.limit.unwrap_or(100).min(500))
    {
        Ok(rows) => (StatusCode::OK, Json(json!({"data":rows}))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

async fn websocket(ws: WebSocketUpgrade, State(state): State<ServerState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket_loop(socket, state))
}

async fn websocket_loop(mut socket: WebSocket, state: ServerState) {
    let initial = state.app.read().ok().and_then(|value| {
        serde_json::to_string(&json!({"type":"state.snapshot","data":public_state(&value)})).ok()
    });
    if let Some(initial) = initial
        && socket.send(Message::Text(initial.into())).await.is_err()
    {
        return;
    }
    let mut receiver = state.events.subscribe();
    while let Ok(payload) = receiver.recv().await {
        if socket.send(Message::Text(payload.into())).await.is_err() {
            break;
        }
    }
}

fn public_state(state: &AppState) -> AppState {
    let mut safe = state.clone();
    safe.keylog_path = "[LOCAL_PATH_REDACTED]".into();
    safe.activity.clear();
    safe
}

async fn overlay() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(OVERLAY_HTML))
}

async fn live_overlay() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(LIVE_OVERLAY_HTML_V2),
    )
}

const OVERLAY_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<style>
html,body{margin:0;background:transparent;color:#fff;font-family:Inter,Segoe UI,sans-serif;overflow:hidden}
.card{display:inline-flex;min-width:520px;gap:20px;align-items:center;padding:18px 24px;border:1px solid #ffffff26;border-radius:18px;background:linear-gradient(135deg,#10151aee,#1b242cee);box-shadow:0 12px 40px #0008}
.dot{width:12px;height:12px;border-radius:50%;background:#f6b73c;box-shadow:0 0 18px #f6b73c}.dot.ok{background:#53e0a1;box-shadow:0 0 18px #53e0a1}
.title{font-size:13px;letter-spacing:.15em;text-transform:uppercase;color:#aab7c2}.phase{font-size:25px;font-weight:800;margin-top:3px}.stats{display:flex;gap:18px;margin-left:auto}.n{font-size:22px;font-weight:800}.k{font-size:11px;color:#aab7c2;text-transform:uppercase}
</style></head><body><div class="card"><div id="dot" class="dot"></div><div><div class="title">ARC Live Discovery</div><div id="phase" class="phase">Connecting...</div></div><div class="stats"><div><div id="packets" class="n">0</div><div class="k">Packets</div></div><div><div id="decrypt" class="n">0</div><div class="k">Decrypted</div></div><div><div id="obs" class="n">0</div><div class="k">Events</div></div></div></div>
<script>
const phase=document.querySelector('#phase'),dot=document.querySelector('#dot'),packets=document.querySelector('#packets'),decrypt=document.querySelector('#decrypt'),obs=document.querySelector('#obs');
function draw(s){phase.textContent=String(s.phase||'unknown').replaceAll('_',' ');packets.textContent=s.packets_seen||0;decrypt.textContent=s.decrypted_records||0;obs.textContent=s.observations||0;dot.classList.toggle('ok',s.stats_stream_ready)}
async function snapshot(){try{draw(await(await fetch('/api/v1/snapshot')).json())}catch{phase.textContent='Collector offline'}}
function connect(){const ws=new WebSocket(`ws://${location.host}/ws`);ws.onmessage=e=>{const m=JSON.parse(e.data);draw(m.data)};ws.onclose=()=>setTimeout(connect,1000)}snapshot();connect();
</script></body></html>"#;

const LIVE_OVERLAY_HTML_V2: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<style>
*{box-sizing:border-box}html,body{margin:0;background:transparent;color:#f7f8f9;font-family:"Segoe UI Variable Text","Segoe UI",sans-serif;overflow:hidden}body{padding:4px}.panel{--panel-color:9 16 21;--panel-alpha:.55;--border-alpha:.16;--panel-blur:6px;--cells:3;display:grid;grid-template-columns:repeat(var(--cells),minmax(0,1fr));width:min(690px,calc(100vw - 8px));padding:5px 6px;background:rgb(var(--panel-color)/var(--panel-alpha));border:1px solid rgb(255 255 255/var(--border-alpha));border-radius:8px;backdrop-filter:blur(var(--panel-blur))}.cell{min-width:0;min-height:58px;display:grid;align-content:center;padding:7px 14px;border-left:1px solid rgb(255 255 255/var(--border-alpha))}.cell:first-child{border-left:0}.panel[data-cells="2"] .cell,.panel[data-cells="1"] .cell{text-align:center}.value{min-width:0;overflow:hidden;color:#f7f8f9;font-size:34px;font-weight:800;line-height:1;letter-spacing:-.025em;text-overflow:ellipsis;white-space:nowrap;font-variant-numeric:tabular-nums}.panel[data-cells="2"] .value,.panel[data-cells="1"] .value{font-size:38px}.label{margin-top:6px;overflow:hidden;color:#c5d0d5;font-size:10px;font-weight:600;line-height:1.15;letter-spacing:.035em;text-overflow:ellipsis;text-transform:uppercase;white-space:nowrap}.accent{color:#58e3a3}.danger{color:#ff727f}.loot{color:#ffc35a}@media(max-width:480px){body{padding:2px}.panel{width:calc(100vw - 4px);padding:4px}.cell{min-height:54px;padding:6px 8px}.value{font-size:28px}.label{font-size:9px}}
</style></head><body><div id="panel" class="panel"></div>
<script>
const params=new URLSearchParams(location.search),presetOverride=params.get('preset'),languageOverride=params.get('lang')||params.get('language'),opacityOverride=params.get('opacity'),backgroundOverride=params.get('background')||params.get('bg'),blurOverride=params.get('blur');
let formatter=new Intl.NumberFormat('ru-RU');
const num=v=>formatter.format(Number(v)||0),signed=v=>{const n=Number(v)||0;return n>0?'+'+formatter.format(n):n<0?'\u2212'+formatter.format(Math.abs(n)):'0'};
function language(value){return String(value||'ru').toLowerCase()==='en'?'en':'ru'}
function pickPreset(list,active){
 if(!Array.isArray(list)||list.length===0)return null;
 if(presetOverride){
  const byId=list.find(p=>p&&p.id===presetOverride);if(byId)return byId;
  const index=Number(presetOverride);if(Number.isInteger(index)&&index>=1&&index<=list.length)return list[index-1];
 }
 return list.find(p=>p&&p.id===active)||list[0];
}
function color(value){if(Array.isArray(value)&&value.length===3)return value.map(v=>Math.min(255,Math.max(0,Number(v)||0)));const hex=String(value||'').replace('#','');return/^[0-9a-f]{6}$/i.test(hex)?[0,2,4].map(i=>parseInt(hex.slice(i,i+2),16)):[9,16,21]}
function applyAppearance(panel,x){const parsed=Number(opacityOverride??x.opacity),opacity=Number.isFinite(parsed)?Math.min(100,Math.max(0,parsed)):55,level=opacity/100,bg=color(backgroundOverride??x.background_color),blurParsed=Number(blurOverride??x.background_blur),blur=Number.isFinite(blurParsed)?Math.min(20,Math.max(0,blurParsed)):6;panel.style.setProperty('--panel-alpha',String(level));panel.style.setProperty('--border-alpha',String(level*.28));panel.style.setProperty('--panel-color',bg.join(' '));panel.style.setProperty('--panel-blur',blur+'px')}
function draw(s){
 const x=s.overlay||s.stats||{},lang=language(languageOverride||x.language),panel=document.getElementById('panel');
 formatter=new Intl.NumberFormat(lang==='ru'?'ru-RU':'en-US');
 applyAppearance(panel,x);
 const preset=pickPreset(x.presets,x.preset);
 const cells=preset&&Array.isArray(preset.cells)?preset.cells:[];
 panel.style.setProperty('--cells',String(Math.max(cells.length,1)));
 panel.dataset.cells=String(cells.length);
 panel.replaceChildren();
 for(const cell of cells){
  const style=String(cell&&cell.style||'plain'),amount=Number(cell&&cell.value)||0;
  const box=document.createElement('div');box.className='cell';
  const value=document.createElement('div');value.className='value';
  if(style==='balance'){value.textContent=signed(amount);value.classList.add(amount<0?'danger':'accent')}
  else{value.textContent=num(amount);if(style!=='plain')value.classList.add(style)}
  const label=document.createElement('div');label.className='label';
  label.textContent=(lang==='en'?cell&&cell.label_en:cell&&cell.label_ru)||'';
  box.append(value,label);panel.append(box);
 }
}
async function init(){try{draw(await(await fetch('/api/v1/overlay')).json())}catch{}}
function connect(){const ws=new WebSocket(`ws://${location.host}/ws`);ws.onmessage=e=>draw(JSON.parse(e.data).data);ws.onclose=()=>setTimeout(connect,1000)}
init();connect();
</script></body></html>"#;
