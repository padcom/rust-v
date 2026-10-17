//! Plays the guest's virtio-snd output on the host's default audio device.

use std::sync::Arc;

use rv_machine::virtio::snd::AudioRing;

/// Keeps the output stream alive; dropping it stops playback.
pub struct Output {
    #[cfg(feature = "audio")]
    _stream: cpal::Stream,
}

#[cfg(feature = "audio")]
pub fn start(ring: Arc<AudioRing>) -> Result<Output, String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("unsupported output sample format {:?}", supported.sample_format()));
    }
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    ring.attach(config.sample_rate);
    let mut stereo = Vec::new();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| {
                // The ring is stereo; spread it over however many channels the device has.
                let frames = data.len() / channels;
                stereo.resize(2 * frames, 0.0);
                ring.pull(&mut stereo);
                for (out, lr) in data.chunks_exact_mut(channels).zip(stereo.as_chunks::<2>().0) {
                    for (c, o) in out.iter_mut().enumerate() {
                        *o = if channels == 1 {
                            (lr[0] + lr[1]) / 2.0
                        } else {
                            lr[c.min(1)] * (c < 2) as u8 as f32
                        };
                    }
                }
            },
            |e| eprintln!("rust-v: audio: {e}\r"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(Output { _stream: stream })
}

#[cfg(not(feature = "audio"))]
pub fn start(_ring: Arc<AudioRing>) -> Result<Output, String> {
    Err("built without audio support".into())
}
