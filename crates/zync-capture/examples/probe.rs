//! Prints what the platform capture backend is actually delivering.
//!
//! Existence proof for a backend, not a test: it opens the real source, shows
//! how it describes itself and what resolution it reports, then samples the
//! centre pixel once a second so a stalled stream, a black stream and a live
//! one are all distinguishable by eye.
//!
//! `cargo run -p zync-capture --example probe`

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::time::Duration;

    const SECONDS: u32 = 5;

    let source = zync_capture::open()?;
    let (width, height) = source.source_size();

    println!("source:      {}", source.describe());
    println!("source size: {width}x{height}");

    (1..=SECONDS).try_for_each(|second| {
        std::thread::sleep(Duration::from_secs(1));

        let frames = source.frames_captured();
        match source.next_frame() {
            Ok(frame) => {
                let (frame_width, frame_height) = frame.size();
                let centre = frame.rgb(frame_width / 2, frame_height / 2);
                println!(
                    "{second}s: frames={frames} frame={frame_width}x{frame_height} centre={centre:?}"
                );
            }
            Err(e) => println!("{second}s: frames={frames} error={e:#}"),
        }

        anyhow::Ok(())
    })
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("The capture probe is only wired up for macOS.");
}
