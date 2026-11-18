//! Live zero-copy check: attach to a VM, import the guest's scanout buffers into the compositor
//! and draw every flipped frame from them on arrival; report the time from flip received to GPU
//! done, and write the last frame as a PNG.
//!
//! `cargo run -p qorral-gpu --example zerocopy -- QMP_PORT QEMU_PID SECONDS OUT.png`

#[cfg(windows)]
fn main() {
    use qorral_display::{Caps, Display, Event};
    use qorral_gpu::{Compositor, GuestMemory, ImageParams, Layout};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: zerocopy QMP_PORT QEMU_PID SECONDS OUT.png");
        std::process::exit(2);
    }
    let port: u16 = args[0].parse().expect("port");
    let pid: u32 = args[1].parse().expect("pid");
    let secs: u64 = args[2].parse().expect("seconds");
    let out = args[3].clone();

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
    struct Report {
        total_ms: Vec<f64>,
        gpu_ms: Vec<f64>,
        imports: u32,
        zero_copy: u32,
        errors: Vec<String>,
        frame: Option<(u32, u32, Vec<u8>)>,
        device: String,
    }
    let report = Arc::new(Mutex::new(Report::default()));
    let snapshot = Arc::new(AtomicBool::new(false));
    let (rep, snap) = (report.clone(), snapshot.clone());
    // Shared so the main thread can redraw the last frame at the end (a static desktop does not
    // flip).
    /// The compositor, the target size and the last flipped buffer.
    type Shared = Arc<Mutex<(Option<Compositor>, (u32, u32), u32)>>;
    let shared: Shared = Arc::new(Mutex::new((None, (0, 0), 0)));
    let sh = shared.clone();

    let display = Display::attach(pid, 0, Caps { vulkan: true, ..Caps::default() }, &mut qmp, move |e| {
        let t_recv = Instant::now();
        let mut rep = rep.lock().unwrap();
        let mut guard = sh.lock().unwrap();
        let (comp, size, last) = &mut *guard;
        match e {
            Some(Event::ScanoutBlob {
                resource_id,
                handle,
                allocation_size,
                memory_type_index,
                device_uuid,
                offset,
                width,
                height,
                stride,
                image,
                ..
            }) => {
                if comp.is_none() {
                    match Compositor::new(&device_uuid, width, height) {
                        Ok(c) => {
                            rep.device = c.device_name.clone();
                            *size = (width, height);
                            *comp = Some(c);
                        }
                        Err(err) => rep.errors.push(err.to_string()),
                    }
                }
                if let Some(c) = comp.as_mut() {
                    let image = image.map(|i| ImageParams {
                        flags: i.flags,
                        image_type: i.image_type,
                        format: i.format,
                        width: i.width,
                        height: i.height,
                        depth: i.depth,
                        mip_levels: i.mip_levels,
                        array_layers: i.array_layers,
                        samples: i.samples,
                        tiling: i.tiling,
                        usage: i.usage,
                        sharing_mode: i.sharing_mode,
                    });
                    let mem = GuestMemory {
                        handle: handle as isize,
                        allocation_size,
                        memory_type_index,
                        layout: Layout { offset, width, height, stride },
                        image,
                    };
                    match c.import(resource_id, mem) {
                        Ok(zero_copy) => {
                            rep.imports += 1;
                            if zero_copy {
                                rep.zero_copy += 1;
                            }
                        }
                        Err(err) => rep.errors.push(err.to_string()),
                    }
                }
            }
            Some(Event::FlipBlob { resource_id, .. }) if resource_id != 0 => {
                let Some(c) = comp.as_mut() else { return };
                *last = resource_id;
                let readback = snap.swap(false, Ordering::Relaxed);
                match c.draw(resource_id, [0.0, 0.0, size.0 as f32, size.1 as f32], readback) {
                    Ok(gpu) => {
                        rep.total_ms.push(t_recv.elapsed().as_secs_f64() * 1e3);
                        rep.gpu_ms.push(gpu.as_secs_f64() * 1e3);
                        if readback {
                            match c.pixels() {
                                Ok(px) => rep.frame = Some((size.0, size.1, px)),
                                Err(err) => rep.errors.push(err.to_string()),
                            }
                        }
                    }
                    Err(err) => rep.errors.push(err.to_string()),
                }
            }
            Some(Event::ForgetBlob { resource_id }) => {
                if let Some(c) = comp.as_mut() {
                    c.forget(resource_id);
                }
            }
            _ => {}
        }
    })
    .expect("attach");
    std::thread::sleep(Duration::from_secs(secs));
    snapshot.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(500));
    {
        // No flip took the snapshot: draw the last shown buffer now.
        let mut guard = shared.lock().unwrap();
        let (comp, size, last) = &mut *guard;
        let mut rep = report.lock().unwrap();
        if rep.frame.is_none()
            && let Some(c) = comp.as_mut()
            && c.has(*last)
            && c.draw(*last, [0.0, 0.0, size.0 as f32, size.1 as f32], true).is_ok()
            && let Ok(px) = c.pixels()
        {
            rep.frame = Some((size.0, size.1, px));
        }
    }
    drop(display);

    let rep = report.lock().unwrap();
    println!("device: {}; guest buffers imported: {} ({} zero copy)", rep.device, rep.imports, rep.zero_copy);
    for e in rep.errors.iter().take(5) {
        println!("error: {e}");
    }
    let stats = |name: &str, v: &[f64]| {
        if v.is_empty() {
            return;
        }
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        println!(
            "{name}: {} frames, median {:.3} ms, p99 {:.3} ms, max {:.3} ms",
            s.len(),
            s[s.len() / 2],
            s[(s.len() * 99 / 100).min(s.len() - 1)],
            s[s.len() - 1]
        );
    };
    stats("flip received -> drawn (GPU done)", &rep.total_ms);
    stats("GPU submit -> done", &rep.gpu_ms);
    if let Some((w, h, px)) = &rep.frame {
        let rgba: Vec<u8> = px.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], 255]).collect();
        let f = std::fs::File::create(&out).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), *w, *h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().and_then(|mut wr| wr.write_image_data(&rgba)).expect("write png");
        println!("frame written to {out}");
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("the zerocopy example is Windows-only for now");
}
