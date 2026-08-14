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

const _LIVE_OVERLAY_HTML_V1: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<style>
*{box-sizing:border-box}html,body{margin:0;background:transparent;color:#f7f8f9;font-family:Inter,Segoe UI,sans-serif}body{padding:10px}.panel{width:min(760px,calc(100vw - 20px));padding:13px;background:linear-gradient(145deg,#090d10f2,#10181ef0);border:1px solid #ffffff24;border-radius:18px;box-shadow:0 14px 42px #0008}.head{display:flex;align-items:center;gap:10px;padding:1px 3px 12px}.dot{width:9px;height:9px;border-radius:50%;background:#e45d68;box-shadow:0 0 14px #e45d68}.dot.ok{background:#53e0a1;box-shadow:0 0 14px #53e0a1}.title{font-size:14px;font-weight:850;letter-spacing:.12em;text-transform:uppercase}.status{margin-top:3px;font-size:9px;color:#8da0ad;text-transform:uppercase;letter-spacing:.1em}.meta{margin-left:auto;text-align:right}.mode{font-size:10px;color:#53e0a1;font-weight:800;text-transform:uppercase;letter-spacing:.1em}.mode.demo{color:#f6b73c}.updated{margin-top:3px;font-size:9px;color:#718490}.preset{display:none;grid-template-columns:repeat(3,minmax(0,1fr));gap:5px}.preset.active{display:grid}.cell{min-height:90px;padding:15px;background:linear-gradient(150deg,#19242bed,#11191fed);border:1px solid #ffffff0d;border-radius:10px}.value{font-size:31px;font-weight:900;line-height:1;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.label{margin-top:10px;font-size:9px;color:#9aabb7;text-transform:uppercase;letter-spacing:.12em}.accent{color:#53e0a1}.danger{color:#ff707c}.loot{color:#f6b73c}.outcome{align-items:center}.outcome .cell{text-align:center;min-height:112px}.outcome .value{font-size:46px}.scope{display:flex;align-items:center;justify-content:center;text-align:center;color:#91a4b0;font-size:11px;line-height:1.5;text-transform:uppercase;letter-spacing:.1em}@media(max-width:560px){.preset{grid-template-columns:1fr}.panel{width:calc(100vw - 20px)}}
</style></head><body><div class="panel"><div class="head"><div id="dot" class="dot"></div><div><div id="title" class="title">ARC Live</div><div id="status" class="status">Connecting</div></div><div class="meta"><div id="mode" class="mode">Live</div><div id="updated" class="updated">Waiting for sync</div></div></div>
<div id="account" class="preset"><div class="cell"><div id="accountDowns" class="value">0</div><div class="label">Total player knocks</div></div><div class="cell"><div id="accountDamage" class="value accent">0</div><div class="label">Damage dealt to raiders</div></div><div class="cell"><div id="accountLoot" class="value loot">0</div><div class="label">Total value extracted</div></div></div>
<div id="session" class="preset"><div class="cell"><div id="sessionDowns" class="value">0</div><div class="label">Knocks since launch</div></div><div class="cell"><div id="sessionWins" class="value accent">0</div><div class="label">Successful exits</div></div><div class="cell"><div id="sessionLoot" class="value loot">0</div><div class="label">Value extracted</div></div></div>
<div id="outcome" class="preset outcome"><div class="cell"><div id="outcomeWins" class="value accent">0</div><div class="label">Win · extracted alive</div></div><div id="outcomeScope" class="scope">Since app launch</div><div class="cell"><div id="outcomeLosses" class="value danger">0</div><div class="label">Lose · knocked out</div></div></div>
</div>
<script>
const el=id=>document.getElementById(id),nf=new Intl.NumberFormat('ru-RU');
const num=v=>nf.format(Number(v)||0),override=new URLSearchParams(location.search).get('preset');
function preset(value){value=String(value||'account').toLowerCase();if(value==='1')return'account';if(value==='2')return'session';if(value==='3')return'outcome';return['account','session','outcome'].includes(value)?value:'account'}
function draw(s){const x=s.overlay||s.stats||{},active=preset(override||x.preset);for(const name of ['account','session','outcome'])el(name).classList.toggle('active',name===active);el('title').textContent={account:'Account totals',session:'Since ARC Live launch',outcome:'Win | Lose'}[active];el('accountDowns').textContent=num(x.downs);el('accountDamage').textContent=num(x.raider_damage);el('accountLoot').textContent=num(x.loot_value);el('sessionDowns').textContent=num(x.session_downs);el('sessionWins').textContent=num(x.session_extractions);el('sessionLoot').textContent=num(x.session_loot_value);const sessionRounds=(Number(x.session_extractions)||0)+(Number(x.session_deaths)||0),useToday=sessionRounds===0&&x.today_available,wins=useToday?x.today_extractions:x.session_extractions,losses=useToday?x.today_deaths:x.session_deaths;el('outcomeWins').textContent=num(wins);el('outcomeLosses').textContent=num(losses);el('outcomeScope').textContent=useToday?'Today · fallback':'Since app launch';const demo=x.mode==='demo';el('mode').textContent=demo?'Demo data':'Live data';el('mode').classList.toggle('demo',demo);const running=s.game_running!==false;el('dot').classList.toggle('ok',running);el('status').textContent=running?'Game connected':'Game not detected';const stamp=s.updated_at||s.last_update;el('updated').textContent=stamp?'Updated '+new Date(stamp).toLocaleTimeString('ru-RU'):'Waiting for sync'}
async function init(){try{draw(await(await fetch('/api/v1/overlay')).json())}catch{}}function connect(){const ws=new WebSocket(`ws://${location.host}/ws`);ws.onmessage=e=>draw(JSON.parse(e.data).data);ws.onclose=()=>setTimeout(connect,1000)}init();connect();
</script></body></html>"#;

const LIVE_OVERLAY_HTML_V2: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<style>
*{box-sizing:border-box}html,body{margin:0;background:transparent;color:#f7f8f9;font-family:"Segoe UI Variable Text","Segoe UI",sans-serif;overflow:hidden}body{padding:4px}.panel{--panel-color:9 16 21;--panel-alpha:.55;--border-alpha:.16;--panel-blur:6px;width:min(690px,calc(100vw - 8px));padding:5px 6px;background:rgb(var(--panel-color)/var(--panel-alpha));border:1px solid rgb(255 255 255/var(--border-alpha));border-radius:8px;backdrop-filter:blur(var(--panel-blur))}.head{display:none}.preset{display:none;grid-template-columns:repeat(3,minmax(0,1fr))}.preset.active{display:grid}.cell{min-width:0;min-height:58px;display:grid;align-content:center;padding:7px 14px;border-left:1px solid rgb(255 255 255/var(--border-alpha))}.cell:first-child{border-left:0}.value{min-width:0;overflow:hidden;color:#f7f8f9;font-size:34px;font-weight:800;line-height:1;letter-spacing:-.025em;text-overflow:ellipsis;white-space:nowrap;font-variant-numeric:tabular-nums}.label{margin-top:6px;overflow:hidden;color:#c5d0d5;font-size:10px;font-weight:600;line-height:1.15;letter-spacing:.035em;text-overflow:ellipsis;text-transform:uppercase;white-space:nowrap}.accent{color:#58e3a3}.danger{color:#ff727f}.loot{color:#ffc35a}.outcome{grid-template-columns:repeat(2,minmax(0,1fr))}.outcome .cell{min-height:58px;text-align:center}.outcome .value{font-size:38px}@media(max-width:480px){body{padding:2px}.panel{width:calc(100vw - 4px);padding:4px}.cell{min-height:54px;padding:6px 8px}.value{font-size:28px}.label{font-size:9px}.outcome .value{font-size:32px}}
</style></head><body><div id="panel" class="panel"><div class="head"><div id="dot" class="dot"></div><div id="title" class="title">ARC Live</div><div class="meta"><div id="mode" class="mode">Эфир</div><div id="updated" class="updated">--:--:--</div></div></div>
<div id="account" class="preset"><div class="cell"><div id="accountDowns" class="value">0</div><div id="accountDownsLabel" class="label">Ноки игроков</div></div><div class="cell"><div id="accountDamage" class="value accent">0</div><div id="accountDamageLabel" class="label">Урон рейдерам</div></div><div class="cell"><div id="accountLoot" class="value loot">0</div><div id="accountLootLabel" class="label">Вынесено</div></div></div>
<div id="session" class="preset"><div class="cell"><div id="sessionDowns" class="value">0</div><div id="sessionDownsLabel" class="label">Ноки с запуска</div></div><div class="cell"><div id="sessionWins" class="value accent">0</div><div id="sessionWinsLabel" class="label">Успешные выходы</div></div><div class="cell"><div id="sessionLoot" class="value danger">−56 100</div><div id="sessionLootLabel" class="label">Баланс</div></div></div>
<div id="outcome" class="preset outcome"><div class="cell"><div id="outcomeWins" class="value accent">0</div><div id="outcomeWinsLabel" class="label">Вышел живым</div></div><div class="cell"><div id="outcomeLosses" class="value danger">0</div><div id="outcomeLossesLabel" class="label">Погиб</div></div></div>
<div id="pve" class="preset outcome"><div class="cell"><div id="pveLoot" class="value loot">0</div><div id="pveLootLabel" class="label">Вынесено за стрим</div></div><div class="cell"><div id="pveDamage" class="value accent">0</div><div id="pveDamageLabel" class="label">Урон аркам</div></div></div>
<div id="pvp" class="preset outcome"><div class="cell"><div id="pvpKnocks" class="value">0</div><div id="pvpKnocksLabel" class="label">Ноки игроков</div></div><div class="cell"><div id="pvpDamage" class="value danger">0</div><div id="pvpDamageLabel" class="label">Урон игрокам</div></div></div>
</div>
<script>
const el=id=>document.getElementById(id),params=new URLSearchParams(location.search),presetOverride=params.get('preset'),languageOverride=params.get('lang')||params.get('language'),opacityOverride=params.get('opacity'),backgroundOverride=params.get('background')||params.get('bg'),blurOverride=params.get('blur');let formatter=new Intl.NumberFormat('ru-RU');
const num=v=>formatter.format(Number(v)||0),signed=v=>{const n=Number(v)||0;return n>0?'+'+formatter.format(n):n<0?'−'+formatter.format(Math.abs(n)):'0'};
function preset(value){value=String(value||'account').toLowerCase();if(value==='1')return'account';if(value==='2')return'session';if(value==='3')return'outcome';if(value==='4')return'pve';if(value==='5')return'pvp';return['account','session','outcome','pve','pvp'].includes(value)?value:'account'}
function language(value){return String(value||'ru').toLowerCase()==='en'?'en':'ru'}
function setText(id,value){el(id).textContent=value}
function color(value){if(Array.isArray(value)&&value.length===3)return value.map(v=>Math.min(255,Math.max(0,Number(v)||0)));const hex=String(value||'').replace('#','');return/^[0-9a-f]{6}$/i.test(hex)?[0,2,4].map(i=>parseInt(hex.slice(i,i+2),16)):[9,16,21]}
function applyAppearance(x){const parsed=Number(opacityOverride??x.opacity),opacity=Number.isFinite(parsed)?Math.min(100,Math.max(0,parsed)):55,level=opacity/100,bg=color(backgroundOverride??x.background_color),blurParsed=Number(blurOverride??x.background_blur),blur=Number.isFinite(blurParsed)?Math.min(20,Math.max(0,blurParsed)):6;el('panel').style.setProperty('--panel-alpha',String(level));el('panel').style.setProperty('--border-alpha',String(level*.28));el('panel').style.setProperty('--panel-color',bg.join(' '));el('panel').style.setProperty('--panel-blur',blur+'px')}
function draw(s){
 const x=s.overlay||s.stats||{},active=preset(presetOverride||x.preset),lang=language(languageOverride||x.language),t=lang==='ru'?{title:{account:'Статистика аккаунта',session:'Текущий стрим',outcome:'Победы | Поражения',pve:'PvE',pvp:'PvP'},account:['Ноки игроков','Урон рейдерам','Вынесено'],session:['Ноки за стрим','Успешные выходы','Баланс'],outcome:['Вышел живым','Погиб'],pve:['Вынесено за стрим','Урон аркам'],pvp:['Ноки игроков','Урон игрокам'],mode:['Демо','Эфир']}:{title:{account:'Account totals',session:'Current stream',outcome:'Win | Lose',pve:'PvE',pvp:'PvP'},account:['Player knocks','Raider damage','Extracted value'],session:['Stream knocks','Successful exits','Balance'],outcome:['Extracted alive','Knocked out'],pve:['Stream loot','ARC damage'],pvp:['Player knocks','Player damage'],mode:['Demo','Live']};
 formatter=new Intl.NumberFormat(lang==='ru'?'ru-RU':'en-US');for(const name of ['account','session','outcome','pve','pvp'])el(name).classList.toggle('active',name===active);setText('title',t.title[active]);
 const accountLabels=(lang==='ru'?x.widget_account_labels_ru:x.widget_account_labels_en)||t.account,sessionLabels=(lang==='ru'?x.widget_session_labels_ru:x.widget_session_labels_en)||t.session,outcomeLabels=(lang==='ru'?x.widget_outcome_labels_ru:x.widget_outcome_labels_en)||t.outcome,pveLabels=(lang==='ru'?x.widget_pve_labels_ru:x.widget_pve_labels_en)||t.pve,pvpLabels=(lang==='ru'?x.widget_pvp_labels_ru:x.widget_pvp_labels_en)||t.pvp;
 setText('accountDownsLabel',accountLabels[0]||t.account[0]);setText('accountDamageLabel',accountLabels[1]||t.account[1]);setText('accountLootLabel',accountLabels[2]||t.account[2]);setText('sessionDownsLabel',sessionLabels[0]||t.session[0]);setText('sessionWinsLabel',sessionLabels[1]||t.session[1]);setText('sessionLootLabel',sessionLabels[2]||t.session[2]);setText('outcomeWinsLabel',outcomeLabels[0]||t.outcome[0]);setText('outcomeLossesLabel',outcomeLabels[1]||t.outcome[1]);
 setText('pveLootLabel',pveLabels[0]||t.pve[0]);setText('pveDamageLabel',pveLabels[1]||t.pve[1]);setText('pvpKnocksLabel',pvpLabels[0]||t.pvp[0]);setText('pvpDamageLabel',pvpLabels[1]||t.pvp[1]);
 const account=Array.isArray(x.widget_account)&&x.widget_account.length===3?x.widget_account:[x.eliminations,x.raider_damage,x.loot_value],session=Array.isArray(x.widget_session)&&x.widget_session.length===3?x.widget_session:[x.session_downs,x.session_extractions,x.session_money_delta];const sessionRounds=(Number(x.session_extractions)||0)+(Number(x.session_deaths)||0),useToday=sessionRounds===0&&x.today_available,fallbackOutcome=[useToday?x.today_extractions:x.session_extractions,useToday?x.today_deaths:x.session_deaths],outcome=Array.isArray(x.widget_outcome)&&x.widget_outcome.length===2?x.widget_outcome:fallbackOutcome,pve=Array.isArray(x.widget_pve)&&x.widget_pve.length===2?x.widget_pve:[x.session_loot_value,0],pvp=Array.isArray(x.widget_pvp)&&x.widget_pvp.length===2?x.widget_pvp:[x.session_downs,0];
 setText('accountDowns',num(account[0]));setText('accountDamage',num(account[1]));setText('accountLoot',num(account[2]));setText('sessionDowns',num(session[0]));setText('sessionWins',num(session[1]));setText('sessionLoot',signed(session[2]));el('sessionLoot').classList.toggle('accent',Number(session[2])>=0);el('sessionLoot').classList.toggle('danger',Number(session[2])<0);setText('outcomeWins',num(outcome[0]));setText('outcomeLosses',num(outcome[1]));setText('pveLoot',num(pve[0]));setText('pveDamage',num(pve[1]));setText('pvpKnocks',num(pvp[0]));setText('pvpDamage',num(pvp[1]));const demo=x.mode==='demo';setText('mode',t.mode[demo?0:1]);el('mode').classList.toggle('demo',demo);el('dot').classList.toggle('ok',s.game_running!==false);const stamp=s.updated_at||s.last_update;setText('updated',stamp?new Date(stamp).toLocaleTimeString(lang==='ru'?'ru-RU':'en-US'):'--:--:--');applyAppearance(x)
}
async function init(){try{draw(await(await fetch('/api/v1/overlay')).json())}catch{}}function connect(){const ws=new WebSocket(`ws://${location.host}/ws`);ws.onmessage=e=>draw(JSON.parse(e.data).data);ws.onclose=()=>setTimeout(connect,1000)}init();connect();
</script></body></html>"#;
