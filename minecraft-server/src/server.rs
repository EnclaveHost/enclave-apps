//! All connection and game state, single-threaded (wasm32-wasip2 has no
//! threads). Protocol 47 (Minecraft 1.8.x), offline mode, no compression:
//! handshake → status (server-list ping) or login → play.
//!
//! Play state is deliberately small: creative mode, a procedurally generated
//! hill world streamed in rings around each player, position/chat/block-edit
//! sync between players, keep-alive liveness. No inventories, mobs, physics
//! or persistence — the platform's sandbox has no disk, so the world lives
//! and dies with the deployment.

use crate::proto::{self, P, R};
use crate::world::{self, World};
use crate::{MAX_PLAYERS, VIEW_DISTANCE};
use std::collections::{HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const MAX_CONNS: usize = 64;
const INBUF_MAX: usize = 64 * 1024;
const FRAME_MAX: usize = 32 * 1024; // vanilla caps serverbound packets at 32 KiB
const OUTBUF_MAX: usize = 8 * 1024 * 1024;
const OUTBUF_COMPACT: usize = 256 * 1024;
const CHUNKS_PER_TICK: usize = 6;
const CHUNK_BACKPRESSURE: usize = 512 * 1024;
const KEEPALIVE_EVERY: Duration = Duration::from_secs(10);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const PREPLAY_TIMEOUT: Duration = Duration::from_secs(20);
const REACH_SQ: f64 = 36.0; // creative reach, squared

const PROTOCOL: i32 = 47;
const SERVER_BRAND: &str = "minecraft-server 1.8.9";

#[derive(PartialEq)]
enum State {
    Handshake,
    Status,
    Login,
    Play,
}

struct Conn {
    stream: TcpStream,
    state: State,
    protocol: i32,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    outpos: usize,
    dead: bool,
    connected_at: Instant,
    // Play-state fields (meaningless before login completes):
    name: String,
    uuid: [u8; 16],
    eid: i32,
    x: f64,
    y: f64,
    z: f64,
    yaw: f32,
    pitch: f32,
    on_ground: bool,
    center: (i32, i32),
    loaded: HashSet<(i32, i32)>,
    queue: VecDeque<(i32, i32)>,
    last_ka_sent: Instant,
    last_ka_ack: Instant,
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        let now = Instant::now();
        Conn {
            stream,
            state: State::Handshake,
            protocol: 0,
            inbuf: Vec::new(),
            outbuf: Vec::new(),
            outpos: 0,
            dead: false,
            connected_at: now,
            name: String::new(),
            uuid: [0; 16],
            eid: 0,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
            center: (0, 0),
            loaded: HashSet::new(),
            queue: VecDeque::new(),
            last_ka_sent: now,
            last_ka_ack: now,
        }
    }

    fn playing(&self) -> bool {
        self.state == State::Play && !self.dead
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        if self.dead {
            return;
        }
        if self.outbuf.len() - self.outpos + bytes.len() > OUTBUF_MAX {
            self.dead = true; // reader too slow to be worth carrying
            return;
        }
        self.outbuf.extend_from_slice(bytes);
    }

    fn send(&mut self, p: &P) {
        self.send_raw(&p.frame());
    }

    fn backlog(&self) -> usize {
        self.outbuf.len() - self.outpos
    }
}

pub struct Server {
    conns: Vec<Conn>,
    world: World,
    next_eid: i32,
    ka_counter: i32,
}

fn chat_json(text: &str, color: &str) -> String {
    format!(r#"{{"text":"{}","color":"{}"}}"#, proto::json_escape(text), color)
}

fn chunk_of(v: f64) -> i32 {
    (v.floor() as i32).div_euclid(16)
}

/// Chunk coordinates within VIEW of center, nearest first.
fn ring(center: (i32, i32)) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(((VIEW_DISTANCE * 2 + 1) * (VIEW_DISTANCE * 2 + 1)) as usize);
    for dx in -VIEW_DISTANCE..=VIEW_DISTANCE {
        for dz in -VIEW_DISTANCE..=VIEW_DISTANCE {
            out.push((center.0 + dx, center.1 + dz));
        }
    }
    out.sort_by_key(|&(cx, cz)| {
        let (dx, dz) = ((cx - center.0) as i64, (cz - center.1) as i64);
        dx * dx + dz * dz
    });
    out
}

impl Server {
    pub fn new() -> Self {
        let (sx, sy, sz) = World::spawn();
        println!("world spawn at ({:.1}, {:.1}, {:.1}); creative, protocol {}", sx, sy, sz, PROTOCOL);
        Server { conns: Vec::new(), world: World::new(), next_eid: 1, ka_counter: 0 }
    }

    pub fn accept(&mut self, stream: TcpStream, peer: SocketAddr) {
        if self.conns.len() >= MAX_CONNS {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let _ = peer; // loopback-only; the bridge is the real peer
        self.conns.push(Conn::new(stream));
    }

    /// Read every socket, parse and dispatch complete frames.
    pub fn pump(&mut self) -> bool {
        let mut busy = false;
        let mut tmp = [0u8; 4096];
        for i in 0..self.conns.len() {
            loop {
                if self.conns[i].dead {
                    break;
                }
                match self.conns[i].stream.read(&mut tmp) {
                    Ok(0) => {
                        self.conns[i].dead = true;
                        break;
                    }
                    Ok(n) => {
                        busy = true;
                        let c = &mut self.conns[i];
                        if c.inbuf.len() + n > INBUF_MAX {
                            c.dead = true;
                            break;
                        }
                        c.inbuf.extend_from_slice(&tmp[..n]);
                        if n < tmp.len() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        self.conns[i].dead = true;
                        break;
                    }
                }
            }
            // Dispatch complete frames.
            while !self.conns[i].dead {
                // Legacy (pre-Netty) server-list ping starts with 0xFE, which
                // is not a valid modern frame; drop those connections.
                if self.conns[i].state == State::Handshake && self.conns[i].inbuf.first() == Some(&0xFE) {
                    self.conns[i].dead = true;
                    break;
                }
                let frame = match proto::split_frame(&self.conns[i].inbuf, FRAME_MAX) {
                    Ok(Some((s, e))) => {
                        let f = self.conns[i].inbuf[s..e].to_vec();
                        self.conns[i].inbuf.drain(..e);
                        f
                    }
                    Ok(None) => break,
                    Err(()) => {
                        self.conns[i].dead = true;
                        break;
                    }
                };
                busy = true;
                if self.handle_frame(i, &frame).is_err() {
                    self.conns[i].dead = true;
                }
            }
        }
        busy
    }

    fn handle_frame(&mut self, i: usize, frame: &[u8]) -> Result<(), ()> {
        let mut r = R::new(frame);
        let id = r.varint()?;
        match self.conns[i].state {
            State::Handshake => self.on_handshake(i, id, &mut r),
            State::Status => self.on_status(i, id, &mut r),
            State::Login => self.on_login(i, id, &mut r),
            State::Play => self.on_play(i, id, &mut r),
        }
    }

    // ---- handshake / status / login -------------------------------------

    fn on_handshake(&mut self, i: usize, id: i32, r: &mut R) -> Result<(), ()> {
        if id != 0x00 {
            return Err(());
        }
        let protocol = r.varint()?;
        let _host = r.string(255)?;
        let _port = r.u16()?;
        let next = r.varint()?;
        let c = &mut self.conns[i];
        c.protocol = protocol;
        c.state = match next {
            1 => State::Status,
            2 => State::Login,
            _ => return Err(()),
        };
        Ok(())
    }

    fn on_status(&mut self, i: usize, id: i32, r: &mut R) -> Result<(), ()> {
        match id {
            0x00 => {
                let online = self.conns.iter().filter(|c| c.playing()).count();
                let json = format!(
                    r#"{{"version":{{"name":"{}","protocol":{}}},"players":{{"max":{},"online":{}}},"description":{{"text":"minecraft-server — creative world in a confidential Enclave"}}}}"#,
                    SERVER_BRAND, PROTOCOL, MAX_PLAYERS, online
                );
                let mut p = P::new(0x00);
                p.string(&json);
                self.conns[i].send(&p);
            }
            0x01 => {
                let payload = r.i64()?;
                let mut p = P::new(0x01);
                p.i64(payload);
                self.conns[i].send(&p);
            }
            _ => return Err(()),
        }
        Ok(())
    }

    fn login_fail(&mut self, i: usize, msg: &str) {
        let mut p = P::new(0x00); // login-state Disconnect
        p.string(&chat_json(msg, "red"));
        self.conns[i].send(&p);
        self.conns[i].dead = true;
    }

    fn on_login(&mut self, i: usize, id: i32, r: &mut R) -> Result<(), ()> {
        if id != 0x00 {
            return Err(());
        }
        let name = r.string(16)?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            self.login_fail(i, "Invalid username.");
            return Ok(());
        }
        if self.conns[i].protocol != PROTOCOL {
            self.login_fail(i, "This server speaks Minecraft 1.8.x (protocol 47). Please use a 1.8.9 client.");
            return Ok(());
        }
        if self.conns.iter().filter(|c| c.playing()).count() >= MAX_PLAYERS {
            self.login_fail(i, "Server is full.");
            return Ok(());
        }
        if self.conns.iter().any(|c| c.playing() && c.name.eq_ignore_ascii_case(&name)) {
            self.login_fail(i, "That name is already playing.");
            return Ok(());
        }
        self.join(i, name);
        Ok(())
    }

    // ---- join sequence ----------------------------------------------------

    fn join(&mut self, i: usize, name: String) {
        let uuid = proto::offline_uuid(&name);
        let eid = self.next_eid;
        self.next_eid += 1;
        let (sx, sy, sz) = World::spawn();

        {
            let c = &mut self.conns[i];
            c.name = name.clone();
            c.uuid = uuid;
            c.eid = eid;
            c.x = sx;
            c.y = sy;
            c.z = sz;
            c.state = State::Play;
            c.last_ka_ack = Instant::now();

            let mut p = P::new(0x02); // Login Success (still login-state framing)
            p.string(&proto::uuid_string(&uuid)).string(&name);
            c.send(&p);

            let mut p = P::new(0x01); // Join Game
            p.i32(eid)
                .u8(1) // creative
                .i8(0) // overworld
                .u8(0) // peaceful
                .u8(MAX_PLAYERS as u8)
                .string("default")
                .bool(false);
            c.send(&p);

            let mut p = P::new(0x39); // Player Abilities: invulnerable + may-fly + creative
            p.i8(0x0D).f32(0.05).f32(0.1);
            c.send(&p);

            let mut p = P::new(0x03); // Time Update: noon, frozen (negative = no cycle)
            p.i64(0).i64(-6000);
            c.send(&p);

            let mut p = P::new(0x05); // Spawn Position (compass target)
            p.position(sx.floor() as i32, sy as i32, sz.floor() as i32);
            c.send(&p);

            let mut p = P::new(0x06); // Update Health
            p.f32(20.0).varint(20).f32(5.0);
            c.send(&p);
        }

        // Tab list: everyone (including the newcomer) to the newcomer, and the
        // newcomer to everyone else. List entries must precede Spawn Player.
        let add_new = Self::list_add(&uuid, &name);
        for j in 0..self.conns.len() {
            if j != i && self.conns[j].playing() {
                let entry = Self::list_add(&self.conns[j].uuid, &self.conns[j].name);
                self.conns[i].send_raw(&entry);
                self.conns[j].send_raw(&add_new);
            }
        }
        self.conns[i].send_raw(&add_new);

        // Ground beneath the player before the teleport, so the client leaves
        // the "Loading terrain" screen on solid ground; the rest streams.
        let center = (chunk_of(sx), chunk_of(sz));
        for &(cx, cz) in ring(center).iter().take(9) {
            let f = self.world.chunk_frame(cx, cz);
            let c = &mut self.conns[i];
            c.send_raw(&f);
            c.loaded.insert((cx, cz));
        }
        self.conns[i].center = center;
        self.retarget_chunks(i);

        {
            let c = &mut self.conns[i];
            let mut p = P::new(0x08); // Player Position And Look (absolute)
            p.f64(sx).f64(sy).f64(sz).f32(0.0).f32(0.0).i8(0);
            c.send(&p);
        }

        // Entity spawns, both directions.
        let spawn_new = Self::spawn_player(&self.conns[i]);
        for j in 0..self.conns.len() {
            if j != i && self.conns[j].playing() {
                let spawn_other = Self::spawn_player(&self.conns[j]);
                self.conns[i].send_raw(&spawn_other);
                self.conns[j].send_raw(&spawn_new);
            }
        }

        let mut hello = P::new(0x02);
        hello.string(&chat_json(
            "Welcome to minecraft-server — an ephemeral creative world inside a confidential Enclave. Nothing is saved.",
            "green",
        ))
        .i8(0);
        self.conns[i].send(&hello);

        let joined = format!("{} joined the game", name);
        println!("+ {} (eid {})", name, eid);
        let mut p = P::new(0x02);
        p.string(&chat_json(&joined, "yellow")).i8(0);
        self.broadcast(None, &p.frame());
    }

    fn list_add(uuid: &[u8; 16], name: &str) -> Vec<u8> {
        let mut p = P::new(0x38); // Player List Item
        p.varint(0) // action: add
            .varint(1)
            .raw(uuid)
            .string(name)
            .varint(0) // no properties (no skin signature in offline mode)
            .varint(1) // gamemode: creative
            .varint(0) // ping
            .bool(false); // no display name
        p.frame()
    }

    fn list_remove(uuid: &[u8; 16]) -> Vec<u8> {
        let mut p = P::new(0x38);
        p.varint(4).varint(1).raw(uuid);
        p.frame()
    }

    fn spawn_player(c: &Conn) -> Vec<u8> {
        let mut p = P::new(0x0C); // Spawn Player
        p.varint(c.eid)
            .raw(&c.uuid)
            .fixed(c.x)
            .fixed(c.y)
            .fixed(c.z)
            .angle(c.yaw)
            .angle(c.pitch)
            .i16(0); // held item: empty
        // Metadata: the 1.8 client needs at least health to render a player.
        p.u8(0x66).f32(20.0); // (type float << 5) | index 6: health
        p.u8(0x0A).u8(0x7F); // (type byte << 5) | index 10: all skin parts
        p.u8(0x7F); // terminator
        p.frame()
    }

    // ---- play-state packets ------------------------------------------------

    fn on_play(&mut self, i: usize, id: i32, r: &mut R) -> Result<(), ()> {
        match id {
            0x00 => {
                let _id = r.varint()?;
                self.conns[i].last_ka_ack = Instant::now();
            }
            0x01 => {
                let msg = r.string(256)?;
                self.on_chat(i, msg);
            }
            0x03 => {
                self.conns[i].on_ground = r.bool()?;
            }
            0x04 => {
                let (x, y, z) = (r.f64()?, r.f64()?, r.f64()?);
                let g = r.bool()?;
                self.on_move(i, x, y, z, None, g);
            }
            0x05 => {
                let (yaw, pitch) = (r.f32()?, r.f32()?);
                let g = r.bool()?;
                let (x, y, z) = {
                    let c = &self.conns[i];
                    (c.x, c.y, c.z)
                };
                self.on_move(i, x, y, z, Some((yaw, pitch)), g);
            }
            0x06 => {
                let (x, y, z) = (r.f64()?, r.f64()?, r.f64()?);
                let (yaw, pitch) = (r.f32()?, r.f32()?);
                let g = r.bool()?;
                self.on_move(i, x, y, z, Some((yaw, pitch)), g);
            }
            0x07 => {
                let status = r.i8()?;
                let pos = r.u64()?;
                let _face = r.i8()?;
                if status == 0 || status == 2 {
                    let (x, y, z) = proto::unpack_position(pos);
                    self.on_dig(i, x, y, z);
                }
            }
            0x08 => {
                let pos = r.u64()?;
                let face = r.i8()?;
                let slot = proto::read_slot(r)?;
                // trailing cursor x/y/z bytes ignored
                if pos != u64::MAX {
                    let (x, y, z) = proto::unpack_position(pos);
                    self.on_place(i, x, y, z, face, slot);
                }
            }
            0x0A => {
                // Arm swing: relay to everyone in sight.
                let eid = self.conns[i].eid;
                let mut p = P::new(0x0B);
                p.varint(eid).u8(0);
                self.broadcast(Some(i), &p.frame());
            }
            // Everything else (inventory clicks, settings, plugin channels,
            // tab-complete, entity actions, ...) is irrelevant to this world.
            _ => {}
        }
        Ok(())
    }

    fn on_chat(&mut self, i: usize, msg: String) {
        let msg: String = msg.chars().filter(|c| !c.is_control()).take(100).collect();
        if msg.is_empty() {
            return;
        }
        if msg.starts_with('/') {
            let mut p = P::new(0x02);
            p.string(&chat_json("Commands are not supported on this server.", "gray")).i8(0);
            self.conns[i].send(&p);
            return;
        }
        let json = format!(
            r#"{{"translate":"chat.type.text","with":[{{"text":"{}"}},{{"text":"{}"}}]}}"#,
            proto::json_escape(&self.conns[i].name),
            proto::json_escape(&msg)
        );
        println!("<{}> {}", self.conns[i].name, msg);
        let mut p = P::new(0x02);
        p.string(&json).i8(0);
        self.broadcast(None, &p.frame());
    }

    fn on_move(&mut self, i: usize, x: f64, y: f64, z: f64, look: Option<(f32, f32)>, on_ground: bool) {
        // Reject NaN/absurd coordinates rather than propagating them.
        if !x.is_finite() || !y.is_finite() || !z.is_finite() || x.abs() > 3.0e7 || z.abs() > 3.0e7 {
            return;
        }
        {
            let c = &mut self.conns[i];
            c.x = x;
            c.y = y;
            c.z = z;
            if let Some((yaw, pitch)) = look {
                c.yaw = yaw;
                c.pitch = pitch;
            }
            c.on_ground = on_ground;
        }
        let (eid, yaw) = (self.conns[i].eid, self.conns[i].yaw);
        let mut tp = P::new(0x18); // Entity Teleport (absolute — no drift)
        tp.varint(eid)
            .fixed(x)
            .fixed(y)
            .fixed(z)
            .angle(yaw)
            .angle(self.conns[i].pitch)
            .bool(on_ground);
        self.broadcast(Some(i), &tp.frame());
        let mut hl = P::new(0x19); // Entity Head Look
        hl.varint(eid).angle(yaw);
        self.broadcast(Some(i), &hl.frame());

        let center = (chunk_of(x), chunk_of(z));
        if center != self.conns[i].center {
            self.conns[i].center = center;
            self.retarget_chunks(i);
        }
    }

    fn in_reach(&self, i: usize, x: i32, y: i32, z: i32) -> bool {
        let c = &self.conns[i];
        let (dx, dy, dz) = (
            x as f64 + 0.5 - c.x,
            y as f64 + 0.5 - (c.y + 1.62),
            z as f64 + 0.5 - c.z,
        );
        dx * dx + dy * dy + dz * dz <= REACH_SQ
    }

    /// Tell one client the server's truth about a block (prediction rollback).
    fn revert(&mut self, i: usize, x: i32, y: i32, z: i32) {
        let state = self.world.block_at(x, y, z);
        let mut p = P::new(0x23);
        p.position(x, y, z).varint(state as i32);
        self.conns[i].send(&p);
    }

    fn apply_block(&mut self, x: i32, y: i32, z: i32, state: u16) {
        self.world.set_block(x, y, z, state);
        let mut p = P::new(0x23);
        p.position(x, y, z).varint(state as i32);
        self.broadcast(None, &p.frame());
    }

    fn on_dig(&mut self, i: usize, x: i32, y: i32, z: i32) {
        if !(0..256).contains(&y) || !self.in_reach(i, x, y, z) {
            self.revert(i, x, y, z);
            return;
        }
        if self.world.block_at(x, y, z) == world::AIR {
            return;
        }
        self.apply_block(x, y, z, world::AIR);
    }

    fn on_place(&mut self, i: usize, x: i32, y: i32, z: i32, face: i8, slot: Option<(i16, u8, i16)>) {
        let clicked = self.world.block_at(x, y, z);
        // Tall grass and flowers are replaced in place; everything else
        // offsets by the clicked face.
        let (tx, ty, tz) = if matches!(clicked, world::TALL_GRASS | world::DANDELION | world::POPPY) {
            (x, y, z)
        } else {
            match face {
                0 => (x, y - 1, z),
                1 => (x, y + 1, z),
                2 => (x, y, z - 1),
                3 => (x, y, z + 1),
                4 => (x - 1, y, z),
                5 => (x + 1, y, z),
                _ => {
                    self.revert(i, x, y, z);
                    return;
                }
            }
        };
        let ok = matches!(slot, Some((id, _, _)) if (1..=197).contains(&id))
            && (1..256).contains(&ty)
            && self.in_reach(i, tx, ty, tz)
            && matches!(self.world.block_at(tx, ty, tz), world::AIR | world::TALL_GRASS | world::DANDELION | world::POPPY);
        if !ok {
            self.revert(i, x, y, z);
            self.revert(i, tx, ty, tz);
            return;
        }
        let (id, _, damage) = slot.unwrap();
        self.apply_block(tx, ty, tz, ((id as u16) << 4) | (damage as u16 & 15));
    }

    // ---- chunk streaming / timers -------------------------------------------

    fn retarget_chunks(&mut self, i: usize) {
        let center = self.conns[i].center;
        let want = ring(center);
        let c = &mut self.conns[i];
        c.queue = want.into_iter().filter(|p| !c.loaded.contains(p)).collect();
        let far: Vec<(i32, i32)> = c
            .loaded
            .iter()
            .filter(|&&(cx, cz)| {
                (cx - center.0).abs() > VIEW_DISTANCE + 1 || (cz - center.1).abs() > VIEW_DISTANCE + 1
            })
            .cloned()
            .collect();
        for (cx, cz) in far {
            c.loaded.remove(&(cx, cz));
            let f = World::unload_frame(cx, cz);
            c.send_raw(&f);
        }
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        for i in 0..self.conns.len() {
            if self.conns[i].dead {
                continue;
            }
            if self.conns[i].state != State::Play {
                if now.duration_since(self.conns[i].connected_at) > PREPLAY_TIMEOUT {
                    self.conns[i].dead = true;
                }
                continue;
            }
            // Liveness.
            if now.duration_since(self.conns[i].last_ka_ack) > KEEPALIVE_TIMEOUT {
                self.kick(i, "Timed out");
                continue;
            }
            if now.duration_since(self.conns[i].last_ka_sent) >= KEEPALIVE_EVERY {
                self.ka_counter = self.ka_counter.wrapping_add(1);
                let ka = self.ka_counter;
                let c = &mut self.conns[i];
                c.last_ka_sent = now;
                let mut p = P::new(0x00);
                p.varint(ka);
                c.send(&p);
            }
            // Stream queued chunks, a few per tick, only while the socket
            // is draining (backpressure instead of unbounded buffering).
            let mut sent = 0;
            while sent < CHUNKS_PER_TICK && self.conns[i].backlog() < CHUNK_BACKPRESSURE {
                let Some((cx, cz)) = self.conns[i].queue.pop_front() else {
                    break;
                };
                if self.conns[i].loaded.contains(&(cx, cz)) {
                    continue;
                }
                let f = self.world.chunk_frame(cx, cz);
                let c = &mut self.conns[i];
                c.send_raw(&f);
                c.loaded.insert((cx, cz));
                sent += 1;
            }
        }
    }

    fn kick(&mut self, i: usize, msg: &str) {
        let mut p = P::new(0x40); // play-state Disconnect
        p.string(&chat_json(msg, "red"));
        self.conns[i].send(&p);
        self.conns[i].dead = true;
    }

    fn broadcast(&mut self, except: Option<usize>, bytes: &[u8]) {
        for j in 0..self.conns.len() {
            if Some(j) == except || !self.conns[j].playing() {
                continue;
            }
            self.conns[j].send_raw(bytes);
        }
    }

    /// Drain outbufs (non-blocking; keeps the remainder on WouldBlock).
    pub fn flush(&mut self) {
        for c in &mut self.conns {
            if c.dead {
                continue;
            }
            while c.outpos < c.outbuf.len() {
                match c.stream.write(&c.outbuf[c.outpos..]) {
                    Ok(0) => {
                        c.dead = true;
                        break;
                    }
                    Ok(n) => c.outpos += n,
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                    Err(_) => {
                        c.dead = true;
                        break;
                    }
                }
            }
            if c.outpos > 0 && c.outpos == c.outbuf.len() {
                c.outbuf.clear();
                c.outpos = 0;
            } else if c.outpos > OUTBUF_COMPACT {
                c.outbuf.drain(..c.outpos);
                c.outpos = 0;
            }
        }
    }

    /// Drop dead connections and announce departed players.
    pub fn reap(&mut self) {
        let mut gone: Vec<(i32, [u8; 16], String)> = Vec::new();
        let mut j = 0;
        while j < self.conns.len() {
            if !self.conns[j].dead {
                j += 1;
                continue;
            }
            let c = self.conns.swap_remove(j);
            // Best-effort delivery of any final packet (kick/disconnect).
            if c.outpos < c.outbuf.len() {
                let mut s = c.stream;
                let _ = s.write(&c.outbuf[c.outpos..]);
                let _ = s.shutdown(Shutdown::Both);
            } else {
                let _ = c.stream.shutdown(Shutdown::Both);
            }
            if c.state == State::Play {
                gone.push((c.eid, c.uuid, c.name));
            }
        }
        for (eid, uuid, name) in gone {
            println!("- {} (eid {})", name, eid);
            let mut destroy = P::new(0x13);
            destroy.varint(1).varint(eid);
            self.broadcast(None, &destroy.frame());
            self.broadcast(None, &Self::list_remove(&uuid));
            let mut p = P::new(0x02);
            p.string(&chat_json(&format!("{} left the game", name), "yellow")).i8(0);
            self.broadcast(None, &p.frame());
        }
    }
}
