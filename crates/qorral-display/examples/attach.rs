//! Live check: attach to a running VM's display and report what arrives.
//!
//! `cargo run -p qorral-display --example attach -- QMP_PORT QEMU_PID [SECONDS]`

#[cfg(windows)]
fn main() {
    use qorral_display::{Caps, Display, Event, SharedMap};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(port), Some(pid)) =
        (args.first().and_then(|a| a.parse::<u16>().ok()), args.get(1).and_then(|a| a.parse::<u32>().ok()))
    else {
        eprintln!("usage: attach QMP_PORT QEMU_PID [SECONDS]");
        std::process::exit(2);
    };
    let secs: u64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(5);

    // A minimal QMP client: greeting, capabilities, then one command at a time.
    let s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("QMP connect");
    let mut w = s.try_clone().unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    let mut qmp = |cmd: &str, args: &[(&str, &str)]| -> Result<(), String> {
        let a: Vec<String> = args.iter().map(|(k, v)| format!("\"{k}\":\"{v}\"")).collect();
        writeln!(w, "{{\"execute\":\"{cmd}\",\"arguments\":{{{}}}}}", a.join(",")).map_err(|e| e.to_string())?;
        loop {
            line.clear();
            r.read_line(&mut line).map_err(|e| e.to_string())?;
            if line.contains("\"return\"") {
                return Ok(());
            }
            if line.contains("\"error\"") {
                return Err(line.trim().to_string());
            }
        }
    };
    qmp("qmp_capabilities", &[]).expect("qmp_capabilities");

    #[derive(Default)]
    struct Stats {
        counts: std::collections::BTreeMap<&'static str, u64>,
        map: Option<(SharedMap, u32, u32, u32)>,
        update_px: u64,
        first: Option<Instant>,
        last: Option<Instant>,
        centre: Vec<u32>,
    }
    let stats = Arc::new(Mutex::new(Stats::default()));
    let st = stats.clone();
    let t0 = Instant::now();
    let display = Display::attach(pid, 0, Caps { map: true, d3d11: false }, &mut qmp, move |e| {
        let mut s = st.lock().unwrap();
        let now = Instant::now();
        let name = match &e {
            None => "closed",
            Some(Event::Scanout { .. }) => "Scanout",
            Some(Event::Update { .. }) => "Update",
            Some(Event::ScanoutMap { .. }) => "ScanoutMap",
            Some(Event::UpdateMap { .. }) => "UpdateMap",
            Some(Event::ScanoutTexture { .. }) => "ScanoutTexture",
            Some(Event::UpdateTexture { .. }) => "UpdateTexture",
            Some(Event::Disable) => "Disable",
            Some(Event::MouseSet { .. }) => "MouseSet",
            Some(Event::CursorDefine { .. }) => "CursorDefine",
        };
        *s.counts.entry(name).or_default() += 1;
        match e {
            Some(Event::ScanoutMap { handle, offset, width, height, stride, format }) => {
                println!(
                    "{:>8.1} ms ScanoutMap {width}x{height} stride {stride} format {format:#x}",
                    t0.elapsed().as_secs_f64() * 1e3
                );
                s.map = SharedMap::open(handle, offset, (stride * height) as usize)
                    .ok()
                    .map(|m| (m, width, height, stride));
            }
            Some(Event::UpdateMap { width, height, .. }) | Some(Event::Update { width, height, .. }) => {
                s.update_px += (width.max(0) as u64) * (height.max(0) as u64);
                s.first.get_or_insert(now);
                s.last = Some(now);
                if let Some((m, w, h, stride)) = &s.map {
                    let off = (*h / 2 * *stride + *w / 2 * 4) as usize;
                    let b = m.bytes();
                    let px = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]);
                    s.centre.push(px);
                }
            }
            Some(Event::Scanout { width, height, .. }) => println!("Scanout {width}x{height} (in message)"),
            _ => {}
        }
    })
    .expect("attach");
    println!("attached in {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    std::thread::sleep(Duration::from_secs(secs));
    let _ = display.mouse_to(10, 10);
    drop(display);
    let s = stats.lock().unwrap();
    println!("events: {:?}", s.counts);
    let updates = s.counts.get("UpdateMap").copied().unwrap_or(0) + s.counts.get("Update").copied().unwrap_or(0);
    if let (Some(a), Some(b)) = (s.first, s.last) {
        let span = (b - a).as_secs_f64();
        println!(
            "updates: {updates} in {span:.2} s ({:.1}/s), {:.1} Mpx changed, distinct centre pixels: {}",
            updates as f64 / span.max(1e-9),
            s.update_px as f64 / 1e6,
            s.centre.iter().collect::<std::collections::BTreeSet<_>>().len()
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("the attach example is Windows-only for now");
}
