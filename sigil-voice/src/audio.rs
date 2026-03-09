use audiopus::{coder::Encoder, coder::Decoder, Application, Channels, SampleRate};

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

pub struct AudioDecoder {
    decoder: Decoder,
}

impl AudioDecoder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Discord audio is always 48kHz Stereo
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)?;
        Ok(Self { decoder })
    }

    /// Decode an Opus packet into a 20ms PCM frame (1920 samples).
    pub fn decode_opus(
        &mut self,
        opus: &[u8],
        out_pcm: &mut [i16],
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        use std::convert::TryFrom;
        let opus_packet = audiopus::packet::Packet::try_from(opus)?;
        let pcm_signals = audiopus::MutSignals::try_from(out_pcm)?;
        
        let len = self.decoder.decode(Some(opus_packet), pcm_signals, false)?;
        Ok(len)
    }
}
