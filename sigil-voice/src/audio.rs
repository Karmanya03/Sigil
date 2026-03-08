use audiopus::{
    coder::Encoder as OpusEncoder,
    Application, Bitrate, Channels, SampleRate,
};

pub struct AudioEncoder {
    encoder: OpusEncoder,
}

impl AudioEncoder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Discord voice is standard 48kHz, Stereo, 20ms frames
        let mut encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(128_000))?;
        Ok(Self { encoder })
    }

    /// Encodes a 20ms frame of raw PCM data (960 samples per channel, stereo 16-bit = 3840 bytes).
    /// Returns the Opus encoded length written to `out_buf`.
    pub fn encode_pcm(&mut self, pcm: &[i16], out_buf: &mut [u8]) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let size = self.encoder.encode(pcm, out_buf)?;
        Ok(size)
    }
}
