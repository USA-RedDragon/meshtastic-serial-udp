use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serialport::{
    ClearBuffer, DataBits, FlowControl, Parity, Result as SpResult, SerialPort, StopBits,
};

/// A mock serial port backed by shared byte buffers for testing.
pub struct MockSerialPort {
    read_buf: Arc<Mutex<VecDeque<u8>>>,
    write_buf: Arc<Mutex<Vec<u8>>>,
    timeout: Duration,
}

impl MockSerialPort {
    pub fn new() -> Self {
        Self {
            read_buf: Arc::new(Mutex::new(VecDeque::new())),
            write_buf: Arc::new(Mutex::new(Vec::new())),
            timeout: Duration::from_millis(100),
        }
    }

    /// Push bytes into the read buffer (simulates data arriving on serial).
    pub fn inject_read_data(&self, data: &[u8]) {
        let mut buf = self.read_buf.lock().unwrap();
        buf.extend(data);
    }

    /// Take all bytes written to the mock serial port.
    pub fn take_written(&self) -> Vec<u8> {
        let mut buf = self.write_buf.lock().unwrap();
        std::mem::take(&mut *buf)
    }
}

impl io::Read for MockSerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut rb = self.read_buf.lock().unwrap();
        if rb.is_empty() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "mock: no data"));
        }
        let count = buf.len().min(rb.len());
        for b in buf.iter_mut().take(count) {
            *b = rb.pop_front().unwrap();
        }
        Ok(count)
    }
}

impl io::Write for MockSerialPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut wb = self.write_buf.lock().unwrap();
        wb.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SerialPort for MockSerialPort {
    fn name(&self) -> Option<String> {
        Some("mock".to_string())
    }

    fn baud_rate(&self) -> SpResult<u32> {
        Ok(115200)
    }

    fn data_bits(&self) -> SpResult<DataBits> {
        Ok(DataBits::Eight)
    }

    fn flow_control(&self) -> SpResult<FlowControl> {
        Ok(FlowControl::None)
    }

    fn parity(&self) -> SpResult<Parity> {
        Ok(Parity::None)
    }

    fn stop_bits(&self) -> SpResult<StopBits> {
        Ok(StopBits::One)
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn set_baud_rate(&mut self, _: u32) -> SpResult<()> {
        Ok(())
    }

    fn set_data_bits(&mut self, _: DataBits) -> SpResult<()> {
        Ok(())
    }

    fn set_flow_control(&mut self, _: FlowControl) -> SpResult<()> {
        Ok(())
    }

    fn set_parity(&mut self, _: Parity) -> SpResult<()> {
        Ok(())
    }

    fn set_stop_bits(&mut self, _: StopBits) -> SpResult<()> {
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) -> SpResult<()> {
        self.timeout = timeout;
        Ok(())
    }

    fn write_request_to_send(&mut self, _: bool) -> SpResult<()> {
        Ok(())
    }

    fn write_data_terminal_ready(&mut self, _: bool) -> SpResult<()> {
        Ok(())
    }

    fn read_clear_to_send(&mut self) -> SpResult<bool> {
        Ok(true)
    }

    fn read_data_set_ready(&mut self) -> SpResult<bool> {
        Ok(true)
    }

    fn read_ring_indicator(&mut self) -> SpResult<bool> {
        Ok(false)
    }

    fn read_carrier_detect(&mut self) -> SpResult<bool> {
        Ok(true)
    }

    fn bytes_to_read(&self) -> SpResult<u32> {
        let rb = self.read_buf.lock().unwrap();
        Ok(rb.len() as u32)
    }

    fn bytes_to_write(&self) -> SpResult<u32> {
        Ok(0)
    }

    fn clear(&self, _: ClearBuffer) -> SpResult<()> {
        Ok(())
    }

    fn try_clone(&self) -> SpResult<Box<dyn SerialPort>> {
        Ok(Box::new(MockSerialPort {
            read_buf: Arc::clone(&self.read_buf),
            write_buf: Arc::clone(&self.write_buf),
            timeout: self.timeout,
        }))
    }

    fn set_break(&self) -> SpResult<()> {
        Ok(())
    }

    fn clear_break(&self) -> SpResult<()> {
        Ok(())
    }
}
