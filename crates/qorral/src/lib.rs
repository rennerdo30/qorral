//! Qorral's app: the GUI (`qorral-gui`) and the CLI (`qorral`) share this
//! crate, and both drive VMs only through `qorral-core`.

pub mod cli;
#[cfg(windows)]
pub mod frame;
pub mod model;
pub mod platform;
pub mod snapshot;
pub mod theme;
pub mod ui;
pub mod window;

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TRACE_ON: OnceLock<bool> = OnceLock::new();

/// Records a startup or frame mark (`QORRAL_TRACE=1`). Marks are buffered and
/// printed at exit: writing each one to a redirected stderr is slow on
/// Windows and would distort the very timings it reports.
pub fn trace(t0: Instant, what: &str) {
    if *TRACE_ON.get_or_init(|| std::env::var_os("QORRAL_TRACE").is_some()) {
        let line = format!("{:>8.2} ms  {what}", t0.elapsed().as_secs_f64() * 1000.0);
        if let Ok(mut v) = TRACE.lock() {
            v.push(line);
        }
    }
}

pub fn flush_trace() {
    if let Ok(v) = TRACE.lock() {
        for l in v.iter() {
            eprintln!("{l}");
        }
    }
}

/// GUI options (`qorral-gui --help`).
#[derive(Default)]
pub struct GuiArgs {
    pub snapshot: Option<std::path::PathBuf>,
    pub smoke: bool,
    pub size: (usize, usize),
    pub scale: Option<f32>,
    /// Snapshot with example machines instead of the user's library.
    pub demo: bool,
    pub empty: bool,
    pub remote: bool,
    pub hover: Option<(i32, i32)>,
    pub query: Option<String>,
    pub select: Option<String>,
}

pub fn parse_gui_args(args: impl Iterator<Item = String>) -> Result<GuiArgs, String> {
    let mut a = GuiArgs { size: (1600, 1000), ..GuiArgs::default() };
    let mut it = args;
    while let Some(arg) = it.next() {
        let mut val = |n: &str| it.next().ok_or_else(|| format!("{n} needs a value"));
        match arg.as_str() {
            "--snapshot" => a.snapshot = Some(val("--snapshot")?.into()),
            "--smoke" => a.smoke = true,
            "--size" => {
                let v = val("--size")?;
                let (w, h) = v.split_once('x').ok_or("--size is WxH")?;
                a.size = (w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?);
            }
            "--scale" => a.scale = Some(val("--scale")?.parse().map_err(|_| "bad scale")?),
            "--demo" => a.demo = true,
            "--empty" => a.empty = true,
            "--remote" => a.remote = true,
            "--query" => a.query = Some(val("--query")?),
            "--select" => a.select = Some(val("--select")?),
            "--hover" => {
                let v = val("--hover")?;
                let (x, y) = v.split_once(',').ok_or("--hover is X,Y")?;
                a.hover = Some((x.parse().map_err(|_| "bad x")?, y.parse().map_err(|_| "bad y")?));
            }
            "--version" => {
                println!("qorral {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!(
                    "usage: qorral-gui [--snapshot out.png [--size WxH] [--scale S] [--demo|--empty] [--remote] [--hover X,Y] [--query Q] [--select ID]] [--smoke]"
                );
                std::process::exit(0);
            }
            s => return Err(format!("unknown option {s}")),
        }
    }
    Ok(a)
}

/// Runs the GUI: offscreen snapshot or window.
pub fn run_gui(args: GuiArgs) -> i32 {
    let t0 = Instant::now();
    let code = if args.snapshot.is_some() { snapshot::run(&args, t0) } else { window::run(&args, t0) };
    flush_trace();
    code
}
