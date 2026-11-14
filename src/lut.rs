//! 3D LUTs (`.cube`), as DJI publishes D-Log M to Rec.709 and as grading
//! looks come. A LUT maps the stitched picture's display values before the
//! effects (`Grade`), so exposure and the rest act on Rec.709 as before.
//! Looked up trilinearly, as the GPU's 3D texture filters, so the player,
//! GPU export and CPU export agree.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// LUTs loaded this run, each file once.
static LOADED: Mutex<Vec<(PathBuf, Arc<Lut>)>> = Mutex::new(Vec::new());

/// `path`'s LUT, loaded once a run; its id for `get`.
pub fn shared(path: &Path) -> Result<usize> {
    let mut loaded = LOADED.lock().unwrap();
    if let Some(id) = loaded.iter().position(|(p, _)| p == path) {
        return Ok(id);
    }
    let lut = Lut::load(path)?;
    loaded.push((path.to_owned(), Arc::new(lut)));
    Ok(loaded.len() - 1)
}

/// A LUT `shared` loaded.
pub fn get(id: usize) -> Arc<Lut> {
    LOADED.lock().unwrap()[id].1.clone()
}

/// Where the user's LUTs are kept, copied in so projects survive a moved
/// download: `<config>/luts/`, each named by its file (`<name>.cube`).
pub fn library() -> Option<PathBuf> {
    Some(crate::player::config_dir()?.join("luts"))
}

/// The library's LUTs by name, sorted.
pub fn library_names() -> Vec<String> {
    let Some(dir) = library() else { return Vec::new() };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let cube = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("cube"));
            cube.then(|| Some(path.file_stem()?.to_string_lossy().into_owned())).flatten()
        })
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Copies the `.cube` at `path` into the library, after checking it reads,
/// named by its file (with " 2" and on if another of that name differs).
/// Gives its name.
pub fn add_to_library(path: &Path) -> Result<String> {
    Lut::load(path)?;
    let dir = library().context("no configuration folder (HOME is not set)")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let stem = path.file_stem().context("no file name")?.to_string_lossy().into_owned();
    let bytes = std::fs::read(path)?;
    for k in 1.. {
        let name = if k == 1 { stem.clone() } else { format!("{stem} {k}") };
        let to = dir.join(format!("{name}.cube"));
        match std::fs::read(&to) {
            Ok(there) if there == bytes => return Ok(name),
            Ok(_) => continue,
            Err(_) => {
                crate::replace_file(&to, &bytes)?;
                return Ok(name);
            }
        }
    }
    unreachable!()
}

/// Takes a LUT out of the library. Cuts naming it then show as filmed.
pub fn remove_from_library(name: &str) -> Result<()> {
    let path = library().context("no configuration folder")?.join(format!("{name}.cube"));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// Whether a cut's `lut` is a file (`….cube`) rather than a library name.
pub fn is_file(name: &str) -> bool {
    name.to_lowercase().ends_with(".cube")
}

/// The file a cut's `lut` names: the file itself, or the library's LUT of
/// that name.
pub fn find(name: &str) -> Option<PathBuf> {
    let path = if is_file(name) { PathBuf::from(name) } else { library()?.join(format!("{name}.cube")) };
    path.is_file().then_some(path)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lut {
    /// Points along each side.
    pub size: usize,
    /// Output colours, red fastest, then green, then blue (the .cube order).
    pub table: Vec<[f32; 3]>,
    /// The input range the table spans (`DOMAIN_MIN`/`DOMAIN_MAX`, else 0..1).
    pub domain: ([f32; 3], [f32; 3]),
}

impl Lut {
    pub fn load(path: &Path) -> Result<Lut> {
        // Not as UTF-8 only: a title or comment in Latin-1 (©, é) is no reason
    // to refuse numbers that are plain ASCII. Nor is a byte-order mark.
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
        Lut::parse(text).with_context(|| format!("reading the LUT {}", path.display()))
    }

    /// A `.cube` file (Adobe/Resolve): keywords, then `size³` lines of three
    /// numbers. 1D LUTs are refused: they're a different thing.
    pub fn parse(text: &str) -> Result<Lut> {
        let (mut size, mut table) = (None, Vec::new());
        let mut domain = ([0.0; 3], [1.0; 3]);
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut words = line.split_whitespace();
            let first = words.next().unwrap_or("");
            let three = |words: std::str::SplitWhitespace| -> Result<[f32; 3]> {
                let v: Vec<f32> = words.map(|w| w.parse::<f32>()).collect::<Result<_, _>>().with_context(|| format!("bad numbers in \"{line}\""))?;
                v.try_into().map_err(|_| anyhow::anyhow!("expected three numbers in \"{line}\""))
            };
            match first {
                "LUT_3D_SIZE" => size = Some(words.next().context("LUT_3D_SIZE without a size")?.parse::<usize>()?),
                "LUT_1D_SIZE" => bail!("a 1D LUT; only 3D LUTs are supported"),
                "DOMAIN_MIN" => domain.0 = three(words)?,
                "DOMAIN_MAX" => domain.1 = three(words)?,
                "TITLE" | "LUT_3D_INPUT_RANGE" | "LUT_1D_INPUT_RANGE" => {}
                _ if first.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.') => table.push(three(line.split_whitespace())?),
                // Keywords from other tools: skipped, as Resolve does.
                _ => {}
            }
        }
        let size = size.context("no LUT_3D_SIZE")?;
        if !(2..=256).contains(&size) {
            bail!("LUT_3D_SIZE {size} is out of range");
        }
        if table.len() != size.pow(3) {
            bail!("{} colours where a {size}³ LUT has {}", table.len(), size.pow(3));
        }
        if (0..3).any(|c| domain.1[c] <= domain.0[c]) {
            bail!("DOMAIN_MAX must be above DOMAIN_MIN");
        }
        Ok(Lut { size, table, domain })
    }

    /// `rgb` (0..1) through the table, trilinearly; outside the domain it
    /// holds the edge.
    pub fn apply(&self, rgb: [f64; 3]) -> [f64; 3] {
        let n = self.size;
        let mut at = [0usize; 3];
        let mut f = [0f64; 3];
        for c in 0..3 {
            let (lo, hi) = (self.domain.0[c] as f64, self.domain.1[c] as f64);
            let x = ((rgb[c] - lo) / (hi - lo)).clamp(0.0, 1.0) * (n - 1) as f64;
            at[c] = (x.floor() as usize).min(n - 2);
            f[c] = x - at[c] as f64;
        }
        let mut out = [0f64; 3];
        for corner in 0..8 {
            let d = [corner & 1, (corner >> 1) & 1, (corner >> 2) & 1];
            let w: f64 = (0..3).map(|c| if d[c] == 1 { f[c] } else { 1.0 - f[c] }).product();
            if w == 0.0 {
                continue;
            }
            let v = self.table[(at[0] + d[0]) + (at[1] + d[1]) * n + (at[2] + d[2]) * n * n];
            for c in 0..3 {
                out[c] += w * v[c] as f64;
            }
        }
        out
    }

    /// `apply` mixed with the colour as it was: 1 is the LUT, 0 untouched.
    pub fn blend(&self, rgb: [f64; 3], strength: f64) -> [f64; 3] {
        if strength <= 0.0 {
            return rgb;
        }
        let to = self.apply(rgb);
        std::array::from_fn(|c| rgb[c] + (to[c] - rgb[c]) * strength.min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(size: usize, f: impl Fn([f64; 3]) -> [f64; 3]) -> String {
        let mut s = format!("TITLE \"test\"\n# a comment\nLUT_3D_SIZE {size}\n");
        let n = (size - 1) as f64;
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    let v = f([r as f64 / n, g as f64 / n, b as f64 / n]);
                    s += &format!("{:.6}\t{:.6}\t{:.6}\t\r\n", v[0], v[1], v[2]);
                }
            }
        }
        s
    }

    #[test]
    fn identity_and_linear_tables_come_out_exact() {
        let id = Lut::parse(&cube(17, |c| c)).unwrap();
        // Any map linear in each channel is what trilinear reproduces exactly.
        let swap = Lut::parse(&cube(5, |c| [c[2], 1.0 - c[0], 0.5 * c[1] + 0.25])).unwrap();
        for rgb in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.3, 0.71, 0.05], [0.999, 0.5, 0.123]] {
            let a = id.apply(rgb);
            let b = swap.apply(rgb);
            let want = [rgb[2], 1.0 - rgb[0], 0.5 * rgb[1] + 0.25];
            for c in 0..3 {
                assert!((a[c] - rgb[c]).abs() < 1e-5, "{rgb:?} → {a:?}");
                assert!((b[c] - want[c]).abs() < 1e-5, "{rgb:?} → {b:?}");
            }
        }
        // Half strength is half way; outside 0..1 holds the edge.
        let half = swap.blend([0.2, 0.4, 0.6], 0.5);
        assert!((half[0] - 0.4).abs() < 1e-5);
        assert_eq!(swap.apply([-1.0, 2.0, 0.5]), swap.apply([0.0, 1.0, 0.5]));
    }

    #[test]
    fn bad_files_are_refused() {
        assert!(Lut::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err(), "too few colours");
        assert!(Lut::parse("LUT_1D_SIZE 2\n0 0 0\n1 1 1\n").is_err(), "1D");
        assert!(Lut::parse("0 0 0\n").is_err(), "no size");
        assert!(Lut::parse(&cube(2, |c| c).replace("0.000000\t0.000000\t0.000000", "0 x 0")).is_err(), "not numbers");
        let domain = cube(2, |c| c).replace("LUT_3D_SIZE 2", "LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 2 2 2");
        let lut = Lut::parse(&domain).unwrap();
        assert!((lut.apply([1.0, 1.0, 1.0])[0] - 0.5).abs() < 1e-6, "the domain scales the input");
    }
}
