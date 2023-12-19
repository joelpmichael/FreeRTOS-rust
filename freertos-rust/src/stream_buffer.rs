use crate::base::*;
use crate::isr::*;
use crate::shim::*;
use crate::units::*;
use crate::CurrentTask;
use crate::Task;

unsafe impl Send for StreamBuffer {}
unsafe impl Sync for StreamBuffer {}

/// A streaming FIFO byte (u8) buffer with a finite size: bytes go in, bytes come out
/// Optimised for single-reader/single-writer scenarios
/// For more information, see https://www.freertos.org/RTOS-stream-buffer-example.html
///
/// !!! NOTE !!!
/// Stream buffers use Task Notifications internally. If a Task Notification is sent
/// to a task waiting on a stream buffer read or write, the task will resume causing
/// less data than expected to be transferred.
///
/// !!! WARNING !!!
/// Stream buffers assume that only a single task or ISR is writing, and a single task
/// is reading, the stream at a time.
#[derive(Debug)]
pub struct StreamBuffer {
    stream_buffer: FreeRtosStreamBufferHandle,
    max_size: usize,
    trigger_size: usize,
    tx_task: Option<FreeRtosTaskHandle>,
    rx_task: Option<FreeRtosTaskHandle>,
}

impl StreamBuffer {
    /// max_size: bytes (u8 * 1)
    /// maximum size of the stream buffer in bytes
    /// trigger_size: bytes (u8 * 1)
    /// minimum number of bytes in the buffer required before waking the receiving task
    pub fn new(max_size: usize, trigger_size: usize) -> Result<StreamBuffer, FreeRtosError> {
        if trigger_size > max_size {
            return Err(FreeRtosError::InvalidQueueSize);
        }
        let handle = unsafe {
            freertos_rs_stream_buffer_create(
                max_size as FreeRtosSizeT,
                trigger_size as FreeRtosSizeT,
            )
        };

        if handle == 0 as *const _ {
            return Err(FreeRtosError::OutOfMemory);
        }

        Ok(StreamBuffer {
            stream_buffer: handle,
            max_size,
            trigger_size,
            tx_task: None,
            rx_task: None,
        })
    }

    /// # Safety
    ///
    /// `handle` must be a valid FreeRTOS regular stream_buffer handle (not semaphore or mutex).
    ///
    /// task handles must be Some() if a stream TX or RX is in progress,
    /// otherwise FreeRTOS configASSERT() will be called if another TX or RX starts
    #[inline]
    pub unsafe fn from_raw_handle(
        handle: FreeRtosStreamBufferHandle,
        max_size: usize,
        trigger_size: usize,
        tx_task: Option<FreeRtosTaskHandle>,
        rx_task: Option<FreeRtosTaskHandle>,
    ) -> Self {
        Self {
            stream_buffer: handle,
            max_size,
            trigger_size,
            tx_task,
            rx_task,
        }
    }
    #[inline]
    pub fn raw_handle(
        &self,
    ) -> (
        FreeRtosStreamBufferHandle,
        usize,
        usize,
        Option<FreeRtosTaskHandle>,
        Option<FreeRtosTaskHandle>,
    ) {
        (
            self.stream_buffer,
            self.max_size,
            self.trigger_size,
            self.tx_task,
            self.rx_task,
        )
    }

    /// Send bytes to the end of the stream_buffer. Wait up to max_wait for all bytes to be sent.
    /// If bytes to be sent is more than the buffer size, only buffer size number of bytes will be sent.
    ///
    /// Ensures there is only a single task sending at a time. If multiple tasks are sending at once,
    /// then the time waiting for the currently-sending task is subtracted from the buffer sending
    /// timeout.
    ///
    /// Returns actual number of bytes sent to the stream, which may be fewer than requested
    pub fn send<D: DurationTicks>(
        &mut self,
        bytes: &[u8],
        max_wait: D,
    ) -> Result<usize, FreeRtosError> {
        let mut wait_remain = max_wait.to_ticks();
        while self.tx_task.is_some() && wait_remain > 0 {
            CurrentTask::delay(Duration::eps());
            wait_remain -= Duration::eps().to_ticks();
        }
        if wait_remain == 0 {
            return Err(FreeRtosError::QueueSendTimeout);
        }
        self.tx_task = match Task::current() {
            Ok(t) => Some(t.raw_handle()),
            Err(e) => {
                return Err(e);
            }
        };
        let result = unsafe {
            match freertos_rs_stream_buffer_send(
                self.stream_buffer,
                bytes as *const _ as FreeRtosVoidPtr,
                bytes.len() as FreeRtosSizeT,
                wait_remain,
            ) {
                0 => Err(FreeRtosError::QueueFull),
                sent => Ok(sent),
            }
        };
        self.tx_task = None;
        result
    }
    /// Send bytes to the stream buffer, until all bytes are sent
    /// or an error is returned
    ///
    /// !!! WARNING !!!
    /// send_all uses send() with max_wait set to Duration::infinite()
    /// which will live-lock the sending task until receive() is called
    /// from another task if the stream buffer fills up
    pub fn send_all(&mut self, bytes: &[u8]) -> Result<(), FreeRtosError> {
        let len = bytes.len();
        let mut remain = len;
        while remain > 0 {
            remain -= self.send(&bytes[len - remain..], Duration::infinite())?;
        }
        Ok(())
    }

    /// Send bytes to the end of the stream_buffer, from an interrupt.
    /// If bytes to be sent is more than the buffer size, only buffer size number of bytes will be sent.
    ///
    /// Will only write as many bytes to the buffer as there is currently free.
    ///
    /// Returns actual number of bytes sent to the stream, which may be fewer than requested
    pub fn send_from_isr(
        &self,
        context: &mut InterruptContext,
        bytes: &[u8],
    ) -> Result<usize, FreeRtosError> {
        unsafe {
            match freertos_rs_stream_buffer_send_isr(
                self.stream_buffer,
                bytes as *const _ as FreeRtosVoidPtr,
                bytes.len() as FreeRtosSizeT,
                context.get_task_field_mut(),
            ) {
                0 => Err(FreeRtosError::QueueFull),
                sent => Ok(sent),
            }
        }
    }

    /// Receive bytes from the stream_buffer. Wait up to max_wait for at least the stream's trigger_size bytes.
    /// If the stream has fewer than trigger_size bytes, but more than 0, then the bytes will be returned after
    /// waiting for max_wait. If bytes to be received is more than the buffer size, only buffer size number of
    /// bytes will be received.
    ///
    /// Ensures there is only a single task receiving at a time. If multiple tasks are receiving at once,
    /// then the time waiting for the currently-receiving task is subtracted from the buffer receiving
    /// timeout.
    ///
    /// Returns actual number of bytes received from the stream, which may be fewer than requested
    pub fn receive<D: DurationTicks>(
        &mut self,
        bytes: &mut [u8],
        max_wait: D,
    ) -> Result<usize, FreeRtosError> {
        let mut wait_remain = max_wait.to_ticks();
        while self.rx_task.is_some() && wait_remain > 0 {
            CurrentTask::delay(Duration::eps());
            wait_remain -= Duration::eps().to_ticks();
        }
        if wait_remain == 0 {
            return Err(FreeRtosError::QueueSendTimeout);
        }
        self.rx_task = match Task::current() {
            Ok(t) => Some(t.raw_handle()),
            Err(e) => {
                return Err(e);
            }
        };

        let result = unsafe {
            match freertos_rs_stream_buffer_receive(
                self.stream_buffer,
                bytes as *mut _ as FreeRtosMutVoidPtr,
                bytes.len(),
                wait_remain,
            ) {
                0 => Err(FreeRtosError::QueueReceiveTimeout),
                sent => Ok(sent),
            }
        };
        self.rx_task = None;
        result
    }
    /// Receive bytes from the stream buffer, until all bytes are received
    /// or an error is returned
    ///
    /// !!! WARNING !!!
    /// receive_all uses receive() with max_wait set to Duration::infinite()
    /// which will live-lock the receiving task until send() is called
    /// from another task if the stream buffer is empty
    pub fn receive_all(&mut self, bytes: &mut [u8]) -> Result<(), FreeRtosError> {
        let len = bytes.len();
        let mut remain = len;
        while remain > 0 {
            remain -= self.receive(&mut bytes[len - remain..], Duration::infinite())?;
        }
        Ok(())
    }

    /// Get the number of bytes in the stream_buffer.
    pub fn len(&self) -> usize {
        unsafe { freertos_rs_stream_buffer_bytes_waiting(self.stream_buffer) }
    }
}

impl Drop for StreamBuffer {
    fn drop(&mut self) {
        unsafe {
            freertos_rs_stream_buffer_delete(self.stream_buffer);
        }
    }
}
