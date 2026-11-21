use macroquad::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// Версии Minecraft Java Edition: (название, номер протокола).
// Номер должен совпадать с версией сервера. 775 проверен, остальные из памяти.
const VERSIONS: [(&str, i32); 4] = [
    ("26.1.2", 775),
    ("1.21.11", 774),
    ("1.21.4", 769),
    ("1.21.1", 767),
];

// Список серверов. Поменяйте адреса на свои прямо здесь.
const SERVERS: [&str; 3] = ["127.0.0.1:25565", "192.168.0.10:25565", "your.server.com:25565"];

// ======================= СЕТЬ =======================
mod net {
    use flate2::read::ZlibDecoder;
    use std::io::{self, Cursor, Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub type Log = Arc<Mutex<Vec<String>>>;

    fn say(log: &Log, s: impl Into<String>) {
        log.lock().unwrap().push(s.into());
    }

    fn write_varint(buf: &mut Vec<u8>, mut v: u32) {
        loop {
            let mut b = (v & 0x7F) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            buf.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn read_varint(r: &mut impl Read) -> io::Result<i32> {
        let mut n = 0u32;
        for i in 0..5 {
            let mut b = [0u8];
            r.read_exact(&mut b)?;
            n |= ((b[0] & 0x7F) as u32) << (7 * i);
            if b[0] & 0x80 == 0 {
                return Ok(n as i32);
            }
        }
        Err(io::Error::new(io::ErrorKind::InvalidData, "bad VarInt"))
    }

    fn write_string(buf: &mut Vec<u8>, s: &str) {
        write_varint(buf, s.len() as u32);
        buf.extend_from_slice(s.as_bytes());
    }

    fn read_string(r: &mut impl Read) -> io::Result<String> {
        let len = read_varint(r)? as usize;
        let mut v = vec![0u8; len];
        r.read_exact(&mut v)?;
        String::from_utf8(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    struct Conn {
        stream: TcpStream,
        compression: bool,
    }

    impl Conn {
        fn send(&mut self, id: u32, body: &[u8]) -> io::Result<()> {
            let mut pkt = vec![];
            write_varint(&mut pkt, id);
            pkt.extend_from_slice(body);
            let mut out = vec![];
            if self.compression {
                let mut inner = vec![];
                write_varint(&mut inner, 0); // Data Length = 0: без сжатия
                inner.extend_from_slice(&pkt);
                write_varint(&mut out, inner.len() as u32);
                out.extend_from_slice(&inner);
            } else {
                write_varint(&mut out, pkt.len() as u32);
                out.extend_from_slice(&pkt);
            }
            self.stream.write_all(&out)
        }

        fn recv(&mut self) -> io::Result<(i32, Vec<u8>)> {
            let len = read_varint(&mut self.stream)? as usize;
            let mut data = vec![0u8; len];
            self.stream.read_exact(&mut data)?;
            let payload = if self.compression {
                let mut cur = Cursor::new(data);
                let data_len = read_varint(&mut cur)? as usize;
                let pos = cur.position() as usize;
                let rest = &cur.get_ref()[pos..];
                if data_len == 0 {
                    rest.to_vec()
                } else {
                    let mut out = Vec::with_capacity(data_len);
                    ZlibDecoder::new(rest).read_to_end(&mut out)?;
                    out
                }
            } else {
                data
            };
            let mut c = Cursor::new(payload);
            let id = read_varint(&mut c)?;
            let pos = c.position() as usize;
            Ok((id, c.into_inner()[pos..].to_vec()))
        }

        fn handshake(&mut self, host: &str, port: u16, protocol: i32, next: u32) -> io::Result<()> {
            let mut b = vec![];
            write_varint(&mut b, protocol as u32);
            write_string(&mut b, host);
            b.extend_from_slice(&port.to_be_bytes());
            write_varint(&mut b, next);
            self.send(0x00, &b)
        }
    }

    fn connect(addr: &str) -> io::Result<(Conn, String, u16)> {
        let (host, port) = match addr.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(25565)),
            None => (addr.to_string(), 25565),
        };
        let stream = TcpStream::connect((host.as_str(), port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok((Conn { stream, compression: false }, host, port))
    }

    fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
        let i = s.find(a)? + a.len();
        let j = s[i..].find(b)?;
        Some(&s[i..i + j])
    }

    fn number_after<'a>(s: &'a str, a: &str) -> Option<&'a str> {
        let i = s.find(a)? + a.len();
        let t = &s[i..];
        let n = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
        Some(&t[..n])
    }

    pub fn status(addr: &str, protocol: i32, log: &Log) {
        if let Err(e) = status_inner(addr, protocol, log) {
            say(log, format!("Error: {e}"));
        }
    }

    fn status_inner(addr: &str, protocol: i32, log: &Log) -> io::Result<()> {
        say(log, format!("Connecting to {addr}..."));
        let (mut c, host, port) = connect(addr)?;
        c.handshake(&host, port, protocol, 1)?;
        c.send(0x00, &[])?;
        let (id, body) = c.recv()?;
        if id != 0x00 {
            say(log, format!("Unexpected packet 0x{id:02X}"));
            return Ok(());
        }
        let json = read_string(&mut Cursor::new(body))?;
        let ver = between(&json, "\"version\":{\"name\":\"", "\"").unwrap_or("?");
        let online = number_after(&json, "\"online\":").unwrap_or("?");
        say(log, format!("Server version: {ver}"));
        say(log, format!("Players online: {online}"));
        Ok(())
    }

    pub fn login(addr: &str, protocol: i32, name: &str, log: &Log) {
        if let Err(e) = login_inner(addr, protocol, name, log) {
            say(log, format!("Error: {e}"));
        }
    }

    fn login_inner(addr: &str, protocol: i32, name: &str, log: &Log) -> io::Result<()> {
        say(log, format!("Connecting to {addr}..."));
        let (mut c, host, port) = connect(addr)?;
        c.handshake(&host, port, protocol, 2)?;
        let mut b = vec![];
        write_string(&mut b, name);
        b.extend_from_slice(&[0u8; 16]);
        c.send(0x00, &b)?;
        loop {
            let (id, body) = c.recv()?;
            match id {
                0x00 => {
                    let r = read_string(&mut Cursor::new(body))?;
                    say(log, format!("Rejected: {r}"));
                    return Ok(());
                }
                0x01 => {
                    say(log, "Online-mode server: Microsoft login needed (not supported yet)");
                    return Ok(());
                }
                0x03 => {
                    let t = read_varint(&mut Cursor::new(body))?;
                    c.compression = t >= 0;
                }
                0x02 => {
                    say(log, format!("Logged in as {name}"));
                    c.send(0x03, &[])?; // Login Acknowledged -> Configuration
                    break;
                }
                _ => {}
            }
        }
        for _ in 0..5 {
            match c.recv() {
                Ok((id, body)) => say(log, format!("Config packet 0x{id:02X} ({} bytes)", body.len())),
                Err(e) => {
                    say(log, format!("Stopped: {e}"));
                    break;
                }
            }
        }
        say(log, "Done (world loading is not implemented yet)");
        Ok(())
    }
}

// ======================= UI =======================

fn tapped(r: Rect) -> bool {
    for t in touches() {
        if matches!(t.phase, TouchPhase::Started) && r.contains(t.position) {
            return true;
        }
    }
    is_mouse_button_pressed(MouseButton::Left) && r.contains(Vec2::from(mouse_position()))
}

fn button(r: Rect, label: &str, fs: f32) -> bool {
    draw_rectangle(r.x, r.y, r.w, r.h, Color::new(0.18, 0.38, 0.2, 1.0));
    draw_rectangle_lines(r.x, r.y, r.w, r.h, 3.0, Color::new(0.6, 0.9, 0.6, 1.0));
    let d = measure_text(label, None, fs as u16, 1.0);
    draw_text(label, r.x + (r.w - d.width) / 2.0, r.y + (r.h + d.height) / 2.0, fs, WHITE);
    tapped(r)
}

fn rand_nick() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("Player{}", t % 9000 + 1000)
}

struct Menu {
    ver: usize,
    srv: usize,
    nick: String,
    log: net::Log,
    busy: Arc<AtomicBool>,
}

impl Menu {
    fn new() -> Self {
        Menu {
            ver: 0,
            srv: 0,
            nick: rand_nick(),
            log: Arc::new(Mutex::new(vec!["Pick a version and a server".to_string()])),
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start(&mut self, join: bool) {
        self.log.lock().unwrap().clear();
        self.busy.store(true, Ordering::SeqCst);
        let (log, busy) = (self.log.clone(), self.busy.clone());
        let addr = SERVERS[self.srv].to_string();
        let proto = VERSIONS[self.ver].1;
        let nick = self.nick.clone();
        std::thread::spawn(move || {
            if join {
                net::login(&addr, proto, &nick, &log);
            } else {
                net::status(&addr, proto, &log);
            }
            busy.store(false, Ordering::SeqCst);
        });
    }

    /// Возвращает true, если нажали "Singleplayer".
    fn frame(&mut self) -> bool {
        clear_background(Color::new(0.08, 0.1, 0.14, 1.0));
        let (sw, sh) = (screen_width(), screen_height());
        let m = sw * 0.06;
        let bw = sw - 2.0 * m;
        let bh = (sh * 0.075).max(40.0);
        let gap = bh * 0.25;
        let fs = bh * 0.42;
        let mut y = sh * 0.04;

        draw_text("JelikLauncher", m, y + fs * 1.4, fs * 1.4, WHITE);
        y += bh * 1.4;

        let busy = self.busy.load(Ordering::SeqCst);
        let (vn, vp) = VERSIONS[self.ver];
        if button(Rect::new(m, y, bw, bh), &format!("Version: {vn} ({vp})"), fs) {
            self.ver = (self.ver + 1) % VERSIONS.len();
        }
        y += bh + gap;
        if button(Rect::new(m, y, bw, bh), &format!("Server: {}", SERVERS[self.srv]), fs) {
            self.srv = (self.srv + 1) % SERVERS.len();
        }
        y += bh + gap;
        if button(Rect::new(m, y, bw, bh), &format!("Nick: {} (tap: new)", self.nick), fs) {
            self.nick = rand_nick();
        }
        y += bh + gap;
        if button(Rect::new(m, y, bw, bh), "Check server", fs) && !busy {
            self.start(false);
        }
        y += bh + gap;
        if button(Rect::new(m, y, bw, bh), "Join server (offline-mode)", fs) && !busy {
            self.start(true);
        }
        y += bh + gap;
        let play = button(Rect::new(m, y, bw, bh), "Singleplayer world", fs);
        y += bh + gap;

        let lines = self.log.lock().unwrap().clone();
        let start = lines.len().saturating_sub(8);
        for (i, l) in lines[start..].iter().enumerate() {
            let s: String = l.chars().take(60).collect();
            draw_text(&s, m, y + fs * 0.9 * (i as f32 + 1.0), fs * 0.75, LIGHTGRAY);
        }
        play
    }
}

// ======================= МИР =======================

const W: usize = 48;
const H: usize = 24;
const D: usize = 48;
const CS: usize = 16;

struct World {
    blocks: Vec<u8>, // 0 = воздух, 1 = трава, 2 = земля, 3 = камень
}

impl World {
    fn new() -> Self {
        let mut blocks = vec![0u8; W * H * D];
        for z in 0..D {
            for x in 0..W {
                let h = (10.0 + (x as f32 * 0.25).sin() * 3.0 + (z as f32 * 0.18).cos() * 3.0)
                    as usize;
                for y in 0..=h.min(H - 1) {
                    blocks[(y * D + z) * W + x] = if y == h {
                        1
                    } else if y + 3 > h {
                        2
                    } else {
                        3
                    };
                }
            }
        }
        World { blocks }
    }

    fn inside(x: i32, y: i32, z: i32) -> bool {
        x >= 0 && y >= 0 && z >= 0 && (x as usize) < W && (y as usize) < H && (z as usize) < D
    }

    fn get(&self, x: i32, y: i32, z: i32) -> u8 {
        if Self::inside(x, y, z) {
            self.blocks[(y as usize * D + z as usize) * W + x as usize]
        } else {
            0
        }
    }

    fn set(&mut self, x: i32, y: i32, z: i32, b: u8) {
        if Self::inside(x, y, z) {
            self.blocks[(y as usize * D + z as usize) * W + x as usize] = b;
        }
    }
}

struct Face {
    n: (i32, i32, i32),
    c: [[f32; 3]; 4],
    shade: f32,
}

const FACES: [Face; 6] = [
    Face { n: (1, 0, 0), c: [[1., 0., 0.], [1., 1., 0.], [1., 1., 1.], [1., 0., 1.]], shade: 0.8 },
    Face { n: (-1, 0, 0), c: [[0., 0., 0.], [0., 0., 1.], [0., 1., 1.], [0., 1., 0.]], shade: 0.8 },
    Face { n: (0, 1, 0), c: [[0., 1., 0.], [0., 1., 1.], [1., 1., 1.], [1., 1., 0.]], shade: 1.0 },
    Face { n: (0, -1, 0), c: [[0., 0., 0.], [1., 0., 0.], [1., 0., 1.], [0., 0., 1.]], shade: 0.5 },
    Face { n: (0, 0, 1), c: [[0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.]], shade: 0.65 },
    Face { n: (0, 0, -1), c: [[0., 0., 0.], [0., 1., 0.], [1., 1., 0.], [1., 0., 0.]], shade: 0.65 },
];

fn block_color(b: u8) -> Color {
    match b {
        1 => Color::new(0.30, 0.65, 0.25, 1.0),
        2 => Color::new(0.50, 0.35, 0.20, 1.0),
        _ => Color::new(0.50, 0.50, 0.52, 1.0),
    }
}

fn build_chunk(w: &World, cx: usize, cz: usize) -> Mesh {
    let mut vertices: Vec<Vertex> = vec![];
    let mut indices: Vec<u16> = vec![];
    for y in 0..H {
        for z in cz * CS..(cz + 1) * CS {
            for x in cx * CS..(cx + 1) * CS {
                let b = w.get(x as i32, y as i32, z as i32);
                if b == 0 {
                    continue;
                }
                let base = block_color(b);
                for f in FACES.iter() {
                    let (nx, ny, nz) = f.n;
                    if w.get(x as i32 + nx, y as i32 + ny, z as i32 + nz) != 0 {
                        continue;
                    }
                    let s = f.shade;
                    let col = Color::new(base.r * s, base.g * s, base.b * s, 1.0);
                    let i0 = vertices.len() as u16;
                    for c in f.c.iter() {
                        vertices.push(Vertex::new(
                            x as f32 + c[0],
                            y as f32 + c[1],
                            z as f32 + c[2],
                            0.0,
                            0.0,
                            col,
                        ));
                    }
                    indices.extend_from_slice(&[i0, i0 + 1, i0 + 2, i0, i0 + 2, i0 + 3]);
                }
            }
        }
    }
    Mesh { vertices, indices, texture: None }
}

fn build_world(w: &World) -> Vec<Mesh> {
    let mut meshes = vec![];
    for cz in 0..D / CS {
        for cx in 0..W / CS {
            meshes.push(build_chunk(w, cx, cz));
        }
    }
    meshes
}

fn raycast(w: &World, origin: Vec3, dir: Vec3) -> Option<(IVec3, IVec3)> {
    let mut prev = origin.floor().as_ivec3();
    let mut t = 0.0;
    while t < 8.0 {
        let c = (origin + dir * t).floor().as_ivec3();
        if w.get(c.x, c.y, c.z) != 0 {
            return Some((c, prev));
        }
        prev = c;
        t += 0.05;
    }
    None
}

struct Ptr {
    start: Vec2,
    last: Vec2,
    t0: f64,
    moved: f32,
    left: bool,
}

struct Game {
    world: World,
    meshes: Vec<Mesh>,
    pos: Vec3,
    yaw: f32,
    pitch: f32,
    place_mode: bool,
    ptrs: HashMap<u64, Ptr>,
}

impl Game {
    fn new() -> Self {
        let world = World::new();
        let meshes = build_world(&world);
        Game {
            world,
            meshes,
            pos: vec3(24.0, 18.0, 24.0),
            yaw: 0.0,
            pitch: -0.4,
            place_mode: false,
            ptrs: HashMap::new(),
        }
    }

    /// Возвращает true, если нужно вернуться в меню.
    fn frame(&mut self) -> bool {
        let dt = get_frame_time();
        let (sw, sh) = (screen_width(), screen_height());
        let ui = (sw.min(sh) / 720.0).max(1.0);
        let place_btn = Rect::new(sw - 140.0 * ui, 10.0 * ui, 130.0 * ui, 55.0 * ui);
        let menu_btn = Rect::new(10.0 * ui, 10.0 * ui, 130.0 * ui, 55.0 * ui);
        if tapped(menu_btn) {
            return true;
        }

        let mut events: Vec<(u64, TouchPhase, Vec2)> =
            touches().iter().map(|t| (t.id, t.phase, t.position)).collect();
        if events.is_empty() {
            let m = Vec2::from(mouse_position());
            if is_mouse_button_pressed(MouseButton::Left) {
                events.push((100, TouchPhase::Started, m));
            } else if is_mouse_button_down(MouseButton::Left) {
                events.push((100, TouchPhase::Moved, m));
            } else if is_mouse_button_released(MouseButton::Left) {
                events.push((100, TouchPhase::Ended, m));
            }
        }

        let front = vec3(
            self.yaw.cos() * self.pitch.cos(),
            self.pitch.sin(),
            self.yaw.sin() * self.pitch.cos(),
        );

        for (id, phase, p) in events {
            match phase {
                TouchPhase::Started => {
                    self.ptrs.insert(
                        id,
                        Ptr { start: p, last: p, t0: get_time(), moved: 0.0, left: p.x < sw * 0.5 },
                    );
                }
                TouchPhase::Ended | TouchPhase::Cancelled => {
                    if let Some(t) = self.ptrs.remove(&id) {
                        let is_tap = !t.left && t.moved < 15.0 * ui && get_time() - t.t0 < 0.3;
                        if is_tap {
                            if place_btn.contains(p) {
                                self.place_mode = !self.place_mode;
                            } else if let Some((hit, prev)) = raycast(&self.world, self.pos, front) {
                                if self.place_mode {
                                    if prev != self.pos.floor().as_ivec3() {
                                        self.world.set(prev.x, prev.y, prev.z, 3);
                                    }
                                } else {
                                    self.world.set(hit.x, hit.y, hit.z, 0);
                                }
                                self.meshes = build_world(&self.world);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(t) = self.ptrs.get_mut(&id) {
                        let d = p - t.last;
                        t.moved += d.length();
                        t.last = p;
                        if !t.left {
                            self.yaw += d.x * 0.005;
                            self.pitch = (self.pitch - d.y * 0.005).clamp(-1.5, 1.5);
                        }
                    }
                }
            }
        }

        let mut mv = Vec2::ZERO;
        for t in self.ptrs.values().filter(|t| t.left) {
            mv += ((t.last - t.start) / (80.0 * ui)).clamp_length_max(1.0) * vec2(1.0, -1.0);
        }
        if is_key_down(KeyCode::W) { mv.y += 1.0; }
        if is_key_down(KeyCode::S) { mv.y -= 1.0; }
        if is_key_down(KeyCode::D) { mv.x += 1.0; }
        if is_key_down(KeyCode::A) { mv.x -= 1.0; }
        if is_key_pressed(KeyCode::B) { self.place_mode = !self.place_mode; }
        let mv = mv.clamp_length_max(1.0);
        let righ
