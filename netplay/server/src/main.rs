//! A two-seat WebSocket relay. The game and its persistent saves stay with the players.
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    routing::get,
};
use sol_netplay::{Command, Event, Hello, VERSION};
use sol_relay::{Failure, Game, MAX_MESSAGE, hex};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    time::timeout,
};

type Rooms = Arc<Mutex<HashMap<String, Room>>>;
const GRACE: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct Connection {
    out: mpsc::Sender<Event>,
    stop: watch::Sender<bool>,
}

impl Connection {
    fn send(&self, msg: Event) {
        if self.out.try_send(msg).is_err() {
            self.stop.send_replace(true);
        }
    }
}

struct Seat {
    token: String,
    connection: Option<Connection>,
    lost: Option<Instant>,
}

impl Seat {
    fn new(connection: Connection) -> Result<Self, Failure> {
        Ok(Self {
            token: random(32)?,
            connection: Some(connection),
            lost: None,
        })
    }
}

struct Room {
    core: String,
    game: String,
    name: String,
    seats: [Option<Seat>; 2],
    play: Game,
}

fn random(length: usize) -> Result<String, Failure> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = vec![0; length];
    getrandom::fill(&mut bytes).map_err(|_| "unavailable")?;
    Ok(bytes
        .iter()
        .map(|n| ALPHABET[usize::from(n & 31)] as char)
        .collect())
}

impl Room {
    fn send(&self, seat: usize, event: Event) {
        if let Some(tx) = self.seats[seat]
            .as_ref()
            .and_then(|s| s.connection.as_ref())
        {
            tx.send(event);
        }
    }

    fn both(&self, event: Event) {
        self.send(0, event.clone());
        self.send(1, event);
    }

    fn apply(&mut self, seat: usize, command: Command) -> Result<(), Failure> {
        if let Some(event) = self.play.apply(seat, command)? {
            match event {
                Event::Snapshot { .. } => self.send(1, event),
                Event::Pong { seat } => self.send(seat, event),
                _ => self.both(event),
            }
        }
        Ok(())
    }
}

fn enter(
    rooms: &mut HashMap<String, Room>,
    hello: Hello,
    connection: Connection,
) -> Result<(String, usize), Failure> {
    let (code, seat) = match hello {
        Hello::Create {
            version: VERSION,
            core,
            game,
            name,
        } => {
            if !hex(&core, 64) || !hex(&game, 40) || name.len() > 512 {
                return Err("game");
            }
            if rooms.len() >= 256 {
                return Err("full");
            }
            let mut code = random(6)?;
            while rooms.contains_key(&code) {
                code = random(6)?;
            }
            rooms.insert(
                code.clone(),
                Room {
                    core,
                    play: Game::new(game.clone()),
                    game,
                    name,
                    seats: [Some(Seat::new(connection)?), None],
                },
            );
            (code, 0)
        }
        Hello::Join {
            version: VERSION,
            code,
            core,
        } => {
            let room = rooms.get_mut(&code).ok_or("missing")?;
            if room.core != core {
                return Err("version");
            }
            if room.seats[1].is_some() {
                return Err("full");
            }
            room.seats[1] = Some(Seat::new(connection)?);
            (code, 1)
        }
        Hello::Resume {
            version: VERSION,
            code,
            token,
        } => {
            let room = rooms.get_mut(&code).ok_or("missing")?;
            let index = room
                .seats
                .iter()
                .position(|s| s.as_ref().is_some_and(|s| s.token == token))
                .ok_or("missing")?;
            let seat = room.seats[index].as_mut().unwrap();
            if let Some(old) = seat.connection.replace(connection) {
                old.stop.send_replace(true);
            }
            seat.lost = None;
            room.apply(index, Command::Pause)?;
            (code, index)
        }
        _ => return Err("version"),
    };
    let room = &rooms[&code];
    room.send(
        seat,
        Event::Seat {
            code: code.clone(),
            seat,
            token: room.seats[seat].as_ref().unwrap().token.clone(),
            game: room.game.clone(),
            name: room.name.clone(),
        },
    );
    Ok((code, seat))
}

fn end(rooms: &mut HashMap<String, Room>, code: &str, reason: Failure) {
    if let Some(room) = rooms.remove(code) {
        room.both(Event::Error {
            reason: reason.into(),
        });
    }
}

async fn write(socket: &mut WebSocket, msg: &Event) -> bool {
    let text = serde_json::to_string(msg).unwrap();
    matches!(
        timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(text.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn serve(mut socket: WebSocket, rooms: Rooms) {
    let Ok(Some(Ok(Message::Text(text)))) = timeout(Duration::from_secs(10), socket.recv()).await
    else {
        return;
    };
    let (out, mut rx) = mpsc::channel(32);
    let (stop, mut stopped) = watch::channel(false);
    let connection = Connection { out, stop };
    let result = serde_json::from_str(&text)
        .map_err(|_| "version")
        .and_then(|h| enter(&mut rooms.lock().unwrap(), h, connection.clone()));
    let (code, seat) = match result {
        Ok(pair) => pair,
        Err(reason) => {
            write(
                &mut socket,
                &Event::Error {
                    reason: reason.into(),
                },
            )
            .await;
            return;
        }
    };
    let owns = |room: &Room| {
        room.seats[seat]
            .as_ref()
            .and_then(|s| s.connection.as_ref())
            .is_some_and(|c| c.out.same_channel(&connection.out))
    };
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = stopped.changed() => break,
            Some(event) = rx.recv() => {
                if !write(&mut socket, &event).await || matches!(event, Event::Error { .. }) { break; }
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(30) { break; }
                if !matches!(timeout(Duration::from_secs(5), socket.send(Message::Ping(Default::default()))).await, Ok(Ok(()))) { break; }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    last_seen = Instant::now();
                    let mut rooms = rooms.lock().unwrap();
                    let Some(room) = rooms.get_mut(&code).filter(|r| owns(r)) else { break };
                    let result = serde_json::from_str(&text).map_err(|_| "protocol").and_then(|cmd| room.apply(seat, cmd));
                    if let Err(reason) = result { end(&mut rooms, &code, reason); }
                }
                Some(Ok(Message::Pong(_) | Message::Ping(_))) => { last_seen = Instant::now(); }
                _ => break,
            }
        }
    }
    let mut rooms = rooms.lock().unwrap();
    if let Some(room) = rooms.get_mut(&code).filter(|r| owns(r)) {
        let player = room.seats[seat].as_mut().unwrap();
        player.connection = None;
        player.lost = Some(Instant::now());
        let _ = room.apply(seat, Command::Pause);
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let rooms: Rooms = Default::default();
    let registry = rooms.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let mut rooms = registry.lock().unwrap();
            let expired: Vec<_> = rooms
                .iter()
                .filter(|(_, r)| {
                    r.seats
                        .iter()
                        .flatten()
                        .any(|s| s.lost.is_some_and(|t| t.elapsed() >= GRACE))
                })
                .map(|(k, _)| k.clone())
                .collect();
            for code in expired {
                end(&mut rooms, &code, "ended");
            }
        }
    });
    let permits = Arc::new(Semaphore::new(600));
    let app = Router::new()
        .route(
            "/netplay",
            get(move |ws: WebSocketUpgrade, State(rooms): State<Rooms>| {
                let permit = permits.clone().try_acquire_owned();
                async move {
                    let permit = permit.map_err(|_| axum::http::StatusCode::SERVICE_UNAVAILABLE)?;
                    Ok::<_, axum::http::StatusCode>(
                        ws.max_message_size(MAX_MESSAGE)
                            .max_frame_size(MAX_MESSAGE)
                            .on_upgrade(move |socket| async move {
                                let _permit = permit;
                                serve(socket, rooms).await
                            }),
                    )
                }
            }),
        )
        .with_state(rooms);
    let address = std::env::var("SOL_ROOMS_LISTEN").unwrap_or_else(|_| "127.0.0.1:8788".into());
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!("Sol rooms listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> (Connection, mpsc::Receiver<Event>) {
        let (out, events) = mpsc::channel(16);
        let (stop, _) = watch::channel(false);
        (Connection { out, stop }, events)
    }

    #[test]
    fn joining_checks_the_build_and_reconnecting_preserves_the_original_seat() {
        let mut rooms = HashMap::new();
        let core = "a".repeat(64);
        let game = "b".repeat(40);
        let (host, _host_events) = connection();
        let (code, _) = enter(
            &mut rooms,
            Hello::Create {
                version: VERSION,
                core: core.clone(),
                game: game.clone(),
                name: "Two pads".into(),
            },
            host,
        )
        .unwrap();
        let join = |build| Hello::Join {
            version: VERSION,
            core: build,
            code: code.clone(),
        };
        let (guest, mut events) = connection();
        assert_eq!(
            enter(&mut rooms, join("c".repeat(64)), guest.clone()),
            Err("version")
        );
        assert_eq!(
            enter(&mut rooms, join(core.clone()), guest),
            Ok((code.clone(), 1))
        );
        let Event::Seat { token, .. } = events.try_recv().unwrap() else {
            panic!("missing seat");
        };
        let (new, _new_events) = connection();
        assert_eq!(enter(&mut rooms, join(core), new.clone()), Err("full"));
        let resume = |token| Hello::Resume {
            version: VERSION,
            code: code.clone(),
            token,
        };
        assert_eq!(
            enter(&mut rooms, resume("other".into()), new.clone()),
            Err("missing")
        );
        assert_eq!(enter(&mut rooms, resume(token), new), Ok((code.clone(), 1)));
        assert_eq!(
            rooms.get_mut(&code).unwrap().apply(
                1,
                Command::Ready {
                    game: "another ROM".into()
                }
            ),
            Err("game")
        );
    }
}
