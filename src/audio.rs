use std::cmp::Ordering;
use std::fmt::{Debug, Display, Formatter, Write};
use std::io;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::ops::Deref;
use std::sync::Arc;
use async_trait::async_trait;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{
    default_host, BuildStreamError, Device, FromSample, SizedSample, Stream, StreamConfig,
    StreamError
};
use crossbeam::atomic::AtomicCell;
use crossbeam::queue::ArrayQueue;
use songbird::input::{AudioStream, AudioStreamError, Compose, Input, RawAdapter};
use symphonia::core::io::MediaSource;

pub struct InputDeviceListItem {
    pub device: Device,
    pub name: String,
    pub is_default: bool
}

impl Debug for InputDeviceListItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        struct Device;

        impl Debug for Device {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                write!(f, "<Device>")
            }
        }

        f.debug_struct("InputDeviceListItem")
            .field("device", &Device)
            .field("name", &self.name)
            .field("is_default", &self.is_default)
            .finish()
    }
}

impl Display for InputDeviceListItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

pub fn list_input_devices() -> anyhow::Result<Vec<InputDeviceListItem>> {
    let host = default_host();
    let input_devices = host.input_devices()?;
    let Some(default_input_device) = host.default_input_device()
    else {
        return Ok(Vec::new());
    };
    let default_input_device_name = default_input_device.name()?;

    let size_hint = input_devices.size_hint();
    let mut devices = Vec::with_capacity(size_hint.1.unwrap_or(size_hint.0));
    for input_device in input_devices {
        let name = input_device.name()?;
        let is_default = name == default_input_device_name;
        devices.push(InputDeviceListItem {
            device: input_device,
            name,
            is_default
        });
    }

    Ok(devices)
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SampleRate(u32);

impl SampleRate {
    pub fn new(value: u32) -> Self {
        Self(value)
    }
}

impl From<SampleRate> for cpal::SampleRate {
    fn from(value: SampleRate) -> Self {
        cpal::SampleRate(value.0)
    }
}

impl From<cpal::SampleRate> for SampleRate {
    fn from(value: cpal::SampleRate) -> Self {
        SampleRate(value.0)
    }
}

impl Display for SampleRate {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.0 % 1000 {
            0 => write!(f, "{:>2} kHz", self.0 / 1000),
            _ => write!(f, "{} Hz", self.0)
        }
    }
}

impl PartialEq<cpal::SampleRate> for SampleRate {
    fn eq(&self, other: &cpal::SampleRate) -> bool {
        self.0.eq(&other.0)
    }
}

impl PartialOrd<cpal::SampleRate> for SampleRate {
    fn partial_cmp(&self, other: &cpal::SampleRate) -> Option<Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

#[derive(Clone)]
pub struct CpalMediaSource {
    data: Arc<ArrayQueue<u8>>,
    error: Arc<AtomicCell<Option<StreamError>>>,
    channel_count: u32
}

impl CpalMediaSource {
    const DATA_QUEUE_SIZE: usize = 8 * 1024 * 1024;

    pub fn from_device<T>(
        device: &Device,
        stream_config: &StreamConfig
    ) -> Result<(Self, Stream), BuildStreamError>
    where
        T: SizedSample,
        f32: FromSample<T>
    {
        let data_consumer = Arc::new(ArrayQueue::new(Self::DATA_QUEUE_SIZE));
        let data_producer = data_consumer.clone();
        let error = Arc::new(AtomicCell::new(None));
        let callback_error = error.clone();

        let input_stream = device.build_input_stream(
            stream_config,
            move |data: &[T], _| {
                for date in data.iter() {
                    // songbird needs data to be encoded directly as opus, raw i16 or raw f32
                    let sample: f32 = date.to_sample();
                    for byte in sample.to_ne_bytes() {
                        data_producer.force_push(byte);
                    }
                }
            },
            move |stream_error| callback_error.store(Some(stream_error)),
            None
        )?;

        Ok((
            CpalMediaSource {
                data: data_consumer,
                error,
                channel_count: stream_config.channels as u32
            },
            input_stream
        ))
    }

    pub fn into_lazy_input(self) -> Input {
        Input::Lazy(Box::new(self.clone()))
    }
}

impl Read for CpalMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.error.swap(None) {
            Some(StreamError::DeviceNotAvailable) => {
                return Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "source got disconnected"
                ))
            }
            Some(err) => return Err(io::Error::new(ErrorKind::Other, err.to_string())),
            None => ()
        }

        for (i, byte_ref) in buf.iter_mut().enumerate() {
            match (self.data.pop(), i) {
                (None, 0) => return Err(io::ErrorKind::WouldBlock.into()),
                (None, _) => return Ok(i),
                (Some(byte), _) => *byte_ref = byte
            }
        }

        Ok(buf.len())
    }
}

impl Seek for CpalMediaSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        // The source does not provide seekability but the trait `MediaSource` requires
        // this
        unimplemented!()
    }
}

impl MediaSource for CpalMediaSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.data.len() as u64)
    }
}

#[async_trait]
impl Compose for CpalMediaSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Ok(AudioStream {
            input: Box::new(self.clone()),
            hint: None,
        })
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        // since `should_create_async` returns `false` this shouldn't get called
        unimplemented!()
    }

    fn should_create_async(&self) -> bool {
        false
    }
}
