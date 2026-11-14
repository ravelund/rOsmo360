//! Pictures along each clip for the timeline's cut blocks: the stitched view
//! straight ahead at the proxy's keyframes. Made on a thread when the clip
//! loads and sent to the app one at a time, so the blocks fill in as they
//! come. Clips without a proxy get none: their 3840² keyframes would cost
//! seconds each.

use super::{Media, Message};
use crate::cache;
use crate::ffmpeg;
use crate::stab::Mode;
use crate::stitch;
use crate::view::{Projection, View};
use iced::futures::{SinkExt, Stream, StreamExt};
use iced::widget::image::Handle;

/// Rendered size; drawn at about half that, so they stay sharp on HiDPI.
pub const SIZE: (usize, usize) = (128, 72);
/// Proxy lens frames decoded at this many pixels square.
const LENS: usize = 256;
/// At most about this many per clip; a long clip skips keyframes.
const MOST: f64 = 200.0;

pub fn stream(media: Media) -> impl Stream<Item = Message> {
    iced::stream::channel(8, async move |mut output: iced::futures::channel::mpsc::Sender<Message>| {
        let (tx, mut rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let input = media.input.clone();
            if let Err(e) = make(&media, |t, handle| tx.unbounded_send(Message::Thumb(input.clone(), t, handle)).is_ok()) {
                eprintln!("no thumbnails for {}: {e:#}", input.display());
            }
        });
        while let Some(m) = rx.next().await {
            if output.send(m).await.is_err() {
                break;
            }
        }
    })
}

pub fn make(media: &Media, mut send: impl FnMut(f64, Handle) -> bool) -> anyhow::Result<()> {
    if media.proxy.is_none() && media.flat.is_none() {
        return Ok(());
    }
    let (w, h) = SIZE;
    if let Some(thumbs) = cache::thumbs(&media.input, SIZE) {
        for (t, rgba) in thumbs {
            if !send(t, Handle::from_rgba(w as u32, h as u32, rgba)) {
                break;
            }
        }
        return Ok(());
    }
    let every = media.duration / MOST;
    let mut last = f64::NEG_INFINITY;
    let (mut made, mut whole) = (Vec::new(), true);
    // An ordinary video's own keyframes, whole as it plays.
    if let Some(flat) = media.flat {
        let (fw, fh) = flat.size();
        let small = (((fw as f64 * 2.0 * h as f64 / fh as f64) as usize).max(2) / 2 * 2, 2 * h);
        let camera = View::home(Projection::Perspective).camera();
        ffmpeg::key_frames(&media.input, flat.track, small, flat.out(None, None), |t, frame| {
            if t - last < every {
                return true;
            }
            last = t;
            let rgba: Vec<u8> = stitch::flat(&frame, SIZE, &camera).rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
            made.push((t, rgba.clone()));
            whole = send(t, Handle::from_rgba(w as u32, h as u32, rgba));
            whole
        })?;
        if whole && !made.is_empty() {
            cache::store_thumbs(&media.input, SIZE, &made);
        }
        return Ok(());
    }
    let Some(proxy) = &media.proxy else { return Ok(()) };
    let mut stab = media.stab.clone();
    stab.set_mode(Mode::Horizon);
    let camera = View { fov: 100.0, ..View::home(Projection::Perspective) }.camera();
    let ray = |x: f64, y: f64| stitch::apply(camera.user, camera.ray(w as f64 / h as f64, [x / w as f64 * 2.0 - 1.0, y / h as f64 * 2.0 - 1.0]));
    ffmpeg::key_frames(proxy, 0, (LENS * media.rig.tracks.len(), LENS), ffmpeg::Out::default(), |t, frame| {
        if t - last < every {
            return true;
        }
        last = t;
        let views = if stab.has_data() { stab.rig_from_view(t * 1000.0, media.readout_ms, 1) } else { vec![stitch::IDENTITY] };
        let frames = ffmpeg::split_proxy(&frame, &media.rig.tracks);
        let picture = stitch::render(&media.rig.lenses, &frames, SIZE, ray, None, &views, &stitch::Seam::default());
        let rgba: Vec<u8> = picture.rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        made.push((t, rgba.clone()));
        whole = send(t, Handle::from_rgba(w as u32, h as u32, rgba));
        whole
    })?;
    // Only a whole set: one cut short (the clip closed) would stay short.
    if whole && !made.is_empty() {
        cache::store_thumbs(&media.input, SIZE, &made);
    }
    Ok(())
}
