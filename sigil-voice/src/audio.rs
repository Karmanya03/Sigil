use audiopus::{coder::Encoder, Application, Channels, SampleRate};

pub struct AudioEncoder {
    encoder: Encoder,
}

impl AudioEncoder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)?;
        Ok(Self { encoder })
    }

    /// Encode 20ms of PCM data (1920 i16 samples for stereo 48kHz) into Opus.
    pub fn encode_pcm(
        &mut self,
        pcm: &[i16],
        out_buf: &mut [u8],
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let len = self.encoder.encode(pcm, out_buf)?;
        Ok(len)
    }
}
