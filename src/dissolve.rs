//! Crossfades between cuts in an export: frames written one cut after
//! another, the last frames of a cut held back and blended into the first
//! of the next.
//!
//! Each cut is rendered on its own (its own decoder and framing), so the two
//! cuts of a crossfade are never rendered side by side. The outgoing cut's
//! tail goes to a file instead, and is read back frame by frame as the
//! incoming cut's head arrives: at 2160p a 2 s crossfade would hold 1.5 GB
//! in memory.

use crate::ffmpeg::FrameSink;
use crate::stitch::Frame;
use crate::transition::Transition;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

pub struct Dissolve<'a> {
    out: &'a mut dyn FrameSink,
    /// The cut being written: frames so far, how many it has, how many of
    /// its first blend with the last cut's held tail, and how many of its
    /// last are held for the next.
    n: usize,
    frames: usize,
    head: usize,
    tail: usize,
    /// How its head comes in over the held tail.
    transition: Transition,
    held: Option<BufReader<File>>,
    holding: Option<BufWriter<File>>,
    /// The two files, swapped at each cut: one read, one written.
    files: [PathBuf; 2],
}

impl<'a> Dissolve<'a> {
    /// Its files are named unguessably in the temp folder (by the pid
    /// alone, another user could plant a link there for it to write through).
    pub fn new(out: &'a mut dyn FrameSink) -> Self {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        let key = hasher.finish();
        let file = |k| std::env::temp_dir().join(format!("rosmo360-dissolve-{}-{key:016x}-{k}.rgb", std::process::id()));
        Dissolve { out, n: 0, frames: 0, head: 0, tail: 0, transition: Transition::default(), held: None, holding: None, files: [file(0), file(1)] }
    }

    /// Starts the next cut of `frames` frames, whose first `head` cross the
    /// last cut's held tail by `transition` and whose last `tail` cross the
    /// next cut.
    pub fn cut(&mut self, frames: usize, head: usize, tail: usize, transition: Transition) -> Result<()> {
        self.transition = transition;
        self.held = None;
        if let Some(mut w) = self.holding.take() {
            w.flush()?;
            drop(w);
            self.files.swap(0, 1);
            self.held = Some(BufReader::new(File::open(&self.files[0])?));
        }
        if tail > 0 {
            // Made new, never opened through whatever is at the name: the
            // last cut's file is removed first.
            let _ = std::fs::remove_file(&self.files[1]);
            let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&self.files[1]);
            self.holding = Some(BufWriter::new(file.with_context(|| format!("creating {}", self.files[1].display()))?));
        }
        (self.n, self.frames, self.head, self.tail) = (0, frames, head.min(frames), tail.min(frames));
        Ok(())
    }
}

impl FrameSink for Dissolve<'_> {
    fn write(&mut self, frame: &Frame) -> Result<()> {
        let n = self.n;
        self.n += 1;
        if n + self.tail >= self.frames
            && let Some(w) = &mut self.holding
        {
            return Ok(w.write_all(&frame.rgb)?);
        }
        if n < self.head
            && let Some(r) = &mut self.held
        {
            let mut before = vec![0; frame.rgb.len()];
            r.read_exact(&mut before).context("reading the crossfade's held frames")?;
            // By frame centres: the first frame already a little in.
            let before = Frame { rgb: before, ..*frame };
            let rgb = self.transition.mix((n as f64 + 0.5) / self.head as f64, &before, frame);
            return self.out.write(&Frame { width: frame.width, height: frame.height, rgb, deep: frame.deep });
        }
        self.out.write(frame)
    }
}

impl Drop for Dissolve<'_> {
    fn drop(&mut self) {
        (self.held, self.holding) = (None, None);
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}
