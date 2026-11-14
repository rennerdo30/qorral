//! The intent bar's parser: plain words to a machine draft or an action.
//!
//! "fedora 8gb vulkan" → create Fedora with 8 GB and Vulkan graphics.
//! "start arch" → start the machine called Arch. Pure and unit-tested; the UI
//! only renders what this returns.

use crate::model;
use qorral_core::GpuMode;

/// A machine to create, as far as the words said.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub preset: Option<&'static str>,
    pub name: Option<String>,
    pub cpus: Option<u32>,
    pub memory_gib: Option<u32>,
    pub disk_gib: Option<u32>,
    pub gpu: Option<GpuMode>,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        *self == Draft::default()
    }

    /// The values the machine will get: the words, else the preset, else defaults.
    pub fn resolved(&self) -> (String, u32, u32, u32, GpuMode) {
        let p = self.preset.and_then(model::preset);
        let label = p.map_or("New machine", |p| p.label).to_string();
        (
            self.name.clone().unwrap_or(label),
            self.cpus.or(p.map(|p| p.cpus)).unwrap_or(4),
            self.memory_gib.or(p.map(|p| p.memory_mib / 1024)).unwrap_or(8),
            self.disk_gib.or(p.map(|p| p.disk_gib)).unwrap_or(64),
            self.gpu.or(p.map(|p| p.gpu)).unwrap_or(GpuMode::Basic),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Open,
    Start,
    Stop,
    ForceOff,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    /// Create a machine (Enter asks for its install image).
    Create(Draft),
    /// Act on a machine, by index into the caller's list.
    Machine(Verb, usize),
}

/// Words that name a system preset.
fn preset_word(w: &str) -> Option<&'static str> {
    match w {
        "ubuntu" => Some("ubuntu"),
        "fedora" => Some("fedora"),
        "debian" => Some("debian"),
        "arch" | "archlinux" => Some("arch"),
        "windows" | "win" | "win11" | "windows11" => Some("windows-11"),
        _ => None,
    }
}

/// `8gb`, `8 gb`, `8g` → 8 (the unit may be the next word).
fn size(words: &[String], i: usize) -> Option<(u32, usize)> {
    let w = &words[i];
    let digits: String = w.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    let unit = &w[digits.len()..];
    if matches!(unit, "gb" | "g" | "gib") {
        return Some((n, 1));
    }
    if unit.is_empty() && words.get(i + 1).is_some_and(|u| matches!(u.as_str(), "gb" | "g" | "gib")) {
        return Some((n, 2));
    }
    None
}

/// Parses the bar's text against the machine names. Returns every result
/// worth showing, best first.
pub fn parse(text: &str, machines: &[(String, bool)]) -> Vec<Intent> {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return Vec::new();
    }
    let words: Vec<String> = lower.split_whitespace().map(str::to_string).collect();
    let verb = match words[0].as_str() {
        "start" | "run" | "boot" => Some(Verb::Start),
        "stop" | "shutdown" | "shut" => Some(Verb::Stop),
        "kill" | "poweroff" | "force" => Some(Verb::ForceOff),
        "open" | "show" | "go" => Some(Verb::Open),
        _ => None,
    };
    let rest: Vec<&String> =
        words.iter().skip(if verb.is_some() { 1 } else { 0 }).filter(|w| *w != "down" && *w != "off").collect();
    let query = rest.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");

    let mut out = Vec::new();
    // Machines whose name contains the words.
    if !query.is_empty() {
        for (i, (name, running)) in machines.iter().enumerate() {
            if name.to_lowercase().contains(&query) {
                let v = verb.unwrap_or(if *running { Verb::Open } else { Verb::Start });
                out.push(Intent::Machine(v, i));
            }
        }
    }
    // A creation draft, unless the words were clearly a verb on a machine.
    if verb.is_none() || out.is_empty() {
        let mut d = Draft::default();
        let mut i = 0;
        let mut disk_next = false;
        while i < words.len() {
            let w = words[i].as_str();
            if let Some(p) = preset_word(w) {
                d.preset = Some(p);
            } else if matches!(w, "vulkan" | "venus") {
                d.gpu = Some(GpuMode::Vulkan);
            } else if matches!(w, "opengl" | "gl" | "3d" | "virgl") {
                d.gpu = Some(GpuMode::Opengl);
            } else if w == "disk" {
                disk_next = true;
            } else if let Some((n, used)) = size(&words, i) {
                let disk =
                    disk_next || words.get(i + used).is_some_and(|x| x == "disk") || n >= 32 && d.memory_gib.is_some();
                if disk {
                    d.disk_gib = Some(n.clamp(1, 65536));
                    disk_next = false;
                } else {
                    d.memory_gib = Some(n.clamp(1, 1024));
                }
                i += used;
                continue;
            } else if let Some(n) = w
                .strip_suffix("cpu")
                .or(w.strip_suffix("cores"))
                .or(w.strip_suffix("vcpu"))
                .and_then(|n| n.parse().ok())
            {
                d.cpus = Some(u32::clamp(n, 1, 256));
            } else if let (Ok(n), Some(next)) = (w.parse::<u32>(), words.get(i + 1))
                && matches!(next.as_str(), "cpu" | "cpus" | "cores" | "vcpu" | "vcpus")
            {
                d.cpus = Some(n.clamp(1, 256));
                i += 2;
                continue;
            } else if matches!(w, "named" | "called" | "as") {
                let name: Vec<&str> = text.split_whitespace().skip(i + 1).collect();
                if !name.is_empty() {
                    d.name = Some(name.join(" "));
                }
                break;
            }
            i += 1;
        }
        if !d.is_empty() || out.is_empty() {
            let create = Intent::Create(d);
            // A system name or sizes put creation first; plain names favour machines.
            if words.iter().any(|w| preset_word(w).is_some() || size(&words, 0).is_some()) && verb.is_none() {
                out.insert(0, create);
            } else {
                out.push(create);
            }
        }
    }
    out.truncate(6);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(names: &[(&str, bool)]) -> Vec<(String, bool)> {
        names.iter().map(|(n, r)| (n.to_string(), *r)).collect()
    }

    #[test]
    fn plain_words_become_a_draft() {
        let r = parse("fedora 16gb vulkan 4 cpus", &[]);
        let Intent::Create(d) = &r[0] else { panic!("{r:?}") };
        assert_eq!(d.preset, Some("fedora"));
        assert_eq!(d.memory_gib, Some(16));
        assert_eq!(d.gpu, Some(GpuMode::Vulkan));
        assert_eq!(d.cpus, Some(4));
        let r = parse("win11 8 gb 128gb disk named Build box", &[]);
        let Intent::Create(d) = &r[0] else { panic!("{r:?}") };
        assert_eq!((d.preset, d.memory_gib, d.disk_gib), (Some("windows-11"), Some(8), Some(128)));
        assert_eq!(d.name.as_deref(), Some("Build box"));
        assert_eq!(d.resolved().4, GpuMode::Basic);
    }

    #[test]
    fn machine_names_and_verbs_win_over_drafts() {
        let ms = m(&[("Fedora Workstation", true), ("Arch Linux", false)]);
        assert_eq!(parse("start arch", &ms)[0], Intent::Machine(Verb::Start, 1));
        assert_eq!(parse("shut down fedora", &ms)[0], Intent::Machine(Verb::Stop, 0));
        // A running machine opens; the plain system name still offers a new one too.
        let r = parse("fedora", &ms);
        assert!(r.contains(&Intent::Machine(Verb::Open, 0)));
        assert!(r.iter().any(|i| matches!(i, Intent::Create(d) if d.preset == Some("fedora"))));
        // Unknown words still offer "create from an image".
        assert!(matches!(parse("haiku", &ms)[0], Intent::Create(_)));
        assert!(parse("   ", &ms).is_empty());
    }
}
