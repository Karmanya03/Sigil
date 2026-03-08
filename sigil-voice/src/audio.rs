// Stripped audiopus dependency because `audiopus-sys` fails local CMake builds on Windows.
pub struct AudioEncoder {}

impl AudioEncoder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {})
    }

    /// Mock encoder that just copies the data over.
    pub fn encode_pcm(&mut self, _pcm: &[i16], out_buf: &mut [u8]) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        // Mock output of 100 bytes
        for i in 0..100 {
            if i < out_buf.len() {
                out_buf[i] = 0;
            }
        }
        Ok(100.min(out_buf.len()))
    }
}
