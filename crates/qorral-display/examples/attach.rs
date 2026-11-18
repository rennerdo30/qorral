//! Live check: attach to a running VM's display and report what arrives.
//!
//! `cargo run -p qorral-display --example attach -- QMP_PORT QEMU_PID [SECONDS] [CAPS]`
//! CAPS: `vulkan` (zero copy, default), `map`, `d3d11`, or a comma list.

#[cfg(windows)]
fn main() {
    use qorral_display::{Caps, Display, Event, SharedMap};
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(port), Some(pid)) =
        (args.first().and_then(|a| a.parse::<u16>().ok()), args.get(1).and_then(|a| a.parse::<u32>().ok()))
    else {
        eprintln!("usage: attach QMP_PORT QEMU_PID [SECONDS] [CAPS]");
        std::process::exit(2);
    };
    let secs: u64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(5);
    let want = args.get(3).map(String::as_str).unwrap_or("vulkan");
    let caps = Caps { map: want.contains("map"), d3d11: want.contains("d3d11"), vulkan: want.contains("vulkan") };

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
        counts: BTreeMap<&'static str, u64>,
        map: Option<SharedMap>,
        frames: Vec<Instant>,
        buffers: BTreeMap<u32, u64>,
    }
    let close = |h: u64| {
        // SAFETY: the handle was duplicated into this process for us to own.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h as usize as _) };
    };
    let stats = Arc::new(Mutex::new(Stats::default()));
    let st = stats.clone();
    let t0 = Instant::now();
    let display = Display::attach(pid, 0, caps, &mut qmp, move |e| {
        let mut s = st.lock().unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1e3;
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
            Some(Event::ScanoutBlob { .. }) => "ScanoutBlob",
            Some(Event::FlipBlob { .. }) => "FlipBlob",
            Some(Event::ForgetBlob { .. }) => "ForgetBlob",
        };
        *s.counts.entry(name).or_default() += 1;
        match e {
            Some(Event::ScanoutBlob {
                resource_id,
                handle,
                allocation_size,
                memory_type_index,
                dedicated,
                device_uuid,
                offset,
                width,
                height,
                stride,
                format,
                ..
            }) => {
                println!(
                    "{ms:>8.1} ms ScanoutBlob res {resource_id}: {width}x{height} stride {stride} format {format:#x} \
                     offset {offset}, memory {allocation_size} bytes type {memory_type_index}{} on device {}",
                    if dedicated { " dedicated" } else { "" },
                    device_uuid.iter().map(|b| format!("{b:02x}")).collect::<String>()
                );
                close(handle);
            }
            Some(Event::FlipBlob { resource_id, .. }) => {
                s.frames.push(Instant::now());
                *s.buffers.entry(resource_id).or_default() += 1;
            }
            Some(Event::ScanoutMap { handle, offset, width, height, stride, .. }) => {
                println!("{ms:>8.1} ms ScanoutMap {width}x{height}");
                s.map = SharedMap::open(handle, offset, (stride * height) as usize).ok();
            }
            Some(Event::UpdateMap { .. }) | Some(Event::Update { .. }) => s.frames.push(Instant::now()),
            Some(Event::ScanoutTexture { handle, width, height, .. }) => {
                println!("{ms:>8.1} ms ScanoutTexture {width}x{height}");
                close(handle);
            }
            Some(Event::Scanout { width, height, .. }) => {
                println!("{ms:>8.1} ms Scanout {width}x{height} (in message)")
            }
            _ => {}
        }
    })
    .expect("attach");
    println!("attached in {:.1} ms with {caps:?}", t0.elapsed().as_secs_f64() * 1e3);
    std::thread::sleep(Duration::from_secs(secs));
    drop(display);
    let s = stats.lock().unwrap();
    println!("events: {:?}", s.counts);
    println!("frames per buffer: {:?}", s.buffers);
    if s.frames.len() > 2 {
        let mut gaps: Vec<f64> = s.frames.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1e3).collect();
        gaps.sort_by(f64::total_cmp);
        let span = (*s.frames.last().unwrap() - s.frames[0]).as_secs_f64();
        println!(
            "{} frames in {span:.2} s ({:.1}/s); gap median {:.2} ms, p99 {:.2} ms",
            s.frames.len(),
            (s.frames.len() - 1) as f64 / span,
            gaps[gaps.len() / 2],
            gaps[(gaps.len() * 99 / 100).min(gaps.len() - 1)]
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("the attach example is Windows-only for now");
}
