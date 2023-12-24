use crate::base::*;
use crate::isr::*;
use crate::prelude::v1::*;
use crate::shim::*;
use crate::units::*;

unsafe impl<T: Sized + Copy> Send for MessageBuffer<T> {}
unsafe impl<T: Sized + Copy> Sync for MessageBuffer<T> {}

/// A streaming FIFO message <T> buffer with a finite size: messages go in, messages come out
/// Optimised for single-reader/single-writer scenarios
/// For more information, see https://www.freertos.org/RTOS-message-buffer-example.html
///
/// !!! NOTE !!!
/// Message buffers use Task Notifications internally. If a Task Notification is sent
/// to a task waiting on a message buffer read or write, the task will resume causing
/// a transfer error.
///
/// !!! WARNING !!!
/// Message buffers assume that only a single task or ISR is writing, and a single task
/// is reading, the buffer at a time.
#[derive(Debug)]
pub struct MessageBuffer<T: Sized + Copy> {
    message_buffer: FreeRtosMessageBufferHandle,
    item_type: PhantomData<T>,
    max_messages: usize,
    tx_task: Option<FreeRtosTaskHandle>,
    rx_task: Option<FreeRtosTaskHandle>,
}

impl<T: Sized + Copy> MessageBuffer<T> {
    /// fixed item size is the memory size of the message T
    /// plus 4 bytes to encode the length
    /// if using unsafe send_any() then mem::size_of::<A> <= mem::size_of::<T>
    const item_size: usize = mem::size_of::<T>();

    /// create new message buffer
    /// max_messages: number of <T> messages that can be stored in the buffer
    /// each message consumes mem::size_of::<T> + 4 bytes
    pub fn new(max_messages: usize) -> Result<MessageBuffer<T>, FreeRtosError> {
        let handle = unsafe {
            freertos_rs_message_buffer_create(max_messages * (item_size + 4) as FreeRtosSizeT)
        };

        if handle == 0 as *const _ {
            return Err(FreeRtosError::OutOfMemory);
        }

        Ok(MessageBuffer {
            message_buffer: handle,
            item_type: PhantomData,
            max_messages,
            tx_task: None,
            rx_task: None,
        })
    }

    /// # Safety
    ///
    /// `handle` must be a valid FreeRTOS regular queue handle (not semaphore or mutex).
    ///
    /// The item size of the queue must match the size of `T`.
    ///
    /// task handles must be Some() if a stream TX or RX is in progress,
    /// otherwise FreeRTOS configASSERT() will be called if another TX or RX starts
    #[inline]
    pub unsafe fn from_raw_handle(
        handle: FreeRtosMessageBufferHandle,
        max_messages: usize,
        tx_task: Option<FreeRtosTaskHandle>,
        rx_task: Option<FreeRtosTaskHandle>,
    ) -> Self {
        Self {
            message_buffer: handle,
            item_type: PhantomData,
            max_messages,
            tx_task,
            rx_task,
        }
    }
    #[inline]
    pub fn raw_handle(
        &self,
    ) -> (
        FreeRtosMessageBufferHandle,
        usize,
        Option<FreeRtosTaskHandle>,
        Option<FreeRtosTaskHandle>,
    ) {
        (
            self.message_buffer,
            self.max_messages,
            self.tx_task,
            self.rx_task,
        )
    }

    /// Send item to the end of the message buffer. Wait up to max_wait for item to be sent.
    /// It is (should be) impossible to send an item larger than the buffer size because new()
    /// correctly sizes the buffer for the item.
    ///
    /// Ensures there is only a single task sending at a time. If multiple tasks are sending at once,
    /// then the time waiting for the currently-sending task is subtracted from the buffer sending
    /// timeout.
    ///
    /// Returns actual number of bytes sent to the buffer, which will equal the size of the item + 4
    pub fn send<D: DurationTicks>(&self, item: T, max_wait: D) -> Result<usize, FreeRtosError> {
        // wait until any other tasks have finished sending
        // the time spent waiting here is subtracted from the requested wait time
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
            match freertos_rs_message_buffer_send(
                self.message_buffer,
                &item as *const _ as FreeRtosVoidPtr,
                item_size as FreeRtosSizeT,
                wait_remain,
            ) {
                // xMessageBufferSend() can also return 0 if the item is larger than the total buffer size
                // buffer size gets calculated from item size at compile time, so this condition won't occur
                0 => Err(FreeRtosError::QueueFull),
                sent => Ok(sent as usize),
            }
        };
        self.tx_task = None;
        result
    }

    /// Send bytes to the end of the message buffer, from an interrupt.
    ///
    /// If there is not enough space free in the message buffer, will return 0
    pub fn send_from_isr(
        &self,
        context: &mut InterruptContext,
        item: T,
    ) -> Result<usize, FreeRtosError> {
        unsafe {
            match freertos_rs_message_buffer_send_isr(
                self.message_buffer,
                &item as *const _ as FreeRtosVoidPtr,
                item_size as FreeRtosSizeT,
                context.get_task_field_mut(),
            ) {
                0 => Err(FreeRtosError::QueueFull),
                sent => Ok(sent),
            }
        }
    }

    /// Receive one item from the message buffer. Wait up to max_wait for an item to be available.
    ///
    /// Ensures there is only a single task receiving at a time. If multiple tasks are receiving at once,
    /// then the time waiting for the currently-receiving task is subtracted from the buffer receiving
    /// timeout.
    ///
    /// Returns actual number of bytes received from the message buffer, which is (should be) the same size as the type
    pub fn receive<D: DurationTicks>(&self, max_wait: D) -> Result<T, FreeRtosError> {
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

        unsafe {
            let mut buff = mem::zeroed::<T>();
            let r = freertos_rs_message_buffer_receive(
                self.message_buffer,
                &mut buff as *mut _ as FreeRtosMutVoidPtr,
                item_size as FreeRtosSizeT,
                max_wait.to_ticks(),
            );
            self.rx_task = None;

            if r == item_size {
                Ok(buff)
            } else if r == 0 {
                Err(FreeRtosError::QueueReceiveTimeout)
            } else {
                Err(FreeRtosError::InvalidQueueSize)
            }
        }
    }

    /// Get the number of messages in the queue.
    /// calculate the number of messages by using number of bytes available
    pub fn len(&self) -> u32 {
        let available = unsafe { freertos_rs_message_buffer_bytes_available(self.message_buffer) };
        let used = (self.max_messages * (item_size + 4)) - available;
        used / (item_size + 4)
    }
}

impl<T: Sized + Copy> Drop for MessageBuffer<T> {
    fn drop(&mut self) {
        unsafe {
            freertos_rs_message_buffer_delete(self.message_buffer);
        }
    }
}
